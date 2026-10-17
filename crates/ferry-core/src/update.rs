//! Auto-update decisions that need no network: release versions, SHA256SUMS lines, the assets
//! Ferry needs from a GitHub (or Gitea) release, and the encodings of its update signing key and signature.

/// `vX.Y.Z` or `X.Y.Z`, three numeric parts. A pre-release suffix (`-rc1`) is no version here:
/// Ferry only updates to plain releases.
pub fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let mut it = tag.strip_prefix('v').unwrap_or(tag).split('.').map(|p| p.parse::<u32>().ok().filter(|_| p.bytes().all(|b| b.is_ascii_digit())));
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// True when `tag` is a release newer than the running `current`.
pub fn is_newer(tag: &str, current: &str) -> bool {
    matches!((parse_version(tag), parse_version(current)), (Some(t), Some(c)) if t > c)
}

/// The SHA-256 on the `sha256sum` line naming `file` (`<hex>  <name>` or `<hex> *<name>`).
pub fn checksum_for<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (hex, name) = l.split_once(' ')?;
        let name = name.strip_prefix(' ').or_else(|| name.strip_prefix('*'))?;
        (name.trim_end() == file && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
    })
}

/// What Ferry needs from a release: its version (no `v`) and where its installer, checksums and
/// the checksums' signature are. A release without a usable `SHA256SUMS.txt.sig` is still read,
/// so that refusing it is logged rather than looking like "no update".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub setup_url: String,
    pub sums_url: String,
    pub sig_url: Option<String>,
}

/// An ECDSA P-256 public key written as the hex of its uncompressed point (`04`, X, Y: 130 hex
/// digits), returned as X‖Y.
pub fn p256_point(hex: &str) -> Option<[u8; 64]> {
    let hex = hex.trim().strip_prefix("04").filter(|h| h.len() == 128 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let mut out = [0u8; 64];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// An ECDSA P-256 signature in the DER form `openssl dgst -sign` writes (SEQUENCE of INTEGER r,
/// INTEGER s), returned as r‖s for BCrypt.
pub fn der_signature(der: &[u8]) -> Option<[u8; 64]> {
    let [0x30, len, rest @ ..] = der else { return None };
    if *len as usize != rest.len() {
        return None;
    }
    let mut rest = rest;
    let mut out = [0u8; 64];
    for half in out.chunks_mut(32) {
        let [0x02, n, tail @ ..] = rest else { return None };
        let int = tail.get(..*n as usize).filter(|i| !i.is_empty())?;
        rest = &tail[int.len()..];
        // A positive INTEGER with its top bit set carries one leading zero byte.
        let int = match int {
            [0, x @ ..] if int.len() == 33 => x,
            _ if int.len() <= 32 => int,
            _ => return None,
        };
        half[32 - int.len()..].copy_from_slice(int);
    }
    rest.is_empty().then_some(out)
}

/// The host (and port) of an `https://` URL; `None` for any other scheme, or a host carrying
/// user info (`https://good@evil/`).
pub fn https_host(url: &str) -> Option<&str> {
    let rest = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).map(|_| &url[8..])?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && !host.contains('@')).then_some(host)
}

/// True when release files on `file_host` come from the same place as the API on `api_host`:
/// the same host, or github.com for api.github.com (GitHub serves release files from there).
fn same_place(api_host: &str, file_host: &str) -> bool {
    file_host.eq_ignore_ascii_case(api_host) || (api_host.eq_ignore_ascii_case("api.github.com") && file_host.eq_ignore_ascii_case("github.com"))
}

