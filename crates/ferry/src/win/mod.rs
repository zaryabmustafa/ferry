pub mod copy;
pub mod http;
pub mod net;
pub mod power;
pub mod smb;
pub mod toast;
pub mod tray;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};

/// Take the `Local\Ferry` mutex; false if another engine holds it. The handle stays open
/// for the life of the process.
pub fn single_instance() -> bool {
    unsafe { CreateMutexW(None, true, w!("Local\\Ferry")).is_ok() && GetLastError() != ERROR_ALREADY_EXISTS }
}

/// Let a GUI-subsystem build print to the console it was started from.
pub fn attach_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Hand startup/trip pages back to the OS; idle Ferry then touches almost nothing.
pub fn trim_memory() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}
