use crate::entry::fold;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// serde adapter: u128 as a hex string (JSON numbers can't carry it reliably).
mod hex_u128 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:032x}"))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

/// Read JSON state; a missing file is the default (empty) state, a corrupt one is an error.
pub(crate) fn load_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

/// Write JSON state to a temp file, then rename over the target.
pub(crate) fn save_json<T: Serialize>(path: &Path, v: &T) -> io::Result<()> {
    let s = serde_json::to_vec_pretty(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(path, &s)
}

/// Write `bytes` to `<path>.tmp`, flush it to disk, then rename it over `path`, so a crash
/// leaves either the old file or the new one, never a torn one.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |w| w.write_all(bytes))
}

/// `write_atomic` for content written piece by piece; on an error the temp file is removed
/// and `path` is left as it was.
pub(crate) fn write_atomic_with(path: &Path, fill: impl FnOnce(&mut dyn io::Write) -> io::Result<()>) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut w = io::BufWriter::new(std::fs::File::create(&tmp)?);
    let r = fill(&mut w).and_then(|()| w.flush()).and_then(|()| w.get_ref().sync_all());
    drop(w);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
}

/// Load state; a corrupt file (Ruling R33) is renamed to `<path>.corrupt-<unix secs>` and
/// the empty state is used instead. Returns the state and where a corrupt file went.
pub fn load_or_reset<T: DeserializeOwned + Default>(path: &Path, unix_now: i64) -> io::Result<(T, Option<PathBuf>)> {
    match load_json(path) {
        Ok(v) => Ok((v, None)),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{unix_now}"));
            let aside = PathBuf::from(aside);
            std::fs::rename(path, &aside)?;
            Ok((T::default(), Some(aside)))
        }
        Err(e) => Err(e),
    }
}

/// A file already on the laptop, waiting to be delivered to the destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoItem {
    pub pair: String,
    /// Path relative to the pair root, `\` separated.
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
    #[serde(with = "hex_u128")]
    pub hash: u128,
}

/// The file whose `.ferry-part` may be on disk: a part is only resumed for this exact version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partial {
    pub pair: String,
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoManifest {
    pub items: Vec<CargoItem>,
    #[serde(default)]
    pub partial: Option<Partial>,
}

impl CargoManifest {
    /// True if this exact version (size + mtime; rel is case-insensitive like SMB) of the file is already carried.
    // ponytail: linear scan; index by (pair, rel) if the manifest grows to many thousands.
    pub fn has(&self, pair: &str, rel: &str, size: u64, mtime: i64) -> bool {
        self.items
            .iter()
            .any(|i| i.pair == pair && fold(&i.rel) == fold(rel) && i.size == size && i.mtime == mtime)
    }

    /// Missing file → empty manifest.
    pub fn load(path: &Path) -> io::Result<Self> {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }

    pub fn carried_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    /// Drop every version of (pair, rel); rel matches case-insensitively, pair exactly.
    pub fn remove(&mut self, pair: &str, rel: &str) {
        let rel = fold(rel);
        self.items.retain(|i| !(i.pair == pair && fold(&i.rel) == rel));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(rel: &str, size: u64, hash: u128) -> CargoItem {
        CargoItem { pair: "p".into(), rel: rel.into(), size, mtime: 5, hash }
    }

    #[test]
    fn manifest_round_trips_and_sums() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        assert_eq!(CargoManifest::load(&p).unwrap(), CargoManifest::default());
        let m = CargoManifest { items: vec![item("a", 10, u128::MAX), item("b", 32, (u64::MAX as u128) + 7)], ..Default::default() };
        m.save(&p).unwrap();
        assert_eq!(CargoManifest::load(&p).unwrap(), m);
        assert_eq!(m.carried_bytes(), 42);
    }

    #[test]
    fn corrupt_state_is_moved_aside_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        std::fs::write(&p, b"{\"items\": [tru").unwrap();
        let (m, aside) = load_or_reset::<CargoManifest>(&p, 1234).unwrap();
        assert_eq!(m, CargoManifest::default());
        let aside = aside.unwrap();
        assert_eq!(aside, dir.path().join("manifest.json.corrupt-1234"));
        assert_eq!(std::fs::read(&aside).unwrap(), b"{\"items\": [tru");
        assert!(!p.exists());
        // A good (or missing) file is left alone.
        let (m, aside) = load_or_reset::<CargoManifest>(&p, 1235).unwrap();
        assert_eq!((m, aside), (CargoManifest::default(), None));
        let good = CargoManifest { items: vec![item("a", 1, 2)], ..Default::default() };
        good.save(&p).unwrap();
        assert_eq!(load_or_reset::<CargoManifest>(&p, 1236).unwrap(), (good, None));
        assert!(!dir.path().join("manifest.json.tmp").exists());
    }

    #[test]
    fn remove_is_case_insensitive_on_rel_exact_on_pair() {
        let mut m = CargoManifest { items: vec![item("Dir\\A.TXT", 1, 1), item("b", 1, 2)], ..Default::default() };
        m.remove("P", "dir\\a.txt");
        assert_eq!(m.items.len(), 2);
        m.remove("p", "dir\\a.txt");
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].rel, "b");
    }
}
