//! HTTP(S) GET through WinHTTP, and SHA-256 and ECDSA P-256 verification through BCrypt: system
//! DLLs only, no TLS or crypto crate.
use std::ffi::c_void;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts, WINHTTP_INTERNET_SCHEME_HTTPS, URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::Security::Cryptography::{
    BCryptDestroyKey, BCryptHash, BCryptImportKeyPair, BCryptVerifySignature, BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALG_HANDLE,
    BCRYPT_ECDSA_PUBLIC_P256_MAGIC, BCRYPT_FLAGS, BCRYPT_KEY_HANDLE, BCRYPT_SHA256_ALG_HANDLE,
};

#[derive(Debug)]
pub enum HttpError {
    /// The server answered with this status instead of 200.
    Status(u32),
    Other(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Status(s) => write!(f, "the server answered {s}"),
            HttpError::Other(e) => f.write_str(e),
        }
    }
}

impl From<windows::core::Error> for HttpError {
    fn from(e: windows::core::Error) -> Self {
        HttpError::Other(e.message())
    }
}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        HttpError::Other(e.to_string())
    }
}

/// A WinHTTP handle, closed on drop.
struct H(*mut c_void);

impl H {
    fn new(h: *mut c_void) -> Result<Self, HttpError> {
        if h.is_null() {
            Err(windows::core::Error::from_win32().into())
        } else {
            Ok(H(h))
        }
    }
}

impl Drop for H {
    fn drop(&mut self) {
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// GETs `url` (following redirects) and hands the body to `sink` chunk by chunk.
fn fetch(url: &str, timeout_ms: u32, sink: &mut dyn FnMut(&[u8]) -> Result<(), HttpError>) -> Result<(), HttpError> {
    let wide: Vec<u16> = url.encode_utf16().collect();
    let (mut host, mut path, mut extra) = (vec![0u16; 256], vec![0u16; 2048], vec![0u16; 2048]);
    let mut uc = URL_COMPONENTS {
        dwStructSize: size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: PWSTR(host.as_mut_ptr()),
        dwHostNameLength: host.len() as u32,
        lpszUrlPath: PWSTR(path.as_mut_ptr()),
        dwUrlPathLength: path.len() as u32,
        lpszExtraInfo: PWSTR(extra.as_mut_ptr()),
        dwExtraInfoLength: extra.len() as u32,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&wide, 0, &mut uc) }.map_err(|e| HttpError::Other(format!("{url} is not a valid address: {}", e.message())))?;
    let host = String::from_utf16_lossy(&host[..uc.dwHostNameLength as usize]);
    // The object name is the path plus the "?query" (the extra info, minus any "#fragment").
    let extra = String::from_utf16_lossy(&extra[..uc.dwExtraInfoLength as usize]);
    let path = String::from_utf16_lossy(&path[..uc.dwUrlPathLength as usize]) + extra.split('#').next().unwrap_or("");
    let secure = uc.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;
    let t = timeout_ms as i32;
    unsafe {
        let agent = HSTRING::from(concat!("Ferry/", env!("CARGO_PKG_VERSION")));
        let session = H::new(WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0))?;
        WinHttpSetTimeouts(session.0, t, t, t, t)?;
        let conn = H::new(WinHttpConnect(session.0, &HSTRING::from(host), uc.nPort, 0))?;
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let req = H::new(WinHttpOpenRequest(conn.0, &HSTRING::from("GET"), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags))?;
        WinHttpSendRequest(req.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut())?;
        let (mut status, mut len) = (0u32, 4u32);
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            std::ptr::null_mut(),
        )?;
        if status != 200 {
            return Err(HttpError::Status(status));
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read)?;
            if read == 0 {
                return Ok(());
            }
            sink(&buf[..read as usize])?;
        }
    }
}

