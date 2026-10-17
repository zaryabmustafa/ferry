use ferry_core::entry::long_path;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, SetPriorityClass, PROCESS_MODE_BACKGROUND_BEGIN, PROCESS_MODE_BACKGROUND_END,
};

/// Keeps the system awake until dropped. Per-thread state, so it must drop on the creating thread.
pub struct AwakeGuard(PhantomData<*const ()>);

pub fn keep_awake() -> AwakeGuard {
    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
    AwakeGuard(PhantomData)
}

impl Drop for AwakeGuard {
    fn drop(&mut self) {
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// Low CPU/IO priority until dropped. Background mode is process-wide (the `!Send` marker is
/// only for symmetry with `AwakeGuard`). If the process was already in background mode, this
/// guard did not begin it and dropping it leaves the mode to whoever did.
pub struct BackgroundGuard {
    began: bool,
    _not_send: PhantomData<*const ()>,
}

pub fn background_mode() -> BackgroundGuard {
    let began = unsafe { SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN) }.is_ok();
    BackgroundGuard { began, _not_send: PhantomData }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        if self.began {
            let _ = unsafe { SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_END) };
        }
    }
}

pub fn battery_saver_on() -> bool {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.SystemStatusFlag == 1
}

/// Bytes free to the caller on `path`'s volume; 0 on failure (so the budget carries nothing).
pub fn disk_free(path: &Path) -> u64 {
    let w: Vec<u16> = long_path(path).as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    match unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None) } {
        Ok(()) => free,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_is_nonzero_for_temp() {
        assert!(disk_free(&std::env::temp_dir()) > 0);
    }

    /// Process is in background mode iff a fresh BEGIN fails (the probe guard then did not begin it).
    fn in_background() -> bool {
        !background_mode().began
    }

    #[test]
    fn guards_nest_and_drop() {
        let a = keep_awake();
        let b = background_mode();
        let c = background_mode(); // already in background mode: BEGIN fails
        let _ = battery_saver_on();
        assert_eq!((b.began, c.began), (true, false));
        drop(c);
        assert!(in_background(), "inner guard must not end the outer's mode");
        drop(b);
        assert!(!in_background());
        drop(a);
    }
}
