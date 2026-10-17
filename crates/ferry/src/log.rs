//! Small rotating activity log: `YYYY-MM-DD HH:MM:SS  text`, local time.
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 1_048_576;
const ROTATED: &str = "activity.1.log";

pub struct ActivityLog {
    path: PathBuf,
    /// None after a rotation whose reopen failed; retried on the next write.
    file: Option<File>,
    size: u64,
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

impl ActivityLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut log = Self { path: path.to_path_buf(), file: None, size: 0 };
        log.file = Some(open_append(path)?);
        log.size = log.file.as_ref().unwrap().metadata()?.len();
        Ok(log)
    }

    fn reopen(&mut self) {
        if let Ok(f) = open_append(&self.path) {
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
        }
    }

    /// Append one line; logging never fails the caller.
    pub fn line(&mut self, text: &str) {
        let entry = format!("{}  {}\n", stamp(), text);
        if self.file.is_none() {
            self.reopen();
        }
        if self.size > 0 && self.size + entry.len() as u64 > MAX_BYTES {
            // std opens with FILE_SHARE_DELETE, so renaming the open file is fine.
            if std::fs::rename(&self.path, self.path.with_file_name(ROTATED)).is_ok() {
                self.file = None; // never keep appending to the renamed file
                self.reopen();
            }
        }
        if let Some(f) = &mut self.file {
            if f.write_all(entry.as_bytes()).is_ok() {
                self.size += entry.len() as u64;
            }
        }
    }
}

/// Last `n` lines of `path`, topped up from `activity.1.log` when short.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let mut lines = tail_file(path, n);
    let old = path.with_file_name(ROTATED);
    if lines.len() < n && old != path {
        let mut all = tail_file(&old, n - lines.len());
        all.append(&mut lines);
        lines = all;
    }
    lines
}

/// Reads only the end of the file, in 8 KiB chunks, until it has n+1 newlines.
fn tail_file(path: &Path, n: usize) -> Vec<String> {
    let go = || -> io::Result<Vec<String>> {
        let mut f = File::open(path)?;
        let mut pos = f.metadata()?.len();
        let mut buf: Vec<u8> = Vec::new();
        let mut newlines = 0;
        while pos > 0 && newlines <= n {
            let step = pos.min(8192);
            pos -= step;
            f.seek(SeekFrom::Start(pos))?;
            let mut chunk = vec![0; step as usize];
            f.read_exact(&mut chunk)?;
            newlines += chunk.iter().filter(|&&b| b == b'\n').count();
            chunk.extend_from_slice(&buf);
            buf = chunk;
        }
        let text = String::from_utf8_lossy(&buf);
        let mut it = text.split('\n');
        if pos > 0 {
            it.next(); // starts mid-line
        }
        let all: Vec<String> = it.filter(|l| !l.is_empty()).map(str::to_string).collect();
        Ok(all[all.len().saturating_sub(n)..].to_vec())
    };
    go().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_rotates_at_1mb() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("activity.log");
        let mut log = ActivityLog::open(&p).unwrap();
        let text = "x".repeat(980);
        for _ in 0..1200 {
            log.line(&text);
        }
        assert!(dir.path().join("activity.1.log").exists());
        let len = std::fs::metadata(&p).unwrap().len();
        assert!(len > 0 && len <= 1_048_576, "current is {len}");
        let old = std::fs::metadata(dir.path().join("activity.1.log")).unwrap().len();
        assert!(old <= 1_048_576 && old > 1_000_000, "rotated is {old}");
    }

    #[test]
    fn tail_returns_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("activity.log");
        let mut log = ActivityLog::open(&p).unwrap();
        for i in 0..2000 {
            log.line(&format!("line {i} {}", "y".repeat(30)));
        }
        let t = tail(&p, 3);
        assert_eq!(t.len(), 3);
        assert!(t[0].contains("line 1997 ") && t[2].contains("line 1999 "), "{t:?}");
        // lines carry `YYYY-MM-DD HH:MM:SS  ` in front
        assert_eq!(&t[0][4..5], "-");
        assert_eq!(&t[0][19..21], "  ");
        assert_eq!(tail(&p, 5000).len(), 2000);
        assert!(tail(&dir.path().join("missing.log"), 3).is_empty());
    }

    #[test]
    fn tail_reaches_into_rotated_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("activity.log");
        std::fs::write(dir.path().join("activity.1.log"), "old 1\nold 2\n").unwrap();
        std::fs::write(&p, "new 1\n").unwrap();
        assert_eq!(tail(&p, 2), vec!["old 2", "new 1"]);
    }
}