/// The body of `url`, refused when longer than `cap` bytes.
pub fn get(url: &str, cap: usize, timeout_ms: u32) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    fetch(url, timeout_ms, &mut |b| {
        if body.len() + b.len() > cap {
            return Err(HttpError::Other(format!("the answer from {url} is larger than {cap} bytes")));
        }
        body.extend_from_slice(b);
        Ok(())
    })?;
    Ok(body)
}

/// Saves `url` to `dest` through `dest.part`, refused when longer than `cap` bytes.
pub fn download(url: &str, dest: &Path, cap: u64) -> Result<(), HttpError> {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut f = File::create(&part)?;
    let mut n = 0u64;
    let r = fetch(url, 60_000, &mut |b| {
        n += b.len() as u64;
        if n > cap {
            return Err(HttpError::Other(format!("{url} is larger than {cap} bytes")));
        }
        Ok(f.write_all(b)?)
    })
    .and_then(|()| Ok(f.sync_all()?));
    drop(f);
    match r {
        Ok(()) => Ok(fs::rename(&part, dest)?),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// SHA-256 of a file, lowercase hex. Read whole: updates are capped at 64 MiB.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut out = [0u8; 32];
    let s = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, &fs::read(path)?, &mut out) };
    if s.is_err() {
        return Err(io::Error::other(format!("BCryptHash: 0x{:08x}", s.0)));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// A BCrypt key, destroyed on drop.
pub(crate) struct Key(pub(crate) BCRYPT_KEY_HANDLE);

impl Drop for Key {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { BCryptDestroyKey(self.0) };
        }
    }
}

/// Whether `sig` (r‖s) is the ECDSA P-256 signature, by the public key `point` (X‖Y), of the
/// SHA-256 of `msg`.
pub fn verify_p256(point: &[u8; 64], msg: &[u8], sig: &[u8; 64]) -> bool {
    let mut blob = Vec::with_capacity(72);
    blob.extend(BCRYPT_ECDSA_PUBLIC_P256_MAGIC.to_le_bytes());
    blob.extend(32u32.to_le_bytes());
    blob.extend(point);
    let mut key = Key(BCRYPT_KEY_HANDLE::default());
    let mut hash = [0u8; 32];
    unsafe {
        BCryptImportKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, None, BCRYPT_ECCPUBLIC_BLOB, &mut key.0, &blob, 0).is_ok()
            && BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, msg, &mut hash).is_ok()
            && BCryptVerifySignature(key.0, None, &hash, sig, BCRYPT_FLAGS(0)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot HTTP server on 127.0.0.1 answering the first request with `status` and `body`.
    fn serve(status: &str, body: &'static [u8]) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
        });
        format!("http://127.0.0.1:{port}/latest.json")
    }

    #[test]
    fn http_get_reads_body() {
        assert_eq!(get(&serve("200 OK", b"hello"), 1024, 5000).unwrap(), b"hello");
    }

    #[test]
    fn http_get_keeps_query_string() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            // Answer with the request target, e.g. "/releases?limit=1".
            let target = String::from_utf8_lossy(&buf[..n]).split(' ').nth(1).unwrap_or("").to_string();
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{target}", target.len());
        });
        let url = format!("http://127.0.0.1:{port}/releases?limit=1&draft=false#notes");
        assert_eq!(get(&url, 1024, 5000).unwrap(), b"/releases?limit=1&draft=false");
    }

    #[test]
    fn http_get_404_reports_status() {
        assert!(matches!(get(&serve("404 Not Found", b"nope"), 1024, 5000), Err(HttpError::Status(404))));
    }

    #[test]
    fn http_get_over_cap_fails() {
        assert!(matches!(get(&serve("200 OK", b"0123456789"), 4, 5000), Err(HttpError::Other(_))));
    }

    #[test]
    fn download_writes_file_and_no_part_left() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Ferry-Setup-0.2.3.exe");
        download(&serve("200 OK", b"installer bytes"), &dest, 1024).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"installer bytes");
        assert!(!dir.path().join("Ferry-Setup-0.2.3.exe.part").exists());
    }

    #[test]
    fn sha256_of_abc() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("abc");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(sha256_file(&f).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
