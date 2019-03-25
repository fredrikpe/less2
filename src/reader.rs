use std::fs::File;
use std::io::{
    self, stdin, BufRead, Cursor, Initializer, Read, Seek, SeekFrom, Stdin,
};
use std::str;

use grep::matcher::Match;
use grep::regex::RegexMatcher;

use searcher;
use error::{Error, Result};
use standard::StandardSink;
use string_util;
use utf8_validation;
use util;

const DEFAULT_BUF_SIZE: usize = 4096;

pub trait Search {
    fn search(&mut self, matches: &mut Vec<(u64, Match)>, pattern: &str);
}

pub trait FileSwitcher {
    fn search(&mut self, matches: &mut Vec<(u64, Match)>, pattern: &str);
}

pub struct BiBufReader<R> {
    inner: R,
    pos: usize,
    cap: usize,
}

impl<R: Read + Seek> BiBufReader<R> {
    pub fn new(inner: R) -> BiBufReader<R> {
        BiBufReader {
            inner,
            pos: 0,
            cap: 0,
        }
    }

    pub fn jump_offset(&mut self, offset: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Start(offset))?;
        Ok(())
    }

    pub fn jump_percentage(&mut self, percent: u64) -> Result<()> {
        let _ = self.seek_percent(percent)?;
        self.up_n_lines(1)
    }

    pub fn jump_end(&mut self) -> Result<()> {
        let _ = self.seek_percent(100)?;
        self.up_n_lines(util::screen_height() - 1)
    }

    pub fn up_n_lines(&mut self, n: usize) -> Result<()> {
        let buf = self.make_buf_up()?;

        unsafe {
            let size = buf.len();

            let (screen_width, _) = util::screen_width_height();

            let offset = size as i64
                - string_util::nth_last_newline_wrapped(
                    n + 1,
                    str::from_utf8_unchecked(&buf[..]),
                    screen_width as usize,
                ) as i64;
            self.inner.seek(SeekFrom::Current(-(offset as i64)))?;
        }

        Ok(())
    }

    pub fn down_n_lines(&mut self, n: usize) -> Result<()> {
        eprint!("down, ");
        self.pcur_pos()?;
        let (buf, size) = self.make_buf_down()?;

        let (screen_width, _) = util::screen_width_height();

        unsafe {
            let newline_offset = string_util::nth_newline_wrapped(
                n,
                str::from_utf8_unchecked(&buf[..size]),
                screen_width as usize,
            );
            self.inner.seek(SeekFrom::Current(newline_offset as i64))?;
        }

        Ok(())
    }

    pub fn page(&mut self) -> Result<(u64, Vec<u8>)> {
        let size = self.page_size();

        let mut page_buf = Vec::with_capacity(size);
        page_buf.resize(size, 0);

        let bytes_read = match self.inner.read(&mut page_buf[..]) {
            Err(e) => {
                eprintln!("errorrrr {}", e);
                0
            }
            Ok(s) => s as i64,
        };

        let offset = self.inner.seek(SeekFrom::Current(-bytes_read))?;

        Ok((offset, page_buf[..bytes_read as usize].to_vec()))
    }

    pub fn current_offset(&mut self) -> u64 {
        return match self.inner.seek(SeekFrom::Current(0)) {
            Err(_) => panic!("Fatal error. Couldn't get current offset!"),
            Ok(pos) => pos,
        }
    }

    fn make_buf_up(&mut self) -> Result<Vec<u8>> {
        let cur_pos = self.inner.seek(SeekFrom::Current(0))?;

        let size = std::cmp::min(
            self.search_buf_size(),
            self.inner.seek(SeekFrom::Current(0))? as usize,
        );

        let mut buf = vec![0; size];
        let _ = self.inner.seek(SeekFrom::Current(-(size as i64)))?;

        let bytes_read = self.inner.read(&mut buf[..size])?;

        self.inner.seek(SeekFrom::Current((size - bytes_read) as i64))?;

        assert_eq!(cur_pos, self.inner.seek(SeekFrom::Current(0))?);
        Ok(string_util::make_valid(buf))
    }

    fn make_buf_down(&mut self) -> Result<(Vec<u8>, usize)> {
        let cur_pos = self.inner.seek(SeekFrom::Current(0))?;

        let size = self.search_buf_size();
        let mut buf = vec![0; size as usize];

        let bytes_read = self.inner.read(&mut buf)?;

        if bytes_read == 0 {
            return Err(Error::GenericError);
        }

        self.inner.seek(SeekFrom::Current(-(bytes_read as i64)))?;

        assert_eq!(cur_pos, self.inner.seek(SeekFrom::Current(0))?);
        Ok((buf, bytes_read))
    }

    fn search_buf_size(&self) -> usize {
        self.page_size()
    }

    fn page_size(&self) -> usize {
        let (screen_width, screen_height) = util::screen_width_height();
        screen_width as usize * screen_height as usize * 4 // 4 is max utf8 char size
    }

    fn pcur_pos(&mut self) -> Result<()> {
        if let Ok(cur_pos) = self.inner.seek(SeekFrom::Current(0)) {
            eprintln!("cur_pos: {}", cur_pos);
        }
        Ok(())
    }

    fn seek_percent(&mut self, percent: u64) -> Result<u64> {
        let size = self.inner.seek(SeekFrom::End(0))?;
        let offset = std::cmp::min(size, (size * percent / 100) as u64);

        match self.inner.seek(SeekFrom::Start(offset)) {
            Err(_) => panic!("Fatal error in seek_percent!"),
            Ok(pos) => Ok(pos),
        }
    }
}
        
