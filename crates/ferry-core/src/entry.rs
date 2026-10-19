use std::io;
use std::path::{Path, PathBuf};

pub const TICKS_PER_SEC: i64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Windows FILETIME ticks (100 ns since 1601).
    pub mtime: i64,
}

/// Prefix absolute Windows paths with `\\?\` (or `\\?\UNC\`) to lift MAX_PATH.
pub fn long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") {
        p.to_path_buf()
    } else if let Some(rest) = s.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{rest}"))
    } else if s.len() >= 2 && s.as_bytes()[1] == b':' {
        PathBuf::from(format!(r"\\?\{s}"))
    } else {
        p.to_path_buf()
    }
}

#[cfg(windows)]
fn mtime_of(m: &std::fs::Metadata) -> i64 {
    use std::os::windows::fs::MetadataExt;
    m.last_write_time() as i64
}

#[cfg(not(windows))]
fn mtime_of(_: &std::fs::Metadata) -> i64 {
    0
}

/// A name folded the way NTFS and SMB compare names: each character to its simple uppercase,
/// or itself when that is more than one character (ß), as Windows' upcase table does. Unlike
/// `to_lowercase`, ς and σ are one name, and ı, ſ and the Kelvin sign fold as Windows folds them.
pub fn fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut u = c.to_uppercase();
            match (u.next(), u.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

/// Listing order: case-insensitive (as Windows folds case), then exact name.
pub fn name_order(a: &str, b: &str) -> std::cmp::Ordering {
    fold(a).cmp(&fold(b)).then_with(|| a.cmp(b))
}

/// A name a server may hand back that is safe to join under a root: one plain component, valid
/// Unicode. Rejects separators, drive or stream colons, `.`/`..` and control characters, which a
/// hostile server could use to reach outside the folder (`Path::join` replaces the base when
/// given an absolute name).
pub fn safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.chars().any(|c| matches!(c, '\\' | '/' | ':') || c.is_control())
}

/// The file Ferry leaves at a share's root so it can tell the real server from a look-alike.
pub const IDENTITY_FILE: &str = ".ferry-id";

/// List one directory, sorted case-insensitively; skips `.ferry-part` temp files and Ferry's
/// identity file. A name that is
/// not [`safe_name`] fails the whole listing (`InvalidData`), so it is treated as unlistable:
/// nothing in it is copied by that name and no deletes follow from it.
pub fn list_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    list_dir_and_parts(dir).map(|(entries, _)| entries)
}

/// `list_dir`, and the names of the `.ferry-part` files it skipped.
pub fn list_dir_and_parts(dir: &Path) -> io::Result<(Vec<Entry>, Vec<String>)> {
    let (mut out, mut parts) = (Vec::new(), Vec::new());
    for de in std::fs::read_dir(long_path(dir))? {
        let de = de?;
        let name = de.file_name().into_string().ok().filter(|n| safe_name(n)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("unsafe file name {:?} in {}", de.file_name(), dir.display()))
        })?;
        if name.ends_with(".ferry-part") {
            parts.push(name);
            continue;
        }
        if name == IDENTITY_FILE {
            continue;
        }
        let m = de.metadata()?;
        out.push(Entry {
            name,
            is_dir: m.is_dir(),
            size: if m.is_dir() { 0 } else { m.len() },
            mtime: mtime_of(&m),
        });
    }
    out.sort_by(|a, b| name_order(&a.name, &b.name));
    Ok((out, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn names_fold_as_windows_folds_them() {
        assert_eq!(fold("Report.TXT"), fold("report.txt"));
        assert_eq!(fold("\u{3c2}"), fold("\u{3c3}"), "final and medial sigma are one letter");
        assert_eq!(fold("stra\u{df}e"), "STRA\u{df}E", "sharp s stays: its uppercase is two letters");
        assert_ne!(fold("k"), fold("\u{212a}"), "the Kelvin sign is its own name on NTFS");
        assert_eq!(name_order("a", "B"), std::cmp::Ordering::Less);
    }

    #[test]
    fn long_path_prefixes() {
        assert_eq!(long_path(Path::new(r"C:\a\b")), PathBuf::from(r"\\?\C:\a\b"));
        assert_eq!(long_path(Path::new(r"\\nas\repo\x.zip")), PathBuf::from(r"\\?\UNC\nas\repo\x.zip"));
        assert_eq!(long_path(Path::new(r"\\?\C:\a")), PathBuf::from(r"\\?\C:\a"));
    }

    #[test]
    fn list_dir_sorts_and_skips_parts_and_the_identity_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.zip"), b"bb").unwrap();
        std::fs::write(d.path().join("A.zip"), b"a").unwrap();
        std::fs::write(d.path().join("c.zip.ferry-part"), b"x").unwrap();
        std::fs::write(d.path().join(IDENTITY_FILE), b"token").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let names: Vec<_> = list_dir(d.path()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["A.zip", "b.zip", "sub"]);
    }

    #[test]
    fn unsafe_names_are_rejected() {
        for ok in ["a.zip", "x y", "trailing.", "CON", "ünï", ".hidden", "a..b"] {
            assert!(safe_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", r"C:\Users\x", r"\x", "a/b", r"a\b", "a:b", "file.txt:ads", "a\nb", "a\0b"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
    }
}
