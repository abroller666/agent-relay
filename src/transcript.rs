//! Bounded, streaming JSONL reading.
//!
//! A transcript is appended to while its agent runs, so its last line may be
//! half written. Such a line (no newline yet) ends the stream and is
//! reported by `Records::partial_tail`, not as corruption. A finished line
//! that is not JSON is `TranscriptCorrupt`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::ReadLimits;
use crate::error::HandoffError;

/// One finished line and where it is in the file.
#[derive(Debug)]
pub struct Record {
    pub value: Value,
    pub offset: u64,
    pub len: usize,
}

pub struct Records {
    reader: BufReader<File>,
    path: PathBuf,
    max_line: usize,
    offset: u64,
    partial: bool,
    done: bool,
    buf: Vec<u8>,
}

/// The records of `path`, refusing files and lines over the limits.
pub fn read_records(path: &Path, limits: &ReadLimits) -> Result<Records, HandoffError> {
    let file = open(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size > limits.max_file_bytes {
        return Err(HandoffError::ReadLimitExceeded(format!(
            "{}: {}バイト（上限{}）",
            name(path),
            size,
            limits.max_file_bytes
        )));
    }
    Ok(Records {
        reader: BufReader::with_capacity(64 * 1024, file),
        path: path.to_path_buf(),
        max_line: limits.max_line_bytes,
        offset: 0,
        partial: false,
        done: false,
        buf: Vec::new(),
    })
}

impl Records {
    /// Whether the stream ended at a line still being written.
    pub fn partial_tail(&self) -> bool {
        self.partial
    }

    fn next_line(&mut self) -> Result<Option<Record>, HandoffError> {
        loop {
            self.buf.clear();
            let start = self.offset;
            let limit = self.max_line as u64 + 1;
            let n = (&mut self.reader)
                .take(limit)
                .read_until(b'\n', &mut self.buf)
                .map_err(|e| io_error(&self.path, e))?;
            if n == 0 {
                return Ok(None);
            }
            self.offset += n as u64;
            if self.buf.last() != Some(&b'\n') {
                if n as u64 >= limit {
                    return Err(HandoffError::ReadLimitExceeded(format!(
                        "{}: 1行が{}バイトを超えています",
                        name(&self.path),
                        self.max_line
                    )));
                }
                self.partial = true;
                return Ok(None);
            }
            let line = &self.buf[..n - 1];
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value = serde_json::from_slice(line).map_err(|e| {
                HandoffError::TranscriptCorrupt(format!(
                    "{}: {}バイト目の行: {e}",
                    name(&self.path),
                    start
                ))
            })?;
            return Ok(Some(Record {
                value,
                offset: start,
                len: n,
            }));
        }
    }
}

impl Iterator for Records {
    type Item = Result<Record, HandoffError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.next_line().transpose();
        if !matches!(item, Some(Ok(_))) {
            self.done = true;
        }
        item
    }
}

/// The record at `offset` (as returned by `Records`), read again.
pub fn read_at(path: &Path, offset: u64, len: usize) -> Result<Value, HandoffError> {
    let mut file = open(path)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| io_error(path, e))?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).map_err(|e| io_error(path, e))?;
    serde_json::from_slice(buf.trim_ascii_end()).map_err(|_| {
        HandoffError::SourceChanged(format!("{}が読み取り中に書き換えられました", name(path)))
    })
}

fn open(path: &Path) -> Result<File, HandoffError> {
    File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => HandoffError::TranscriptUnavailable(name(path)),
        _ => io_error(path, e),
    })
}

fn io_error(path: &Path, e: std::io::Error) -> HandoffError {
    HandoffError::TranscriptUnavailable(format!("{}: {e}", name(path)))
}

/// The file name only: full paths can carry user names into the popup.
pub fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(content: &str, limits: &ReadLimits) -> (Vec<Result<Record, HandoffError>>, bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, content).unwrap();
        let mut r = read_records(&path, limits).unwrap();
        let items: Vec<_> = r.by_ref().collect();
        (items, r.partial_tail())
    }

    #[test]
    fn reads_finished_lines_and_holds_a_partial_tail() {
        let (items, partial) = records("{\"a\":1}\n\n{\"b\":2}\n{\"c\":", &ReadLimits::default());
        assert_eq!(items.len(), 2);
        assert!(partial);
        let second = items[1].as_ref().unwrap();
        assert_eq!(second.value["b"], 2);
        assert_eq!(second.offset, 9);
    }

    #[test]
    fn broken_finished_line_is_corrupt() {
        let (items, _) = records("{\"a\":1}\n{oops\n{\"b\":2}\n", &ReadLimits::default());
        assert!(matches!(
            items.last(),
            Some(Err(HandoffError::TranscriptCorrupt(_)))
        ));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn long_line_is_a_limit_error_even_unfinished() {
        let limits = ReadLimits {
            max_line_bytes: 8,
            ..ReadLimits::default()
        };
        for content in ["{\"a\":\"0123456789\"}\n", "{\"a\":\"0123456789"] {
            let (items, _) = records(content, &limits);
            assert!(
                matches!(items.last(), Some(Err(HandoffError::ReadLimitExceeded(_)))),
                "{content}"
            );
        }
    }

    #[test]
    fn read_at_returns_the_same_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"b\":\"日本\"}\n").unwrap();
        let recs: Vec<_> = read_records(&path, &ReadLimits::default())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let again = read_at(&path, recs[1].offset, recs[1].len).unwrap();
        assert_eq!(again, recs[1].value);
    }
}
