//! Audit S1: which server is the real one. A router's MAC address and an answer on port 445 are
//! easy to imitate on a network Ferry doesn't control, so the first trip to each server leaves a
//! random token there (`.ferry-id`, hidden, at the root of a share Ferry uses) and keeps a copy on
//! the laptop. Every later trip reads it back before copying: a look-alike server can't show it.
use ferry_core::ipc::Side;
use std::fs;
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

/// The identity file's name. `list_dir` skips it, so it is never mirrored or deleted.
pub const FILE: &str = ferry_core::entry::IDENTITY_FILE;

/// The tokens this laptop left on each server; empty until the first trip there. Kept as
/// `source <token>` and `destination <token>` lines.
#[derive(Debug, Default, PartialEq)]
struct Tokens {
    source: String,
    destination: String,
}

impl Tokens {
    fn side(&mut self, side: Side) -> &mut String {
        match side {
            Side::Source => &mut self.source,
            Side::Destination => &mut self.destination,
        }
    }
}

/// How the check went, when the trip may go ahead.
#[derive(Debug, PartialEq)]
pub enum Checked {
    /// The server shows the token this laptop left there.
    Matches,
    /// First trip here: a new token was left on the server.
    Pinned,
    /// No token yet, and none could be left (a read-only share, say): unchecked.
    NotPinned(String),
}

fn load(state: &Path) -> Tokens {
    let mut t = Tokens::default();
    for line in fs::read_to_string(state).unwrap_or_default().lines() {
        match line.split_once(' ') {
            Some(("source", v)) => t.source = v.trim().into(),
            Some(("destination", v)) => t.destination = v.trim().into(),
            _ => {}
        }
    }
    t
}

fn save(state: &Path, t: &Tokens) -> io::Result<()> {
    fs::write(state, format!("source {}\ndestination {}\n", t.source, t.destination))
}

/// Check `share_root` against the token kept for `side`, or leave a new one (`new_token`) when
/// there is none yet. `Err` is why this server isn't trusted: the trip must not copy.
pub fn check(state: &Path, side: Side, share_root: &Path, new_token: &dyn Fn() -> String) -> Result<Checked, String> {
    let mut tokens = load(state);
    let file = share_root.join(FILE);
    let kept = tokens.side(side).clone();
    if kept.is_empty() {
        let token = new_token();
        // An existing file is from another laptop or an earlier setup: this laptop's token replaces it.
        let _ = fs::remove_file(&file);
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(0x2) // FILE_ATTRIBUTE_HIDDEN
            .open(&file)
            .and_then(|mut f| f.write_all(token.as_bytes()));
        return Ok(match written {
            Ok(()) => {
                *tokens.side(side) = token;
                match save(state, &tokens) {
                    Ok(()) => Checked::Pinned,
                    Err(e) => Checked::NotPinned(e.to_string()),
                }
            }
            Err(e) => Checked::NotPinned(e.to_string()),
        });
    }
    match fs::read_to_string(&file) {
        Ok(found) if found.trim() == kept => Ok(Checked::Matches),
        Ok(_) => Err("its identity file is a different one".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err("it has no identity file".into()),
        Err(e) => Err(format!("its identity file can't be read: {e}")),
    }
}

/// Trust `side`'s server afresh: the next trip there leaves a new token. Used by Remember this
/// network, so a replaced or reset server can be trusted again.
pub fn forget(state: &Path, side: Side) -> io::Result<()> {
    let mut tokens = load(state);
    if tokens.side(side).is_empty() {
        return Ok(());
    }
    tokens.side(side).clear();
    save(state, &tokens)
}

/// 128 random bits as hex, from the system's RNG.
pub fn new_token() -> String {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut b = [0u8; 16];
    // With the system RNG this only fails on invalid arguments.
    let _ = unsafe { BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let (state, share) = (d.path().join("identity.txt"), d.path().join("share"));
        fs::create_dir(&share).unwrap();
        (d, state, share)
    }

    #[test]
    fn first_trip_pins_and_later_trips_match() {
        let (_d, state, share) = env();
        assert_eq!(check(&state, Side::Source, &share, &|| "t1".into()), Ok(Checked::Pinned));
        assert_eq!(fs::read_to_string(share.join(FILE)).unwrap(), "t1");
        assert_eq!(check(&state, Side::Source, &share, &|| unreachable!()), Ok(Checked::Matches));
        // The other side is separate.
        assert_eq!(check(&state, Side::Destination, &share, &|| "t2".into()), Ok(Checked::Pinned));
    }

    #[test]
    fn a_look_alike_server_is_refused() {
        let (_d, state, share) = env();
        check(&state, Side::Source, &share, &|| "real".into()).unwrap();
        let fake = share.with_file_name("fake");
        fs::create_dir(&fake).unwrap();
        assert!(check(&state, Side::Source, &fake, &|| unreachable!()).unwrap_err().contains("no identity file"));
        fs::write(fake.join(FILE), "guess").unwrap();
        assert!(check(&state, Side::Source, &fake, &|| unreachable!()).unwrap_err().contains("different"));
    }

    #[test]
    fn forgetting_lets_the_next_trip_pin_again() {
        let (_d, state, share) = env();
        check(&state, Side::Destination, &share, &|| "old".into()).unwrap();
        forget(&state, Side::Destination).unwrap();
        assert_eq!(check(&state, Side::Destination, &share, &|| "new".into()), Ok(Checked::Pinned));
        assert_eq!(fs::read_to_string(share.join(FILE)).unwrap(), "new");
    }

    #[test]
    fn an_unwritable_share_is_unchecked_not_refused() {
        let (_d, state, share) = env();
        let missing = share.join("no such folder");
        assert!(matches!(check(&state, Side::Source, &missing, &|| "t".into()), Ok(Checked::NotPinned(_))));
        assert_eq!(load(&state), Tokens::default(), "nothing kept, so the next trip tries again");
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