/// Reads a GitHub or Gitea `releases/latest` answer fetched from `api_url`. Anything unexpected
/// means no update, and so does an installer or checksum address that isn't https on the API's
/// own host (see `same_place`): both files come from one place, so a plain-http or off-site link
/// would let anyone on the path, or anyone who can edit the release text, swap them together.
pub fn pick_release(json: &[u8], api_url: &str) -> Option<Release> {
    let host = https_host(api_url)?;
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    parse_version(tag)?;
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    let url = |name: &str| {
        v.get("assets")?.as_array()?.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))?.get("browser_download_url")?.as_str().filter(|u| https_host(u).is_some_and(|h| same_place(host, h))).map(String::from)
    };
    Some(Release { setup_url: url(&format!("Ferry-Setup-{version}.exe"))?, sums_url: url("SHA256SUMS.txt")?, sig_url: url("SHA256SUMS.txt.sig"), version })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  Ferry-0.2.3-portable-x64.zip\n\
        ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789 *Ferry-Setup-0.2.3.exe\n\
        1111111111111111111111111111111111111111111111111111111111111111  Ferry-Setup-0.2.3.exe.sig\n";

    const RELEASE: &str = r#"{
        "id": 7, "tag_name": "v0.2.3", "name": "Ferry 0.2.3", "draft": false, "prerelease": false,
        "assets": [
            {"name": "Ferry-0.2.3-portable-x64.zip", "size": 1, "browser_download_url": "https://example.invalid/zip"},
            {"name": "Ferry-Setup-0.2.3.exe", "size": 5000000, "browser_download_url": "https://example.invalid/setup"},
            {"name": "SHA256SUMS.txt", "size": 200, "browser_download_url": "https://example.invalid/sums"},
            {"name": "SHA256SUMS.txt.sig", "size": 72, "browser_download_url": "https://example.invalid/sig"}
        ]
    }"#;

    #[test]
    fn parse_version_accepts() {
        assert_eq!(parse_version("v0.2.3"), Some((0, 2, 3)));
        assert_eq!(parse_version("10.0.1"), Some((10, 0, 1)));
    }

    #[test]
    fn parse_version_rejects() {
        for t in ["v0.3.0-rc1", "0.3", "latest", "v1.2.3.4", "", "v1.x.3", "v", "1..3"] {
            assert_eq!(parse_version(t), None, "{t}");
        }
    }

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.2.2", "0.2.2"));
        assert!(!is_newer("v0.2.1", "0.2.2"));
        assert!(!is_newer("v0.3.0-rc1", "0.2.2"));
        assert!(!is_newer("v0.3.0", "garbage"));
    }

    #[test]
    fn checksum_for_both_forms() {
        assert_eq!(checksum_for(SUMS, "Ferry-0.2.3-portable-x64.zip"), Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
        assert_eq!(checksum_for(SUMS, "Ferry-Setup-0.2.3.exe"), Some("ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"));
        assert_eq!(checksum_for(SUMS, "Ferry-Setup-0.2.4.exe"), None);
        assert_eq!(checksum_for("abc  Ferry-Setup-0.2.3.exe\n", "Ferry-Setup-0.2.3.exe"), None, "not 64 hex");
    }

    const API: &str = "https://example.invalid/api/v1/repos/o/ferry/releases/latest";

    #[test]
    fn update_links_must_be_https_on_the_api_host() {
        assert_eq!(https_host("https://Example.invalid:3000/x"), Some("Example.invalid:3000"));
        assert_eq!(https_host("HTTPS://example.invalid"), Some("example.invalid"));
        for bad in ["http://example.invalid/x", "https://", "https://u@evil.invalid/x", "ftp://x/y", "example.invalid"] {
            assert_eq!(https_host(bad), None, "{bad}");
        }
        assert_eq!(pick_release(RELEASE.as_bytes(), "http://example.invalid/api"), None, "API over plain http");
        let plain = RELEASE.replace("https://example.invalid/setup", "http://example.invalid/setup");
        assert_eq!(pick_release(plain.as_bytes(), API), None, "installer over plain http");
        let elsewhere = RELEASE.replace("https://example.invalid/sums", "https://evil.invalid/sums");
        assert_eq!(pick_release(elsewhere.as_bytes(), API), None, "checksums on another host");
        let sig_elsewhere = RELEASE.replace("https://example.invalid/sig", "https://evil.invalid/sig");
        assert_eq!(pick_release(sig_elsewhere.as_bytes(), API).unwrap().sig_url, None, "signature on another host is not used");
    }

    #[test]
    fn github_files_may_come_from_github_com() {
        const GITHUB: &str = "https://api.github.com/repos/o/ferry/releases/latest";
        let on = |host: &str| RELEASE.replace("example.invalid", host);
        let r = pick_release(on("github.com").as_bytes(), GITHUB).unwrap();
        assert_eq!(r.setup_url, "https://github.com/setup");
        assert_eq!(r.sig_url.as_deref(), Some("https://github.com/sig"));
        assert!(pick_release(on("GitHub.com").as_bytes(), GITHUB).is_some());
        assert!(pick_release(on("api.github.com").as_bytes(), GITHUB).is_some(), "the API's own host");
        for bad in ["objects.githubusercontent.com", "evil.github.com", "github.com.evil.invalid", "github.com:8443", "gist.github.com"] {
            assert_eq!(pick_release(on(bad).as_bytes(), GITHUB), None, "{bad}");
        }
        assert_eq!(pick_release(on("api.github.com").as_bytes(), "https://github.com/api/latest"), None, "only from api.github.com to github.com");
        assert_eq!(pick_release(on("github.com").as_bytes(), API), None, "github.com files for another API");
        assert_eq!(pick_release(on("github.com").as_bytes(), "http://api.github.com/repos/o/ferry/releases/latest"), None);
    }

    #[test]
    fn pick_release_reads_assets() {
        let r = pick_release(RELEASE.as_bytes(), API).unwrap();
        assert_eq!(
            r,
            Release {
                version: "0.2.3".into(),
                setup_url: "https://example.invalid/setup".into(),
                sums_url: "https://example.invalid/sums".into(),
                sig_url: Some("https://example.invalid/sig".into()),
            }
        );
        let unsigned = RELEASE.replace("SHA256SUMS.txt.sig", "other.sig");
        assert_eq!(pick_release(unsigned.as_bytes(), API).unwrap().sig_url, None, "an unsigned release is read, to be refused later");
    }

    #[test]
    fn p256_point_reads_uncompressed_hex() {
        let hex = format!("04{}{}", "ab".repeat(32), "CD".repeat(32));
        let p = p256_point(&format!("{hex}\n")).unwrap();
        assert_eq!((p[0], p[31], p[32], p[63]), (0xab, 0xab, 0xcd, 0xcd));
        for bad in ["", &hex[2..], &hex[..128], &format!("02{}", &hex[2..]), &format!("{hex}00"), &hex.replace("ab", "+b")] {
            assert_eq!(p256_point(bad), None, "{bad}");
        }
    }

    #[test]
    fn der_signature_reads_openssl_form() {
        // r with its sign byte, s short (leading zeros dropped).
        let mut der = vec![0x30, 0x43, 0x02, 0x21, 0x00];
        der.extend([0x80; 32]);
        der.extend([0x02, 0x1e]);
        der.extend([0x11; 30]);
        let raw = der_signature(&der).unwrap();
        assert_eq!(raw[..32], [0x80; 32]);
        assert_eq!(raw[32..34], [0, 0]);
        assert_eq!(raw[34..], [0x11; 30]);
        let bad = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut d = der.clone();
            f(&mut d);
            der_signature(&d)
        };
        assert_eq!(bad(&|d| d.push(0)), None, "trailing byte");
        assert_eq!(bad(&|d| d.truncate(40)), None, "cut short");
        assert_eq!(bad(&|d| d[0] = 0x31), None, "not a SEQUENCE");
        assert_eq!(bad(&|d| d[4] = 1), None, "33 bytes without a sign byte");
        assert_eq!(der_signature(&[0x30, 0x04, 0x02, 0x00, 0x02, 0x00]), None, "empty integers");
        assert_eq!(der_signature(b""), None);
    }

    #[test]
    fn assets_must_be_https_when_the_api_is() {
        // pick_release now enforces this itself (and, stricter than before, refuses a plain-http API).
        assert!(pick_release(RELEASE.as_bytes(), API).is_some());
        assert_eq!(pick_release(RELEASE.as_bytes(), "http://127.0.0.1:8000/latest.json"), None, "no plain-http API, even for a local test");
        let plain = RELEASE.replace("https://example.invalid/setup", "http://example.invalid/setup");
        assert_eq!(pick_release(plain.as_bytes(), API), None);
        let sums = RELEASE.replace("https://example.invalid/sums", "HTTP://example.invalid/sums");
        assert_eq!(pick_release(sums.as_bytes(), API), None);
    }

    #[test]
    fn pick_release_without_sums_is_none() {
        let no_sums = RELEASE.replace("SHA256SUMS.txt", "checksums.txt");
        assert_eq!(pick_release(no_sums.as_bytes(), API), None);
        let other_setup = RELEASE.replace("Ferry-Setup-0.2.3.exe", "Ferry-Setup-0.2.2.exe");
        assert_eq!(pick_release(other_setup.as_bytes(), API), None);
        let rc = RELEASE.replace("v0.2.3", "v0.2.3-rc1");
        assert_eq!(pick_release(rc.as_bytes(), API), None);
        assert_eq!(pick_release(b"\xff not json", API), None);
        assert_eq!(pick_release(b"{}", API), None);
    }
}
