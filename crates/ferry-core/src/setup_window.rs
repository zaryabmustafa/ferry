//! Whether a Settings window is open, from any process: the window holds a named mutex for its
//! lifetime, and the engine asks whether that mutex exists. The window may have been opened by the
//! engine, by the flyout's Settings button or from a shortcut, so child handles can't tell.
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

pub const NAME: &str = "Local\\FerrySetupWindow";

/// The window's hold on the mutex; released when dropped (or when the process ends).
pub struct Held(HANDLE);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub fn hold(name: &str) -> Option<Held> {
    unsafe { CreateMutexW(None, false, &HSTRING::from(name)) }.ok().map(Held)
}

pub fn is_open(name: &str) -> bool {
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) } {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_mutex_reads_as_open_until_dropped() {
        let name = format!("Local\\FerryTestSetupWindow-{}", std::process::id());
        assert!(!is_open(&name));
        let held = hold(&name).expect("create the mutex");
        assert!(is_open(&name));
        drop(held);
        assert!(!is_open(&name));
    }
}
