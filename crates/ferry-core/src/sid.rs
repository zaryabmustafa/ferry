//! Windows user-SID helpers shared by the pipe server (DACL) and client (server check).
use std::io;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

fn err(e: windows::core::Error) -> io::Error {
    io::Error::other(e)
}

/// SID string (`S-1-5-21-...`) of the token's user.
fn token_user_sid(token: HANDLE) -> io::Result<String> {
    unsafe {
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len); // sizing call fails by design
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)]; // 8-aligned for TOKEN_USER
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len).map_err(err)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(err)?;
        let out = s.to_string().map_err(io::Error::other);
        LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

fn process_token_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(err)?;
        let r = token_user_sid(token);
        let _ = CloseHandle(token);
        r
    }
}

pub fn current_user_sid() -> io::Result<String> {
    process_token_sid(unsafe { GetCurrentProcess() })
}

pub fn process_user_sid(pid: u32) -> io::Result<String> {
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(err)?;
        let r = process_token_sid(p);
        let _ = CloseHandle(p);
        r
    }
}

/// Is the process serving this pipe client handle running as the current user?
pub fn pipe_server_is_current_user(pipe: HANDLE) -> io::Result<bool> {
    let mut pid = 0u32;
    unsafe { windows::Win32::System::Pipes::GetNamedPipeServerProcessId(pipe, &mut pid).map_err(err)? };
    Ok(process_user_sid(pid)? == current_user_sid()?)
}
