//! Tray icon, hidden window, menu, timers and the Win32 message loop (main thread only).
use std::cell::RefCell;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::RegisterPowerSettingNotification;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD,
};
use windows::Win32::System::SystemServices::GUID_POWER_SAVING_STATUS;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, DEVICE_NOTIFY_WINDOW_HANDLE, HICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
    WM_DPICHANGED, WM_POWERBROADCAST, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, SM_CXSMICON, WNDCLASSW, WS_OVERLAPPED,
};

const WM_WAKE: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Idle,
    Filling,
    Unloading,
    Attention,
    Paused,
}

/// Brand tray art per `Icon` slot: [light taskbar (dark glyph), dark taskbar].
const TRAY_ICOS: [[&[u8]; 2]; 4] = [
    [include_bytes!("../../../../assets/brand/tray-idle-light.ico"), include_bytes!("../../../../assets/brand/tray-idle-dark.ico")],
    [include_bytes!("../../../../assets/brand/tray-carrying-light.ico"), include_bytes!("../../../../assets/brand/tray-carrying-dark.ico")],
    [include_bytes!("../../../../assets/brand/tray-attention-light.ico"), include_bytes!("../../../../assets/brand/tray-attention-dark.ico")],
    [include_bytes!("../../../../assets/brand/tray-paused-light.ico"), include_bytes!("../../../../assets/brand/tray-paused-dark.ico")],
];

impl Icon {
    fn slot(self) -> usize {
        match self {
            Icon::Idle => 0,
            Icon::Filling | Icon::Unloading => 1,
            Icon::Attention => 2,
            Icon::Paused => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCmd {
    Open,
    Pause1h,
    PauseForever,
    Resume,
    Quit,
}

/// What the message loop hands to the engine.
pub enum Ev {
    Wake,
    Timer(usize),
    Click,
    Menu(MenuCmd),
    PowerChanged,
    /// The machine woke from sleep.
    Resumed,
}

type Handler = Box<dyn FnMut(Ev)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    static ICONS: RefCell<Vec<HICON>> = const { RefCell::new(Vec::new()) };
    /// Current icon and tooltip, re-added when Explorer restarts.
    static SHOWN: RefCell<(Icon, String)> = RefCell::new((Icon::Idle, "Ferry".into()));
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Hidden top-level window (not message-only: those miss the TaskbarCreated broadcast) plus the icon.
pub fn create() -> io::Result<HWND> {
    unsafe {
        // Crisp icons at any scale: SM_CXSMICON is only real for a DPI-aware window.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("FerryTray"),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("FerryTray"),
            w!("Ferry"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        load_icons(hwnd, NIM_ADD);
        // Battery Saver on/off arrives as WM_POWERBROADCAST.
        let _ = RegisterPowerSettingNotification(HANDLE(hwnd.0), &GUID_POWER_SAVING_STATUS, DEVICE_NOTIFY_WINDOW_HANDLE);
        Ok(hwnd)
    }
}

/// Pick the tray ICOs for the taskbar theme and DPI, show them with `op`, then free the old ones.
fn load_icons(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) {
    let dark = usize::from(!light_taskbar());
    let px = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(hwnd)) }.max(16);
    let icons = TRAY_ICOS
        .iter()
        .map(|pair| {
            frame(pair[dark], px)
                .and_then(|f| unsafe { CreateIconFromResourceEx(f, true, 0x0003_0000, px, px, LR_DEFAULTCOLOR) }.ok())
                .unwrap_or_default()
        })
        .collect();
    let old = ICONS.with(|i| std::mem::replace(&mut *i.borrow_mut(), icons));
    notify(hwnd, op);
    for h in old {
        unsafe {
            let _ = DestroyIcon(h);
        }
    }
}

/// `SystemUsesLightTheme` = 1 means a light taskbar; missing means dark.
fn light_taskbar() -> bool {
    let (mut v, mut len) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut len),
        )
    };
    r.is_ok() && v == 1
}

/// Image bytes of the smallest frame at least `px` wide (else the largest) in an .ico file.
fn frame(ico: &[u8], px: i32) -> Option<&[u8]> {
    if ico.get(..4)? != [0, 0, 1, 0] {
        return None;
    }
    let at = |o: usize, n: usize| ico.get(o..o + n).map(|b| b.iter().rev().fold(0usize, |a, &x| a << 8 | x as usize));
    let mut frames = Vec::new();
    for e in (0..at(4, 2)?).map(|i| 6 + 16 * i) {
        let w = match at(e, 1)? { 0 => 256, w => w as i32 };
        let (len, off) = (at(e + 8, 4)?, at(e + 12, 4)?);
        frames.push((w, ico.get(off..off.checked_add(len)?)?));
    }
    frames.sort_by_key(|f| f.0);
    frames.iter().find(|f| f.0 >= px).or(frames.last()).map(|f| f.1)
}

fn notify(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) {
    let (icon, tip) = SHOWN.with(|s| s.borrow().clone());
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: ICONS.with(|i| i.borrow().get(icon.slot()).copied().unwrap_or_default()),
        ..Default::default()
    };
    for (d, s) in nid.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *d = s;
    }
    unsafe {
        let _ = Shell_NotifyIconW(op, &nid);
    }
}

