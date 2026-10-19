//! Auto-update: find a newer GitHub release, check its signed checksums, download and verify its
//! installer, and leave the files that tell the next start how the install went. Runs on worker
//! threads; the engine decides when.
use crate::win::http::{self, HttpError};
use ferry_core::update::{checksum_for, der_signature, is_newer, p256_point, pick_release, Release};
use std::fs;
use std::path::{Path, PathBuf};
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

/// Where Ferry looks for its newest release. A fork sets `FERRY_RELEASES_URL` at build time.
pub const RELEASES_URL: &str = match option_env!("FERRY_RELEASES_URL") {
    Some(u) => u,
    None => "https://api.github.com/repos/zaryabmustafa/ferry/releases/latest",
};
/// The public half of the key CI signs `SHA256SUMS.txt` with: the hex of its uncompressed P-256
/// point (docs/verification.md, "Update signing key", says how to make one). While a key is
/// being rotated it holds the old and the new key, separated by a space. Ferry installs nothing
/// these keys didn't sign, so while it is empty no update is installed. A fork or a local test
/// sets `FERRY_UPDATE_KEY` at build time.
pub const UPDATE_KEY: &str = match option_env!("FERRY_UPDATE_KEY") {
    Some(k) => k,
    None => "040181b41b083ead8daa6699e42f2289feb2e9ab3fb75c9ae864d9ba3175309959c8ad0c5edcfe15f54ec5dd79ae9f77fbfa04ba07ffd44b708590af5311b3ab03",
};
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY_SECS: i64 = 86_400;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    UpToDate,
    Available(Release),
    /// Downloaded and verified, ready to run; the last part is its SHA-256 (lowercase hex), to
    /// check again just before it runs.
    Ready(Release, PathBuf, String),
    BadChecksum(String),
    /// The release's SHA256SUMS.txt.sig is missing, or isn't `UPDATE_KEY`'s signature of it.
    BadSignature(String),
    Failed(String),
}

/// This exe is the one the installer put in place (not a portable or development copy).
pub fn installed() -> bool {
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Ferry"),
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return false;
    }
    let loc = reg_string(&buf, len);
    std::env::current_exe().ok().and_then(|e| e.parent().map(|p| same_dir(p, &loc))).unwrap_or(false)
}

/// A REG_SZ read into `buf`, `len_bytes` long including its terminator, clamped to the buffer.
fn reg_string(buf: &[u16], len_bytes: u32) -> String {
    let n = (len_bytes as usize / 2).min(buf.len());
    let s = &buf[..n];
    String::from_utf16_lossy(s.iter().position(|&c| c == 0).map_or(s, |end| &s[..end]))
}

fn same_dir(exe_dir: &Path, install_location: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).to_lowercase();
    !install_location.is_empty() && norm(&exe_dir.to_string_lossy()) == norm(install_location)
}

/// Whether the engine may look for an update on its own right now.
/// (Whether the connection is metered is asked after this, since that is a COM call.)
pub fn may_auto_check(on: bool, installed: bool, paused: bool, trip: bool, windows_open: bool, since_ok_secs: i64) -> bool {
    on && installed && !paused && !trip && !windows_open && since_ok_secs >= DAY_SECS
}