impl<R: Search+Seek> Search for BiBufReader<R> {
    fn search(&mut self, matches: &mut Vec<(u64, Match)>, pattern: &str) {
        // Searching will seek from current position to end, so first have
        // to remeber the current position, go to the beginning (we want to
        // search the entire file), then go back to the original position.
        let cur_pos = self.inner.seek(SeekFrom::Current(0)).unwrap();
        self.inner.seek(SeekFrom::Start(0));
        self.inner.search(matches, pattern);
        self.inner.seek(SeekFrom::Start(cur_pos));
    }
}

#[derive(Debug)]
pub struct StdinCursor {
    cursor: Cursor<Vec<u8>>,
    pub file: File,
}

impl StdinCursor {
    pub fn new(mut stdin_file: File) -> StdinCursor {
        let mut cursor: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        // TODO: Currently reads the whole thing at the beginning. Could instead read while
        // scrolling down.
        stdin_file.read_to_end(cursor.get_mut()).unwrap();
        StdinCursor {
            cursor: cursor,
            file: stdin_file,
        }
    }
}

impl Seek for StdinCursor {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(pos)
    }
}

impl Read for StdinCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.cursor.read(buf)
    }
}

#[derive(Debug)]
pub enum InputReader {
    Stdin(StdinCursor),
    Files(Vec<File>),
}

impl Read for InputReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        return match self {
            InputReader::Stdin(stdin_cursor) => stdin_cursor.read(buf),
            InputReader::Files(files) => (&files[0]).read(buf),
        };
    }
}

impl Seek for InputReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        return match self {
            InputReader::Stdin(stdin_cursor) => stdin_cursor.seek(pos),
            InputReader::Files(files) => (&files[0]).seek(pos),
        };
    }
}

impl Search for InputReader {
    fn search(&mut self, matches: &mut Vec<(u64, Match)>, pattern: &str) {
        let matcher = match RegexMatcher::new(&pattern[..]) {
            Err(_) => return,
            Ok(m) => m,
        };

        let mut sink = StandardSink {
            matcher: matcher,
            matches: matches,
            match_count: 0,
        };

        match self {
            InputReader::Stdin(stdin_cursor) => {
                match searcher::search_reader(&mut sink, stdin_cursor) {
                    Err(_) => (),
                    Ok(_) => (),
                }
            }
            InputReader::Files(files) => {
                match searcher::search_file(&mut sink, &files[0]) {
                    Err(_) => (),
                    Ok(_) => (),
                }
            }
        }
    }
}

/// This reader ensures that the position is always at a valid utf-8 code point, i.e., when seeking
/// to a pos it will stop at a nearby valid point if the original is in the middle of a character.
///
/// The start position is assumed valid.
pub struct ValidReader<R> {
    inner: R,
}

impl<R: Read> ValidReader<R> {
    pub fn new(reader: R) -> ValidReader<R> {
        ValidReader { inner: reader }
    }
}

impl<R: Read> Read for ValidReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Seek> Seek for ValidReader<R> {

    /// When seeking to a posi
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut pos = self.inner.seek(pos)?;

        // We assume start is always valid
        if pos == 0 {
            return Ok(pos);
        }

        // Me thinks 8 is enough?
        let mut buf: [u8; 8] = [0; 8];
        let r = self.read(&mut buf)?;

        pos += match utf8_validation::first_valid_pos(&buf[..r]) {
            Some(offset) => offset as u64,
            None => {
                return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "No valid position found.",
                        ))
            }
        };

        self.inner.seek(SeekFrom::Start(pos))
    }
}

impl<S: Search> Search for ValidReader<S> {
    fn search(&mut self, matches: &mut Vec<(u64, Match)>, pattern: &str) {
        self.inner.search(matches, pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::BufReader;

    fn thai_file() -> File {
        File::open("tests/resources/thai.txt").unwrap()
    }

    #[test]
    fn test_seek_to_end() {
        let mut reader = ValidReader::new(thai_file());
        assert_eq!(
            reader.seek(SeekFrom::End(0)).unwrap(),
            reader.seek(SeekFrom::Current(0)).unwrap()
        );
    }

    #[test]
    fn test_seek_0_returns_0() {
        let mut reader = ValidReader::new(thai_file());
        assert_eq!(reader.seek(SeekFrom::Start(0)).unwrap(), 0);
    }

    #[test]
    fn test_valid_from_1() {
        let mut reader = ValidReader::new(thai_file());

        // Seek to invalid pos
        let pos = reader.seek(SeekFrom::Start(1)).unwrap();
        assert_eq!(pos, 3);

        let mut buf: [u8; 1000] = [0; 1000];
        let b = reader.read(&mut buf[..]).unwrap();
        assert!(std::str::from_utf8(&buf[..b]).is_ok());
    }

    #[test]
    fn test_err_at_end() {
        let mut reader = ValidReader::new(thai_file());
        let mut buf: [u8; 10] = [0; 10];
        let b = reader.read(&mut buf[..]).unwrap();
        eprintln!("b {}", b);
        assert!(std::str::from_utf8(&buf[..b]).is_err());
    }

    #[test]
    fn test_bufreader_fails_start() {
        let mut reader = BufReader::new(thai_file());

        // Seek to invalid position
        reader.seek(SeekFrom::Start(1)).unwrap();

        let mut buf: [u8; 1000] = [0; 1000];
        let b = reader.read(&mut buf[..]).unwrap();
        assert!(std::str::from_utf8(&buf[..b]).is_err());
    }

    #[test]
    fn test_bufreader_fails_end() {
        let mut reader = BufReader::new(thai_file());

        // 10 bytes in happens to be in the middle of a grapheme
        let mut buf: [u8; 10] = [0; 10];
        let b = reader.read(&mut buf[..]).unwrap();
        assert!(std::str::from_utf8(&buf[..b]).is_err());
    }
}