/// Change the icon and tooltip; a no-op when nothing changed.
pub fn set_icon(hwnd: HWND, icon: Icon, tip: &str) {
    let changed = SHOWN.with(|s| {
        let mut s = s.borrow_mut();
        if s.0 == icon && s.1 == tip {
            return false;
        }
        *s = (icon, tip.to_string());
        true
    });
    if changed {
        notify(hwnd, NIM_MODIFY);
    }
}

/// Wake the message loop from any thread (`hwnd` as usize, since HWND is not Send).
pub fn wake(hwnd: usize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Arm a periodic window timer; re-arming an existing id resets its countdown.
pub fn set_timer(hwnd: HWND, id: usize, ms: u64) {
    unsafe { SetTimer(Some(hwnd), id, ms.min(0x7fff_ffff) as u32, None) };
}

pub fn kill_timer(hwnd: HWND, id: usize) {
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

/// Remove the icon and end the message loop.
pub fn quit(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Run the message loop, feeding events to `handler`, until `quit`.
pub fn run(handler: impl FnMut(Ev) + 'static) {
    HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    HANDLER.with(|h| h.borrow_mut().take());
}

/// False if the handler is already running (re-entered from a nested message loop).
fn dispatch(ev: Ev) -> bool {
    HANDLER.with(|h| match h.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(f) = g.as_mut() {
                f(ev);
            }
            true
        }
        Err(_) => false,
    })
}

fn menu(hwnd: HWND) -> Option<MenuCmd> {
    let items: [(MenuCmd, PCWSTR); 5] = [
        (MenuCmd::Open, w!("Open Ferry")),
        (MenuCmd::Pause1h, w!("Pause for 1 hour")),
        (MenuCmd::PauseForever, w!("Pause until I resume")),
        (MenuCmd::Resume, w!("Resume")),
        (MenuCmd::Quit, w!("Quit")),
    ];
    unsafe {
        let m = CreatePopupMenu().ok()?;
        for (i, (cmd, text)) in items.iter().enumerate() {
            if *cmd == MenuCmd::Quit {
                let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
            }
            let _ = AppendMenuW(m, MF_STRING, i + 1, *text);
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        items.get((picked.0 as usize).wrapping_sub(1)).map(|(c, _)| *c)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_WAKE => {
            if !dispatch(Ev::Wake) {
                wake(hwnd.0 as usize); // try again once the handler returns
            }
        }
        WM_TIMER => {
            dispatch(Ev::Timer(wp.0)); // a missed tick fires again next period
        }
        WM_TRAY => match lp.0 as u32 {
            WM_LBUTTONUP => {
                dispatch(Ev::Click);
            }
            WM_RBUTTONUP => {
                if let Some(c) = menu(hwnd) {
                    dispatch(Ev::Menu(c));
                }
            }
            _ => {}
        },
        WM_POWERBROADCAST => {
            if wp.0 == PBT_POWERSETTINGCHANGE as usize {
                dispatch(Ev::PowerChanged);
            } else if wp.0 == PBT_APMRESUMEAUTOMATIC as usize {
                dispatch(Ev::Resumed);
            }
            return LRESULT(1);
        }
        WM_DESTROY => {
            notify(hwnd, NIM_DELETE);
            unsafe { PostQuitMessage(0) };
        }
        WM_SETTINGCHANGE if lp.0 != 0 && unsafe { PCWSTR(lp.0 as *const u16).to_string() }.is_ok_and(|s| s == "ImmersiveColorSet") => {
            load_icons(hwnd, NIM_MODIFY);
        }
        WM_DPICHANGED => load_icons(hwnd, NIM_MODIFY),
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => load_icons(hwnd, NIM_ADD),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Ferry` = this exe, quoted.
pub fn set_autostart(on: bool) -> io::Result<()> {
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    unsafe {
        if on {
            let exe = format!("\"{}\"", std::env::current_exe()?.display());
            let v: Vec<u16> = exe.encode_utf16().chain([0]).collect();
            RegSetKeyValueW(HKEY_CURRENT_USER, key, w!("Ferry"), REG_SZ.0, Some(v.as_ptr().cast()), (v.len() * 2) as u32)
                .ok()?;
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, w!("Ferry"));
            if r != ERROR_FILE_NOT_FOUND {
                r.ok()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_brand_ico_has_a_16px_frame_windows_can_load() {
        let app: &[u8] = include_bytes!("../../../../assets/brand/ferry.ico");
        for ico in TRAY_ICOS.iter().flatten().chain([&app]) {
            let f = frame(ico, 16).expect("ico parses");
            assert!(frame(ico, 1) == Some(f), "smallest frame is 16px");
            let h = unsafe { CreateIconFromResourceEx(f, true, 0x0003_0000, 16, 16, LR_DEFAULTCOLOR) }.expect("frame loads");
            unsafe { DestroyIcon(h) }.unwrap();
        }
    }
}
