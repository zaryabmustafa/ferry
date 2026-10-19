use std::io;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_ASSIGNED, ERROR_LOGON_FAILURE, ERROR_NOT_FOUND,
    ERROR_SESSION_CREDENTIAL_CONFLICT, NO_ERROR,
};
use windows::Win32::NetworkManagement::WNet::{
    WNetAddConnection2W, NET_CONNECT_FLAGS, NETRESOURCEW, RESOURCETYPE_ANY, RESOURCETYPE_DISK,
};
use windows::Win32::Security::Credentials::{
    CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// windows::core::Error carries an HRESULT; recover the Win32 code for a proper io::Error.
fn os_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32)
}

/// Zero secret memory in a way the optimizer cannot elide.
fn wipe<T: Default>(buf: &mut [T]) {
    for x in buf {
        unsafe { std::ptr::write_volatile(x, T::default()) };
    }
}

/// Decode the credential's UTF-16 password blob into a NUL-terminated buffer, then wipe the
/// blob in place. A null or empty blob (empty password) yields just the terminator.
///
/// # Safety
/// `c` must come from a successful `CredReadW` and not yet have been freed.
unsafe fn take_password(c: &CREDENTIALW) -> Vec<u16> {
    let mut pw = Vec::new();
    if !c.CredentialBlob.is_null() && c.CredentialBlobSize > 0 {
        let blob = std::slice::from_raw_parts_mut(c.CredentialBlob, c.CredentialBlobSize as usize);
        pw.extend(blob.as_chunks::<2>().0.iter().map(|&b| u16::from_le_bytes(b)));
        wipe(blob);
    }
    pw.push(0);
    pw
}

/// Store `user`/`password` as a generic credential named `target` (UTF-16 password blob).
/// Only Ferry's own `Ferry/...` targets: the pipe must not become a way to overwrite the user's
/// other saved credentials.
pub fn save_credential(target: &str, user: &str, password: &str) -> io::Result<()> {
    if !target.starts_with("Ferry/") || target.len() == "Ferry/".len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Ferry only saves its own credentials (Ferry/...)"));
    }
    if password.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "password contains a NUL character"));
    }
    let (mut t, mut u) = (wide(target), wide(user));
    let mut blob: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(t.as_mut_ptr()),
        UserName: PWSTR(u.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    let r = unsafe { CredWriteW(&cred, 0) };
    wipe(&mut blob);
    r.map_err(os_err)
}

/// Windows requires SMB signing on every connection it makes (`RequireSecuritySignature`). With
/// it, a look-alike server that doesn't know the password can't finish signing in.
pub fn signing_required() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
    let (mut v, mut n) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Services\LanmanWorkstation\Parameters"),
            w!("RequireSecuritySignature"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut v).cast()),
            Some(&mut n),
        )
    };
    r.is_ok() && v == 1
}

/// Open an SMB session to `\\host\share` (no drive letter) with the credential saved under `target`.
pub fn connect(host: &str, share: &str, target: &str) -> io::Result<()> {
    let t = wide(target);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    if let Err(e) = unsafe { CredReadW(PCWSTR(t.as_ptr()), CRED_TYPE_GENERIC, None, &mut p) } {
        let e = os_err(e);
        return Err(if e.raw_os_error() == Some(ERROR_NOT_FOUND.0 as i32) {
            io::Error::new(io::ErrorKind::NotFound, format!("no saved credential named {target}"))
        } else {
            e
        });
    }
    let (user, mut pw) = unsafe { ((*p).UserName.to_string().unwrap_or_default(), take_password(&*p)) };
    unsafe { CredFree(p.cast()) };

    let unc = format!(r"\\{host}\{share}");
    let mut remote = wide(&unc);
    let u = wide(&user);
    let nr = NETRESOURCEW {
        // IPC$ (used to test a sign-in before any folder exists) is not a disk share.
        dwType: if share.eq_ignore_ascii_case("IPC$") { RESOURCETYPE_ANY } else { RESOURCETYPE_DISK },
        lpRemoteName: PWSTR(remote.as_mut_ptr()),
        ..Default::default()
    };
    let rc = unsafe { WNetAddConnection2W(&nr, PCWSTR(pw.as_ptr()), PCWSTR(u.as_ptr()), NET_CONNECT_FLAGS(0)) };
    wipe(&mut pw);
    match rc {
        r if r == NO_ERROR || r == ERROR_ALREADY_ASSIGNED => Ok(()),
        // Windows keeps one sign-in per server: another account's session (File Explorer, a mapped
        // drive) would be used instead of this one, so a test would prove nothing and a trip would
        // copy as someone else.
        r if r == ERROR_SESSION_CREDENTIAL_CONFLICT => Err(io::Error::other(format!(
            r"Windows is already signed in to \\{host} as another user, for example in File Explorer. Ferry won't use that sign-in: disconnect it (net use \\{host} /delete), then try again."
        ))),
        // Signed in, but this account may not use the share: keep the OS code so callers can say so.
        r if r == ERROR_ACCESS_DENIED => Err(io::Error::from_raw_os_error(r.0 as i32)),
        r if r == ERROR_LOGON_FAILURE => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{unc}: {}", io::Error::from_raw_os_error(r.0 as i32)),
        )),
        r => Err(io::Error::from_raw_os_error(r.0 as i32)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    struct Cleanup(Vec<u16>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            unsafe { let _ = CredDeleteW(PCWSTR(self.0.as_ptr()), CRED_TYPE_GENERIC, None); }
        }
    }

    #[test]
    fn credential_round_trip() {
        let target = format!("Ferry/test-{}", std::process::id());
        let wide: Vec<u16> = target.encode_utf16().chain([0]).collect();
        let _cleanup = Cleanup(wide.clone());
        save_credential(&target, "alice", "s3cret").unwrap();
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe { CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None, &mut p).unwrap() };
        let cred = unsafe { &*p };
        let user = unsafe { cred.UserName.to_string().unwrap() };
        let blob = unsafe { std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize) };
        let pw: Vec<u16> = blob.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        let pw = String::from_utf16(&pw).unwrap();
        unsafe { CredFree(p.cast()) };
        assert_eq!(user, "alice");
        assert_eq!(pw, "s3cret");
    }

    #[test]
    fn empty_password_is_decoded_without_touching_a_null_blob() {
        let c = CREDENTIALW::default(); // null blob, size 0
        assert_eq!(unsafe { take_password(&c) }, vec![0]);

        let target = format!("Ferry/test-empty-{}", std::process::id());
        let wide: Vec<u16> = target.encode_utf16().chain([0]).collect();
        let _cleanup = Cleanup(wide.clone());
        save_credential(&target, "alice", "").unwrap();
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe { CredReadW(PCWSTR(wide.as_ptr()), CRED_TYPE_GENERIC, None, &mut p).unwrap() };
        let pw = unsafe { take_password(&*p) };
        unsafe { CredFree(p.cast()) };
        assert_eq!(pw, vec![0]);
    }

    #[test]
    fn password_with_nul_is_rejected() {
        let e = save_credential("Ferry/never-written", "alice", "a\0b").unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn only_ferry_targets_are_written() {
        for t in ["git:https://github.com", "ferry/x", "Ferry/", ""] {
            assert_eq!(save_credential(t, "alice", "pw").unwrap_err().kind(), std::io::ErrorKind::InvalidInput, "{t}");
        }
    }

    #[test]
    fn connect_missing_credential_is_not_found() {
        let e = connect("h", "s", "Ferry/definitely-not-saved").unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(e.to_string().contains("Ferry/definitely-not-saved"));
    }
}