/// Asks the releases API whether there is something newer than this build.
pub fn check(timeout_ms: u32) -> Outcome {
    if ferry_core::update::https_host(RELEASES_URL).is_none() {
        return Outcome::Failed(format!("{RELEASES_URL} is not an https address; Ferry only updates over https"));
    }
    match http::get(RELEASES_URL, 64 << 10, timeout_ms) {
        Ok(body) => match pick_release(&body, RELEASES_URL) {
            Some(r) if is_newer(&r.version, VERSION) => Outcome::Available(r),
            _ => Outcome::UpToDate,
        },
        // No release yet, or the repository is private: nothing newer, quietly.
        Err(HttpError::Status(404)) => Outcome::UpToDate,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// True when `sig` (DER, as `openssl dgst -sha256 -sign` writes it) is the signature of exactly
/// `sums` by one of `keys` (see `UPDATE_KEY`).
pub fn signed(sums: &[u8], sig: &[u8], keys: &str) -> bool {
    der_signature(sig).is_some_and(|s| keys.split_whitespace().filter_map(p256_point).any(|k| http::verify_p256(&k, sums, &s)))
}

/// Downloads `r`'s installer into `dir` and checks it against the release's SHA256SUMS.txt, which
/// must carry `UPDATE_KEY`'s signature: the checksums come from the same server as the installer,
/// so on their own they only catch a damaged download.
pub fn fetch(r: Release, dir: &Path) -> Outcome {
    let name = format!("Ferry-Setup-{}.exe", r.version);
    let Some(sig_url) = r.sig_url.as_deref() else {
        return Outcome::BadSignature(r.version);
    };
    let (sums, sig) = match (http::get(&r.sums_url, 64 << 10, 30_000), http::get(sig_url, 1 << 10, 30_000)) {
        (Ok(sums), Ok(sig)) => (sums, sig),
        (Err(e), _) => return Outcome::Failed(format!("SHA256SUMS.txt: {e}")),
        (_, Err(e)) => return Outcome::Failed(format!("SHA256SUMS.txt.sig: {e}")),
    };
    if !signed(&sums, &sig, UPDATE_KEY) {
        return Outcome::BadSignature(r.version);
    }
    let sums = String::from_utf8_lossy(&sums);
    let Some(want) = checksum_for(&sums, &name).map(str::to_lowercase) else {
        return Outcome::BadChecksum(r.version);
    };
    let dest = dir.join(&name);
    if let Err(e) = fs::create_dir_all(dir).map_err(HttpError::from).and_then(|()| http::download(&r.setup_url, &dest, 64 << 20)) {
        return Outcome::Failed(format!("{name}: {e}"));
    }
    match http::sha256_file(&dest) {
        Ok(got) if got == want => Outcome::Ready(r, dest, want),
        Ok(_) => {
            let _ = fs::remove_file(&dest);
            Outcome::BadChecksum(r.version)
        }
        Err(e) => Outcome::Failed(format!("{name}: {e}")),
    }
}

/// At engine start: how the last install went, if one was started. `(version, took)`; a version
/// that didn't take is skipped from then on. Clears everything else in `dir` but `skip`.
pub fn after_restart(dir: &Path, running: &str) -> Option<(String, bool)> {
    let marker = fs::read_to_string(dir.join("installing")).ok().map(|s| s.trim().to_string());
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name() != "skip" {
            let _ = fs::remove_file(e.path());
        }
    }
    let v = marker.filter(|v| !v.is_empty())?;
    let took = v == running;
    if !took {
        skip(dir, &v);
    }
    Some((v, took))
}

/// Count a checksum mismatch for `version` in `run` (version, mismatches in a row). True on the
/// third in a row, when the release is given up; before that it's taken for a damaged download.
pub fn give_up_after_mismatch(run: &mut (String, u32), version: &str) -> bool {
    if run.0 == version {
        run.1 += 1;
    } else {
        *run = (version.to_string(), 1);
    }
    run.1 >= 3
}

pub fn skipped(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("skip")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn skip(dir: &Path, version: &str) {
    let _ = fs::create_dir_all(dir).and_then(|()| fs::write(dir.join("skip"), version));
}

/// What to tell a user who chose "Update to X.Y.Z" when the update didn't go ahead.
pub fn manual_failure(o: &Outcome) -> Option<String> {
    match o {
        Outcome::Ready(..) | Outcome::Available(_) => None,
        Outcome::UpToDate => Some("No newer version of Ferry was found.".into()),
        Outcome::BadChecksum(v) => Some(format!("Ferry {v} failed its checksum and wasn't installed.")),
        Outcome::BadSignature(v) => Some(format!("Ferry {v} isn't signed with Ferry's update key and wasn't installed.")),
        Outcome::Failed(e) => Some(format!("Ferry couldn't update: {e}")),
    }
}

/// The activity-log line for an outcome; quiet ones (nothing new, private repo) have none, and
/// neither has a failure right after another one (hourly retries while offline).
pub fn outcome_line(o: &Outcome, failed_before: bool) -> Option<String> {
    match o {
        Outcome::UpToDate | Outcome::Available(_) | Outcome::Ready(..) => None,
        Outcome::BadChecksum(v) => Some(format!("update {v} failed its checksum, not installed")),
        Outcome::BadSignature(v) => Some(format!("update {v} isn't signed with Ferry's update key, not installed")),
        Outcome::Failed(_) if failed_before => None,
        Outcome::Failed(e) => Some(format!("update check failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn may_auto_check_guards() {
        let ok = may_auto_check;
        assert!(ok(true, true, false, false, false, 86_400));
        assert!(!ok(false, true, false, false, false, 86_400), "switched off");
        assert!(!ok(true, false, false, false, false, 86_400), "portable");
        assert!(!ok(true, true, true, false, false, 86_400), "paused");
        assert!(!ok(true, true, false, true, false, 86_400), "trip running");
        assert!(!ok(true, true, false, false, true, 86_400), "a window is open");
        assert!(!ok(true, true, false, false, false, 86_399), "checked within a day");
    }

    #[test]
    fn after_restart_success() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("installing"), "0.2.3").unwrap();
        fs::write(d.path().join("Ferry-Setup-0.2.3.exe"), "x").unwrap();
        fs::write(d.path().join("skip"), "0.2.1").unwrap();
        assert_eq!(after_restart(d.path(), "0.2.3"), Some(("0.2.3".to_string(), true)));
        let left: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["skip"]);
    }

    #[test]
    fn after_restart_failed_install_skips_version() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("installing"), "0.2.3\n").unwrap();
        assert_eq!(after_restart(d.path(), "0.2.2"), Some(("0.2.3".to_string(), false)));
        assert_eq!(skipped(d.path()).as_deref(), Some("0.2.3"));
        assert!(!d.path().join("installing").exists());
    }

    #[test]
    fn after_restart_without_marker_is_none() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Ferry-Setup-0.2.3.exe.part"), "x").unwrap();
        assert_eq!(after_restart(d.path(), "0.2.2"), None);
        assert!(!d.path().join("Ferry-Setup-0.2.3.exe.part").exists(), "stale downloads are cleared");
        assert_eq!(after_restart(&d.path().join("missing"), "0.2.2"), None);
    }

    #[test]
    fn a_release_is_given_up_on_the_third_mismatch_in_a_row() {
        let mut run = (String::new(), 0);
        assert!(!give_up_after_mismatch(&mut run, "0.2.5"));
        assert!(!give_up_after_mismatch(&mut run, "0.2.5"));
        assert!(!give_up_after_mismatch(&mut run, "0.2.6"), "a new version starts counting again");
        assert!(!give_up_after_mismatch(&mut run, "0.2.6"));
        assert!(give_up_after_mismatch(&mut run, "0.2.6"));
    }

    #[test]
    fn outcome_lines() {
        assert_eq!(outcome_line(&Outcome::UpToDate, false), None);
        assert_eq!(outcome_line(&Outcome::BadChecksum("0.2.3".into()), false).as_deref(), Some("update 0.2.3 failed its checksum, not installed"));
        assert_eq!(outcome_line(&Outcome::BadSignature("0.2.3".into()), false).as_deref(), Some("update 0.2.3 isn't signed with Ferry's update key, not installed"));
        assert_eq!(outcome_line(&Outcome::Failed("timed out".into()), false).as_deref(), Some("update check failed: timed out"));
        assert_eq!(outcome_line(&Outcome::Failed("timed out".into()), true), None, "only the first failure in a row");
    }

    /// A P-256 key pair made for one test: its public point as hex, and a signer that returns
    /// what `openssl dgst -sha256 -sign` would (DER).
    fn test_key() -> (String, impl Fn(&[u8]) -> Vec<u8>) {
        use crate::win::http::Key;
        use windows::Win32::Security::Cryptography::*;
        let mut key = Key(BCRYPT_KEY_HANDLE::default());
        let mut blob = [0u8; 72];
        let mut n = 0u32;
        unsafe {
            assert!(BCryptGenerateKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, &mut key.0, 256, 0).is_ok());
            assert!(BCryptFinalizeKeyPair(key.0, 0).is_ok());
            assert!(BCryptExportKey(key.0, None, BCRYPT_ECCPUBLIC_BLOB, Some(&mut blob), &mut n, 0).is_ok());
        }
        let hex = std::iter::once(4u8).chain(blob[8..].iter().copied()).map(|b| format!("{b:02x}")).collect();
        let int = |x: &[u8]| {
            let x = &x[x.iter().position(|&b| b != 0).unwrap_or(31)..];
            let pad = x[0] >= 0x80;
            [&[2, x.len() as u8 + pad as u8][..], if pad { &[0] } else { &[] }, x].concat()
        };
        let sign = move |msg: &[u8]| {
            // The whole `Key`, not just its (Copy) handle, so the key lives as long as the closure.
            let key = &key;
            let (mut hash, mut raw, mut n) = ([0u8; 32], [0u8; 64], 0u32);
            unsafe {
                assert!(BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, msg, &mut hash).is_ok());
                assert!(BCryptSignHash(key.0, None, &hash, Some(&mut raw), &mut n, BCRYPT_FLAGS(0)).is_ok());
            }
            let body = [int(&raw[..32]), int(&raw[32..])].concat();
            [&[0x30, body.len() as u8][..], &body].concat()
        };
        (hex, sign)
    }

    const SUMS: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  Ferry-Setup-0.2.3.exe\n";

    #[test]
    fn signed_sums_verify() {
        let (key, sign) = test_key();
        assert!(signed(SUMS, &sign(SUMS), &key));
    }

    #[test]
    fn tampered_sums_are_refused() {
        let (key, sign) = test_key();
        let sig = sign(SUMS);
        let mut sums = SUMS.to_vec();
        sums[0] = b'1';
        assert!(!signed(&sums, &sig, &key), "one changed byte");
        assert!(!signed(&[SUMS, b"\n"].concat(), &sig, &key), "one added byte");
        let mut bad = sig.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(!signed(SUMS, &bad, &key), "changed signature");
        assert!(!signed(SUMS, b"", &key), "empty signature");
    }

    #[test]
    fn signature_by_another_key_is_refused() {
        let (key, _) = test_key();
        let (_, other) = test_key();
        assert!(!signed(SUMS, &other(SUMS), &key));
        assert!(!signed(SUMS, &other(SUMS), ""), "a build without a key installs nothing");
    }

    #[test]
    fn either_key_verifies_during_a_rotation() {
        let (old, sign_old) = test_key();
        let (new, sign_new) = test_key();
        let both = format!("{old} {new}");
        assert!(signed(SUMS, &sign_old(SUMS), &both));
        assert!(signed(SUMS, &sign_new(SUMS), &both));
        assert!(!signed(SUMS, &sign_old(SUMS), &new), "the old key, once dropped");
    }

    #[test]
    fn openssl_signature_verifies() {
        // From `openssl dgst -sha256 -sign` with a throwaway key (since deleted), over the message
        // below. Both of its integers carry the sign byte.
        const KEY: &str = "041ca5932f06d42e145b5568982fa993942c8ed5e65843d2779ff844fb3e50a6d74d87327169420eb200ff426ddb7b83f19cfb03683dbf7832b2ca1b0024aa250a";
        const SIG: &str = "304602210089c17e385ced42fbce47027aeafb038dd42211c69998c7189e90e48cb64792dd022100917a75bab77f7407f8055ad9487b43c55ce8f8f550dfdfbe4a3f900e72cb596f";
        let sig: Vec<u8> = (0..SIG.len()).step_by(2).map(|i| u8::from_str_radix(&SIG[i..i + 2], 16).unwrap()).collect();
        assert!(signed(b"abcd  Ferry-Setup-9.9.9.exe\n", &sig, KEY));
        assert!(!signed(b"abcd  Ferry-Setup-9.9.8.exe\n", &sig, KEY));
    }

    #[test]
    fn release_without_signature_is_refused_before_downloading() {
        let d = tempfile::tempdir().unwrap();
        // Unroutable addresses: reaching for any of them would end in Failed, not BadSignature.
        let r = Release { version: "0.2.3".into(), setup_url: "https://0.0.0.0/s".into(), sums_url: "https://0.0.0.0/h".into(), sig_url: None };
        assert_eq!(fetch(r, d.path()), Outcome::BadSignature("0.2.3".into()));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn embedded_update_keys_are_p256_points() {
        for k in UPDATE_KEY.split_whitespace() {
            assert!(p256_point(k).is_some(), "{k} in UPDATE_KEY is not 130 hex digits starting 04");
        }
    }

    #[test]
    fn manual_failures_are_told() {
        let r = Release { version: "0.2.3".into(), setup_url: String::new(), sums_url: String::new(), sig_url: None };
        assert_eq!(manual_failure(&Outcome::Ready(r.clone(), PathBuf::new(), String::new())), None);
        assert_eq!(manual_failure(&Outcome::Available(r)), None);
        assert_eq!(manual_failure(&Outcome::UpToDate).as_deref(), Some("No newer version of Ferry was found."));
        assert_eq!(manual_failure(&Outcome::BadChecksum("0.2.3".into())).as_deref(), Some("Ferry 0.2.3 failed its checksum and wasn't installed."));
        assert_eq!(
            manual_failure(&Outcome::BadSignature("0.2.3".into())).as_deref(),
            Some("Ferry 0.2.3 isn't signed with Ferry's update key and wasn't installed.")
        );
        assert_eq!(manual_failure(&Outcome::Failed("timed out".into())).as_deref(), Some("Ferry couldn't update: timed out"));
    }

    #[test]
    fn registry_string_never_overruns() {
        let buf: Vec<u16> = "C:/Ferry\0".encode_utf16().collect();
        assert_eq!(reg_string(&buf, (buf.len() * 2) as u32), "C:/Ferry");
        assert_eq!(reg_string(&buf, 4000), "C:/Ferry", "a length past the buffer (ERROR_MORE_DATA) is clamped");
        assert_eq!(reg_string(&buf, 0), "");
    }

    #[test]
    fn same_dir_ignores_case_and_trailing_slash() {
        assert!(same_dir(Path::new(r"C:\Users\a\AppData\Local\Programs\Ferry"), r"c:\users\a\appdata\local\programs\ferry\"));
        assert!(!same_dir(Path::new(r"D:\dev\ferry\target\release"), r"C:\Users\a\AppData\Local\Programs\Ferry"));
        assert!(!same_dir(Path::new(r"C:\Ferry"), ""));
    }
}
