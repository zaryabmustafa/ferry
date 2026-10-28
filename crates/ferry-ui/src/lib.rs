//! The windows, run inside `ferry.exe` as their own short-lived process so the engine never loads
//! the renderer: `ferry --flyout` is the tray panel, `ferry --setup` the settings window and first run.
slint::include_modules!();

mod setup;

use ferry_core::config::{display_name, Config};
use ferry_core::ipc::{self, PairStatus, Phase, Request, Response, Side, Status};
use ferry_core::location::Location;
use slint::winit_030::{winit, EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Runs the window `arg` names (`--flyout` or `--setup`) and returns the process exit code.
pub fn run(arg: &str) -> i32 {
    let r = match arg {
        "--flyout" => flyout(),
        "--setup" => setup::run(),
        _ => return 2,
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            log_error(&arg[2..], &e.to_string());
            1
        }
    }
}

/// stderr is invisible for a windows-subsystem exe started from the tray, so also append to
/// `%LOCALAPPDATA%/Ferry/ui-error.log`.
fn log_error(window: &str, msg: &str) {
    use std::io::Write;
    eprintln!("ferry: {msg}");
    let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Ferry")) else {
        return;
    };
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = std::fs::create_dir_all(&dir).and_then(|()| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("ui-error.log"))?;
        writeln!(f, "{secs} {window}: {msg}")
    });
}

// ---------- copy (pure) ----------

/// Sizes count in 1024s and say MB and GB, as Windows Explorer does.
const GIB: f64 = (1u64 << 30) as f64;

/// GiB with one decimal, ".0" dropped: 38.2, 50.
fn gb(bytes: u64) -> String {
    let s = format!("{:.1}", bytes as f64 / GIB);
    s.strip_suffix(".0").map_or(s.clone(), str::to_string)
}

fn size(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else if bytes >= 1 << 30 {
        format!("{} GB", gb(bytes))
    } else {
        format!("{} MB", bytes >> 20)
    }
}

/// "38.2 of 50 GB · 108 MB/s · about 2 min left"; parts that are 0 or None are left out.
fn detail_line(carried: u64, cap: u64, rate_bps: u64, eta_secs: Option<u64>) -> String {
    let mut parts = vec![];
    if cap > 0 {
        parts.push(format!("{} of {} GB", gb(carried), gb(cap)));
    }
    if rate_bps >> 20 > 0 {
        parts.push(format!("{} MB/s", rate_bps >> 20));
    }
    if let Some(s) = eta_secs.filter(|&s| s > 0) {
        parts.push(format!("about {} min left", s.div_ceil(60)));
    }
    parts.join(" · ")
}

/// What the user calls each side (`config::display_name`).
pub struct Names {
    pub src: String,
    pub dest: String,
}

impl Names {
    pub fn of(cfg: &Config) -> Names {
        Names { src: display_name(cfg, Side::Source), dest: display_name(cfg, Side::Destination) }
    }

    /// "At {name}" for a side, `None` elsewhere.
    fn at(&self, location: Location) -> Option<String> {
        match location {
            Location::Source => Some(format!("At {}", self.src)),
            Location::Destination => Some(format!("At {}", self.dest)),
            Location::Elsewhere => None,
        }
    }
}

fn status_line(phase: Phase, paused_until: Option<i64>, n: &Names) -> String {
    match phase {
        Phase::Filling => format!("Loading at {}", n.src),
        Phase::Unloading => format!("Unloading at {}", n.dest),
        Phase::UpToDate => format!("{} is up to date", n.dest),
        Phase::Waiting => "Waiting: not on a known network".into(),
        Phase::Idle if paused_until.is_some() => "Paused".into(),
        Phase::Idle => "Idle".into(),
    }
}

/// The status line. Between trips it says what the laptop is for right now: cargo to take to the
/// destination, or, at the destination with nothing carried, what is still left (I5).
fn status_text(s: &Status, n: &Names) -> String {
    if let Some(place) = blocked_place(s, n) {
        return format!("{place}. Not copying");
    }
    if s.paused_until.is_some() || !matches!(s.phase, Phase::Idle | Phase::Waiting) {
        return status_line(s.phase, s.paused_until, n);
    }
    match s.location {
        Location::Elsewhere if s.carried_bytes > 0 => format!("Carrying {} to {}", size(s.carried_bytes), n.dest),
        // "Loaded" only when (nearly) full: a fill that stopped early may well start again here.
        Location::Source if s.carried_bytes > 0 && s.carried_bytes >= s.cap_bytes / 10 * 9 => format!("Loaded. Take the laptop to {}", n.dest),
        Location::Source if s.carried_bytes > 0 => format!("{} on board for {}", size(s.carried_bytes), n.dest),
        Location::Destination if s.carried_bytes == 0 => destination_line(s, n).unwrap_or_else(|| status_line(s.phase, None, n)),
        _ => status_line(s.phase, None, n),
    }
}

/// What is left at the destination once the laptop is empty, if anything.
fn destination_line(s: &Status, n: &Names) -> Option<String> {
    if s.pairs.iter().any(|p| p.needs_confirm) {
        return Some(format!("Deletes at {} need your OK", n.dest));
    }
    if s.pairs.iter().any(|p| !p.up_to_date) {
        return Some(match caught_up(s) {
            Some((pct, _, _)) => format!("{} is {pct}% caught up", n.dest),
            None => format!("{} isn't fully up to date yet", n.dest),
        });
    }
    s.trips_behind.filter(|&t| t > 0).map(|t| trips_line(Some(t), n))
}

/// What a pair still needs at the destination, from its source size and its share there.
fn left_to_bring(p: &PairStatus) -> u64 {
    p.source_bytes / 100 * u64::from(100 - p.destination_percent.min(100))
}

/// (percent, bytes at the destination, bytes at the source) over the pairs, weighted by size. None
/// while a folder that isn't up to date has no listing yet (its size is unknown, so any percent
/// would overstate), and when no data is missing (only deletes wait). Never rounds up to 100.
fn caught_up(s: &Status) -> Option<(u8, u64, u64)> {
    if s.pairs.iter().any(|p| !p.up_to_date && p.source_bytes == 0) {
        return None;
    }
    let total: u64 = s.pairs.iter().map(|p| p.source_bytes).sum();
    let have = total - s.pairs.iter().map(left_to_bring).sum::<u64>();
    (have < total).then(|| (((u128::from(have) * 100 / u128::from(total)) as u8).min(99), have, total))
}

/// The flyout's line under the status before problems take it over (`detail_text`). The amount on
/// the laptop is the meter's label, so this says what that amount is for.
fn usual_detail(s: &Status, n: &Names) -> String {
    let speed = detail_line(0, 0, s.rate_bps, s.eta_secs);
    if s.phase == Phase::Unloading && s.trip_bytes > 0 {
        let done = format!("{} of {} GB unloaded", gb(s.trip_bytes.saturating_sub(s.carried_bytes)), gb(s.trip_bytes));
        return [done, speed].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
    }
    if s.paused_until.is_some() || !matches!(s.phase, Phase::Idle | Phase::Waiting) {
        return speed;
    }
    match s.location {
        Location::Source | Location::Elsewhere if s.carried_bytes > 0 => format!("Unloading starts when the laptop is at {}", n.dest),
        Location::Destination if s.carried_bytes == 0 && s.pairs.iter().any(|p| !p.up_to_date && !p.needs_confirm) => {
            match caught_up(s).map(|(_, have, total)| total - have) {
                // Only deletes wait, or a size isn't known yet: nothing to promise about trips.
                None => speed,
                Some(left) if left <= s.cap_bytes => format!("Laptop is empty; the next trip from {} brings the last {}", n.src, size(left)),
                Some(left) => format!("Laptop is empty; {} still to bring from {}", size(left), n.src),
            }
        }
        _ => speed,
    }
}

/// "At {name}" when Ferry is at a side but won't copy (spec 4.5); a pause or a running trip says
/// so instead.
fn blocked_place(s: &Status, n: &Names) -> Option<String> {
    if s.bulk_blocked.is_none() || s.paused_until.is_some() || matches!(s.phase, Phase::Filling | Phase::Unloading) {
        return None;
    }
    n.at(s.location)
}

/// What the last trip couldn't do at the other server's remote address, where that matters now (I6).
fn remote_line(location: Location, ok: bool, n: &Names) -> Option<String> {
    match (location, ok) {
        (_, true) | (Location::Elsewhere, _) => None,
        (Location::Source, false) => Some(format!("Can't reach {} from here; nothing was carried", n.dest)),
        (Location::Destination, false) => Some(format!("Can't reach {} from here; deletes at {} wait", n.src, n.dest)),
    }
}

/// The line under the status: why copies are held back, else remote trouble, else `usual`.
fn detail_text(s: &Status, usual: String, n: &Names) -> String {
    match (&s.bulk_blocked, s.location) {
        (Some(why), Location::Source | Location::Destination) => why.clone(),
        _ => remote_line(s.location, s.remote_ok, n).unwrap_or(usual),
    }
}

fn trust_note(s: &Status) -> &'static str {
    if s.trusting_vpn { "Copying through a VPN you trust on this network" } else { "" }
}

fn trips_line(trips_behind: Option<u32>, n: &Names) -> String {
    let dest = &n.dest;
    match trips_behind {
        None => String::new(),
        Some(0) => format!("{dest} is caught up"),
        Some(1) => format!("{dest} is about 1 trip behind"),
        Some(t) => format!("{dest} is about {t} trips behind"),
    }
}

/// What one Status poll found.
enum Poll {
    Status(Box<Status>),
    /// No pipe: the engine isn't running.
    Down,
    /// The engine answered with an error, or something unreadable.
    Failed(String),
    /// No answer within the timeout.
    Hung,
}

fn classify(r: std::io::Result<Response>) -> Poll {
    match r {
        Ok(Response::Status(s)) => Poll::Status(Box::new(s)),
        Ok(Response::Err(e)) => Poll::Failed(e),
        Ok(other) => Poll::Failed(format!("Unexpected reply: {other:?}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Poll::Down,
        Err(e) => Poll::Failed(e.to_string()),
    }
}

/// (status line, detail line, engine is up) for every poll outcome except a good Status.
fn problem_copy(p: &Poll) -> (&'static str, String, bool) {
    match p {
        Poll::Down => ("Ferry isn't running", String::new(), false),
        Poll::Failed(e) => ("Ferry couldn't read its status", e.clone(), true),
        Poll::Hung | Poll::Status(_) => ("Ferry isn't responding", String::new(), true),
    }
}

/// Runs `f` on its own thread. If it takes longer than `limit`, calls `on_timeout` and keeps waiting.
fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static, limit: Duration, on_timeout: impl FnOnce()) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok().or_else(|| {
        on_timeout();
        rx.recv().ok()
    })
}

const ROW_TEXT: i32 = 0;
const ROW_BAR: i32 = 1;
const ROW_DONE: i32 = 2;
const ROW_ATTENTION: i32 = 3;

/// (detail, right-hand text, kind) for one folder row.
fn folder_row(p: &PairStatus, n: &Names) -> (String, String, i32) {
    if p.needs_confirm {
        return ("Needs your OK to delete files".into(), size(p.carried_bytes), ROW_ATTENTION);
    }
    if p.up_to_date {
        return ("Up to date".into(), String::new(), ROW_DONE);
    }
    let mut parts = vec![];
    if p.new_carried > 0 {
        parts.push(format!("{} new carried", p.new_carried));
    }
    if p.removed_at_destination > 0 {
        parts.push(format!("{} removed at {}", p.removed_at_destination, n.dest));
    }
    if parts.is_empty() {
        (String::new(), format!("{}% at {}", p.destination_percent, n.dest), ROW_BAR)
    } else {
        (parts.join(" · "), size(p.carried_bytes), ROW_TEXT)
    }
}

/// The flyout's line under a folder and whether the running trip is copying it now. Cargo is said
/// as an amount on board; a folder with none says what it still needs at the destination.
fn flyout_line(s: &Status, p: &PairStatus, kind: i32, detail: &str, n: &Names) -> (String, bool) {
    if kind == ROW_ATTENTION || kind == ROW_DONE {
        return (detail.into(), false);
    }
    let now = p.carried_bytes > 0 && s.active_pair == p.id && matches!(s.phase, Phase::Filling | Phase::Unloading);
    let held = size(p.carried_bytes);
    let mut line = match s.phase {
        _ if p.carried_bytes == 0 => match left_to_bring(p) {
            0 => format!("{}% at {}", p.destination_percent, n.dest),
            left => format!("{} still to bring", size(left)),
        },
        Phase::Filling if now => format!("Loading · {held} on board"),
        Phase::Unloading if now => format!("Unloading · {held} left"),
        _ => format!("{held} on board"),
    };
    // Deletions stay visible (PRODUCT.md: never surprise with data loss).
    if p.removed_at_destination > 0 {
        line += &format!(" · {} removed at {}", p.removed_at_destination, n.dest);
    }
    (line, now)
}

// ---------- theme ----------

/// DWM stores the accent as 0xAABBGGRR.
fn abgr_rgb(v: u32) -> (u8, u8, u8) {
    (v as u8, (v >> 8) as u8, (v >> 16) as u8)
}

type Rgb = (u8, u8, u8);

/// WCAG relative luminance.
fn luminance((r, g, b): Rgb) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `t` of `a`, the rest `b`, per sRGB channel (what Slint's `mix` does).
fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let m = |x: u8, y: u8| (f64::from(x) * t + f64::from(y) * (1.0 - t)).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// `fg`, moved toward black or white (away from `bg`) just far enough to reach `ratio` against it.
fn with_contrast(fg: Rgb, bg: Rgb, ratio: f64) -> Rgb {
    let pole = if luminance(bg) > 0.18 { (0, 0, 0) } else { (255, 255, 255) };
    (0..=20).map(|i| mix(fg, pole, 1.0 - f64::from(i) / 20.0)).find(|&c| contrast(c, bg) >= ratio).unwrap_or(pole)
}

/// Meter fills (accent, seg-2, seg-3), each at least 3:1 against `track` (WCAG 1.4.11). The two
/// shades sit between the fixed accent and `ink`, which is on the same side of the track, so they
/// can't fall below the accent's contrast.
fn meter_colors(accent: Rgb, ink: Rgb, track: Rgb) -> [Rgb; 3] {
    let a = with_contrast(accent, track, 3.0);
    [a, mix(a, ink, 0.6), mix(a, ink, 0.3)]
}

/// No real hue: the channels are within 24 of each other (gray, black, white, slate).
fn neutral((r, g, b): Rgb) -> bool {
    r.max(g).max(b) - r.min(g).min(b) < 24
}

/// Dark text on this fill? White wins whenever it reaches WCAG AA (4.5:1), as Windows does.
fn dark_ink_on(fill: Rgb) -> bool {
    contrast((255, 255, 255), fill) < 4.5
}

fn color((r, g, b): Rgb) -> slint::Color {
    slint::Color::from_rgb_u8(r, g, b)
}

fn rgb(c: slint::Color) -> Rgb {
    (c.red(), c.green(), c.blue())
}

fn apply_theme(t: &Theme) {
    use windows::core::w;
    // Windows 10 has no Segoe UI Variable.
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:/Windows".into());
    if !std::path::Path::new(&windir).join("Fonts").join("SegUIVar.ttf").exists() {
        t.set_font("Segoe UI".into());
        t.set_font_display("Segoe UI".into());
    }

    if win::high_contrast() {
        use windows::Win32::Graphics::Gdi::{
            COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT,
        };
        let sys = |i| color(abgr_rgb(win::sys_color(i)));
        let (window, text, hilite) = (sys(COLOR_WINDOW), sys(COLOR_WINDOWTEXT), sys(COLOR_HIGHLIGHT));
        t.set_hc(true);
        // Light or dark by the HC background, so std widgets (scrollbars) pick a matching palette.
        t.set_dark(luminance(rgb(window)) < 0.18);
        for set in [Theme::set_mica, Theme::set_layer, Theme::set_layer_2, Theme::set_accent_soft] {
            set(t, window);
        }
        t.set_subtle(sys(COLOR_BTNFACE));
        // Borders and status colors use the text color: it's the one color guaranteed to show on Window.
        for set in [Theme::set_stroke, Theme::set_ink, Theme::set_ok, Theme::set_warn, Theme::set_landed] {
            set(t, text);
        }
        t.set_freed(sys(COLOR_BTNFACE));
        t.set_muted(sys(COLOR_GRAYTEXT));
        for set in [Theme::set_accent, Theme::set_seg_2, Theme::set_seg_3] {
            set(t, hilite);
        }
        t.set_accent_ink(sys(COLOR_HIGHLIGHTTEXT));
        return;
    }

    let light = win::reg_dword(w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"), w!("AppsUseLightTheme"));
    // Debug builds: FERRY_UI_THEME=light|dark overrides Windows, for screenshots of both themes.
    let forced = if cfg!(debug_assertions) { std::env::var("FERRY_UI_THEME").ok() } else { None };
    let dark = match forced.as_deref() {
        Some("light") => false,
        Some("dark") => true,
        _ => light == Some(0),
    };
    t.set_dark(dark);
    // Like WinUI: the accent's Dark1 shade in light mode and Light2 in dark mode, so it reads on the
    // surface either way; the raw AccentColor only when there is no palette.
    let palette = || {
        let p = win::reg_bytes::<32>(w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent"), w!("AccentPalette"))?;
        let i = if dark { 4 } else { 16 };
        Some((p[i], p[i + 1], p[i + 2]))
    };
    let dwm = || win::reg_dword(w!(r"Software\Microsoft\Windows\DWM"), w!("AccentColor")).map(abgr_rgb);
    // A gray Windows accent can't mark the primary action (it reads as disabled), so it falls back
    // to the brand amber like a missing one.
    let accent = palette().or_else(dwm).filter(|&c| !neutral(c)).unwrap_or_else(|| rgb(t.get_accent()));
    let [a, s2, s3] = meter_colors(accent, rgb(t.get_ink()), rgb(t.get_subtle()));
    t.set_accent(color(a));
    t.set_seg_2(color(s2));
    t.set_seg_3(color(s3));
    let ink = if dark_ink_on(a) { 0x1a } else { 0xff };
    t.set_accent_ink(color((ink, ink, ink)));
}

// ---------- flyout ----------

fn flyout() -> Result<(), slint::PlatformError> {
    use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|a| {
            a.with_skip_taskbar(true)
                .with_corner_preference(CornerPreference::Round)
                .with_resizable(false)
                // Off screen until place() puts it by the tray.
                .with_position(winit::dpi::PhysicalPosition::new(-30000, -30000))
        })
        .select()?;

    let ui = Flyout::new()?;
    apply_theme(&ui.global::<Theme>());
    // The flyout lives for one click, so the names are read once.
    let names = Arc::new(Names::of(&setup::load().unwrap_or_default()));

    let focused = std::rc::Rc::new(std::cell::Cell::new(false));
    let weak = ui.as_weak();
    let seen = focused.clone();
    ui.window().on_winit_window_event(move |_, ev| {
        match ev {
            winit::event::WindowEvent::Focused(true) => seen.set(true),
            winit::event::WindowEvent::Focused(false) => {
                let _ = slint::quit_event_loop();
            }
            // Place once Slint has handled the pending resizes: this event can be a stale
            // creation size (1920x1010), and Slint updates window().size() only after us.
            winit::event::WindowEvent::Resized(_) => {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        place(ui.window());
                    }
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });

    let busy = Arc::new(AtomicBool::new(false));
    let refresh = {
        let weak = ui.as_weak();
        move || {
            let (weak, names) = (weak.clone(), names.clone());
            poll(&busy, move |p| {
                let names = names.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| match p {
                    Poll::Status(s) => show(&ui, &s, &names),
                    p => show_problem(&ui, &p),
                });
            });
        }
    };

    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });
    ui.on_settings(|| {
        let _ = ferry(&["--setup"]);
        let _ = slint::quit_event_loop();
    });
    ui.on_start_engine({
        let refresh = refresh.clone();
        move || {
            let _ = ferry(&[]);
            // The engine needs a moment to open its pipe; the 1 s poll picks it up, this just hurries it.
            slint::Timer::single_shot(Duration::from_millis(300), refresh.clone());
        }
    });
    ui.on_toggle_pause({
        let (weak, refresh) = (ui.as_weak(), refresh.clone());
        move || {
            let paused = weak.upgrade().is_some_and(|ui| ui.get_paused());
            let req = if paused { Request::Resume } else { Request::Pause { minutes: None } };
            let refresh = refresh.clone();
            std::thread::spawn(move || {
                let _ = ipc::call(&pipe(), &req);
                let _ = slint::invoke_from_event_loop(refresh);
            });
        }
    });

    refresh();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), refresh);

    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            place(ui.window());
            ui.window().with_winit_window(|w| w.focus_window());
        }
    });
    // Started from the tray without foreground rights, the window may never get focus (and so
    // never close on focus loss). Ask once more, directly.
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(1), move || {
        if let Some(ui) = weak.upgrade().filter(|_| !focused.get()) {
            ui.window().with_winit_window(win::bring_to_front);
        }
    });
    ui.run()
}

/// The engine's pipe. Debug builds: FERRY_PIPE points the UI at a stand-in engine instead, for
/// screenshots of states the real one can't be put in.
fn pipe() -> String {
    let forced = if cfg!(debug_assertions) { std::env::var("FERRY_PIPE").ok() } else { None };
    forced.unwrap_or_else(ipc::pipe)
}

/// Starts this program again: no arguments is the engine, `--setup` the settings window.
fn ferry(args: &[&str]) -> std::io::Result<std::process::Child> {
    std::process::Command::new(std::env::current_exe()?).args(args).spawn()
}

/// In the primary work area's corner by the tray, 12 px in from each edge.
fn place(w: &slint::Window) {
    let Some(wa) = win::work_area() else { return };
    let margin = (12.0 * w.scale_factor()).round() as i32;
    let size = w.size();
    let (x, y) = corner(win::taskbar_edge(), (wa.left, wa.top, wa.right, wa.bottom), (size.width as i32, size.height as i32), margin);
    w.set_position(slint::PhysicalPosition::new(x, y));
}

/// Where the flyout goes: the tray sits at the right (or bottom) end of the taskbar, so the
/// flyout hugs the taskbar's edge of the work area and that end of it.
fn corner(edge: win::Edge, (l, t, r, b): (i32, i32, i32, i32), (w, h): (i32, i32), m: i32) -> (i32, i32) {
    let x = if edge == win::Edge::Left { l + m } else { r - m - w };
    let y = if edge == win::Edge::Top { t + m } else { b - m - h };
    (x, y)
}

/// One Status call on a worker thread, unless one is still out; `deliver` hands the result to the UI.
fn poll(busy: &Arc<AtomicBool>, deliver: impl Fn(Poll) + Send + 'static) {
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let busy = busy.clone();
    std::thread::spawn(move || {
        // A pipe read can't time out, so a hung engine would otherwise leave the panel stale forever.
        let r = with_timeout(|| ipc::call(&pipe(), &Request::Status), Duration::from_secs(5), || deliver(Poll::Hung));
        busy.store(false, Ordering::Release);
        deliver(r.map_or(Poll::Hung, classify));
    });
}

fn show_problem(ui: &Flyout, p: &Poll) {
    let (status, detail, up) = problem_copy(p);
    ui.set_running(up);
    ui.set_status(status.into());
    ui.set_detail(detail.into());
    ui.set_doing("".into());
    ui.set_trust_note("".into());
    ui.set_location("".into());
    ui.set_show_meter(false);
    ui.set_attention(ModelRc::default());
    ui.set_rows(ModelRc::default());
    ui.set_trips("".into());
}

fn show(ui: &Flyout, s: &Status, n: &Names) {
    let icon = match s.location {
        Location::Source => "\u{E731}",
        Location::Destination => "\u{E80F}",
        Location::Elsewhere => "\u{E707}",
    };
    ui.set_running(true);
    ui.set_location(n.at(s.location).unwrap_or_else(|| "Elsewhere".into()).into());
    ui.set_location_icon(icon.into());
    ui.set_status(status_text(s, n).into());
    ui.set_detail(detail_text(s, usual_detail(s, n), n).into());
    ui.set_doing(s.step.as_str().into());
    ui.set_trust_note(trust_note(s).into());
    ui.set_paused(s.paused_until.is_some());
    ui.set_trips(trips_line(s.trips_behind, n).into());
    ui.set_attention(model(s.attention.iter().map(|a| a.as_str().into()).collect()));

    let m = meter(s, n);
    ui.set_show_meter(m.is_some());
    let (label, amount, segs, value) = m.unwrap_or_default();
    ui.set_meter_label(label.into());
    ui.set_meter_amount(amount.into());
    ui.set_meter_value(value);
    ui.set_segments(model(segs));
    ui.set_rows(model(rows(s, &|_| String::new(), n)));
}

const TONE_FREED: i32 = 3;
const TONE_LANDED: i32 = 4;

/// The flyout's bar: (label, amount, segments, value 0 to 100). What is on the laptop while
/// anything is, or is being loaded; unloading also shows the space already freed. With the laptop
/// empty at the destination, how much of the source is there, if not all of it.
fn meter(s: &Status, n: &Names) -> Option<(String, String, Vec<Seg>, f32)> {
    if s.cap_bytes > 0 && (s.carried_bytes > 0 || s.phase == Phase::Filling) {
        let mut segs = segments(s);
        if s.phase != Phase::Unloading {
            return Some(("On the laptop".into(), format!("{} of {} GB", gb(s.carried_bytes), gb(s.cap_bytes)), segs, meter_value(s)));
        }
        let start = (s.carried_bytes as f32 / s.cap_bytes as f32).min(1.0);
        let width = (s.trip_bytes.saturating_sub(s.carried_bytes) as f32 / s.cap_bytes as f32).min(1.0 - start);
        if width > 0.0 {
            segs.push(Seg { start, width, tone: TONE_FREED });
        }
        return Some(("Left on the laptop".into(), format!("{} GB", gb(s.carried_bytes)), segs, meter_value(s)));
    }
    let empty_here = s.location == Location::Destination && s.phase == Phase::Idle && s.carried_bytes == 0;
    let (pct, have, total) = caught_up(s).filter(|_| empty_here && s.pairs.iter().any(|p| !p.up_to_date))?;
    let seg = Seg { start: 0.0, width: f32::from(pct) / 100.0, tone: TONE_LANDED };
    Some((format!("At {}", n.dest), format!("{} of {} GB", gb(have), gb(total)), vec![seg], f32::from(pct)))
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Share of the cap carried, 0 to 100.
fn meter_value(s: &Status) -> f32 {
    (s.carried_bytes as f64 * 100.0 / s.cap_bytes.max(1) as f64).min(100.0) as f32
}

/// Cap meter segments: one per folder with something carried, toned by the folder's position.
fn segments(s: &Status) -> Vec<Seg> {
    let mut start = 0.0f32;
    let mut segs = vec![];
    for (i, p) in s.pairs.iter().enumerate().filter(|(_, p)| p.carried_bytes > 0) {
        let width = (p.carried_bytes as f32 / s.cap_bytes.max(1) as f32).min(1.0 - start);
        segs.push(Seg { start, width, tone: (i % 3) as i32 });
        start += width;
    }
    segs
}

/// Folder rows; `paths` gives the "source → destination" line for a pair id (the flyout leaves it empty).
fn rows(s: &Status, paths: &dyn Fn(&str) -> String, n: &Names) -> Vec<Row> {
    s.pairs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (detail, right, kind) = folder_row(p, n);
            let (line, now) = flyout_line(s, p, kind, &detail, n);
            let total = size(p.source_bytes);
            let label = [p.id.as_str(), &total, &detail, &right].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
            Row {
                line: line.into(),
                // Matches the folder's meter segment (`segments`).
                tone: if p.carried_bytes > 0 { (i % 3) as i32 } else { -1 },
                now,
                name: p.id.as_str().into(),
                total: total.into(),
                detail: detail.into(),
                right: right.into(),
                percent: f32::from(p.destination_percent.min(100)) / 100.0,
                kind,
                label: label.into(),
                paths: paths(&p.id).into(),
            }
        })
        .collect()
}

mod win {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{GetSysColor, SYS_COLOR_INDEX};
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, SystemParametersInfoW, SPI_GETHIGHCONTRAST, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    pub fn high_contrast() -> bool {
        let mut hc = HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
        let ok = unsafe {
            SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, Some((&raw mut hc).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
        };
        ok.is_ok() && hc.dwFlags.contains(HCF_HIGHCONTRASTON)
    }

    /// 0x00BBGGRR, the same layout as the DWM accent.
    pub fn sys_color(i: SYS_COLOR_INDEX) -> u32 {
        unsafe { GetSysColor(i) }
    }

    pub fn hwnd(w: &slint::winit_030::winit::window::Window) -> Option<HWND> {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match w.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(HWND(h.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    pub fn bring_to_front(w: &slint::winit_030::winit::window::Window) {
        if let Some(h) = hwnd(w) {
            let _ = unsafe { SetForegroundWindow(h) };
        }
    }

    /// The Windows folder picker, modal to `owner`. None when cancelled or unavailable.
    pub fn pick_folder(owner: Option<HWND>) -> Option<std::path::PathBuf> {
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
        unsafe {
            // winit has already made this an STA thread; this only matters if it hasn't.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            d.SetOptions(d.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
            d.Show(owner).ok()?;
            let name = d.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as *const _));
            path.map(Into::into)
        }
    }

    /// The Windows Open dialog, or (`save`) Save As starting at `name`, for a backup file. None when
    /// cancelled or unavailable.
    pub fn pick_backup(owner: Option<HWND>, save: bool, name: &str) -> Option<std::path::PathBuf> {
        use windows::core::{w, HSTRING};
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
        use windows::Win32::UI::Shell::{FileOpenDialog, FileSaveDialog, IFileDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let d: IFileDialog = CoCreateInstance(if save { &FileSaveDialog } else { &FileOpenDialog }, None, CLSCTX_INPROC_SERVER).ok()?;
            d.SetFileTypes(&[COMDLG_FILTERSPEC { pszName: w!("Ferry backup"), pszSpec: w!("*.json") }]).ok()?;
            d.SetDefaultExtension(w!("json")).ok()?;
            let must_exist = if save { Default::default() } else { FOS_FILEMUSTEXIST };
            d.SetOptions(d.GetOptions().ok()? | FOS_FORCEFILESYSTEM | must_exist).ok()?;
            if save {
                d.SetFileName(&HSTRING::from(name)).ok()?;
            }
            d.Show(owner).ok()?;
            let path = d.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let p = path.to_string().ok();
            CoTaskMemFree(Some(path.0 as *const _));
            p.map(Into::into)
        }
    }

    pub fn reg_dword(key: PCWSTR, name: PCWSTR) -> Option<u32> {
        let mut v = 0u32;
        let mut n = 4u32;
        let r = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_DWORD, None, Some((&raw mut v).cast()), Some(&mut n))
        };
        r.is_ok().then_some(v)
    }

    /// A REG_BINARY of exactly N bytes.
    pub fn reg_bytes<const N: usize>(key: PCWSTR, name: PCWSTR) -> Option<[u8; N]> {
        let mut v = [0u8; N];
        let mut n = N as u32;
        let r = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_BINARY, None, Some(v.as_mut_ptr().cast()), Some(&mut n))
        };
        (r.is_ok() && n as usize == N).then_some(v)
    }

    /// Primary monitor minus the taskbar, in physical pixels (winit makes us per-monitor DPI aware).
    pub fn work_area() -> Option<RECT> {
        let mut rc = RECT::default();
        unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, Some((&raw mut rc).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) }
            .ok()
            .map(|()| rc)
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Edge {
        Left,
        Top,
        Right,
        Bottom,
    }

    /// Which screen edge the taskbar is on; Bottom when Windows won't say.
    pub fn taskbar_edge() -> Edge {
        use windows::Win32::UI::Shell::{SHAppBarMessage, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA};
        let mut d = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
        if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut d) } == 0 {
            return Edge::Bottom;
        }
        match d.uEdge {
            ABE_LEFT => Edge::Left,
            ABE_TOP => Edge::Top,
            ABE_RIGHT => Edge::Right,
            _ => Edge::Bottom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names as a user might type them: kept as written, spaces and all.
    fn n() -> Names {
        Names { src: "HQ".into(), dest: "Lake house".into() }
    }

    #[test]
    fn the_flyout_sits_by_the_taskbar() {
        let (wa, size, m) = ((100, 50, 1900, 1030), (300, 400), 12);
        assert_eq!(corner(win::Edge::Bottom, wa, size, m), (1588, 618));
        assert_eq!(corner(win::Edge::Right, wa, size, m), (1588, 618));
        assert_eq!(corner(win::Edge::Top, wa, size, m), (1588, 62));
        assert_eq!(corner(win::Edge::Left, wa, size, m), (112, 618));
    }

    #[test]
    fn names_fall_back_to_the_roles() {
        let mut c = Config::default();
        let d = Names::of(&c);
        assert_eq!((d.src.as_str(), d.dest.as_str()), ("Source", "Destination"));
        assert_eq!(status_line(Phase::UpToDate, None, &d), "Destination is up to date");
        c.destination.name = " Lake house ".into();
        assert_eq!(Names::of(&c).dest, "Lake house");
    }

    #[test]
    fn formats_detail_line() {
        assert_eq!(
            detail_line(41_016_000_000, 53_687_091_200, 108 << 20, Some(120)),
            "38.2 of 50 GB · 108 MB/s · about 2 min left"
        );
    }

    #[test]
    fn phase_copy() {
        assert_eq!(status_line(Phase::Waiting, None, &n()), "Waiting: not on a known network");
    }

    #[test]
    fn detail_line_omits_empty_parts_and_rounds_eta_up() {
        assert_eq!(detail_line(0, 0, 0, None), "");
        assert_eq!(detail_line(0, 53_687_091_200, 0, Some(0)), "0 of 50 GB");
        assert_eq!(detail_line(0, 0, 5 << 20, Some(61)), "5 MB/s · about 2 min left");
    }

    #[test]
    fn every_phase_has_copy() {
        assert_eq!(status_line(Phase::Filling, None, &n()), "Loading at HQ");
        assert_eq!(status_line(Phase::Unloading, None, &n()), "Unloading at Lake house");
        assert_eq!(status_line(Phase::UpToDate, None, &n()), "Lake house is up to date");
        assert_eq!(status_line(Phase::Idle, Some(1), &n()), "Paused");
        assert_eq!(status_line(Phase::Idle, None, &n()), "Idle");
    }

    fn pair(new_carried: u32, removed: u32, pct: u8, done: bool, confirm: bool) -> PairStatus {
        PairStatus {
            id: "p".into(),
            carried_bytes: 1_492_501_135, // 1.39 GiB
            destination_percent: pct,
            new_carried,
            removed_at_destination: removed,
            up_to_date: done,
            needs_confirm: confirm,
            pending_deletes: 0,
            deletes_list: 0,
            source_bytes: 0,
        }
    }

    #[test]
    fn folder_rows_follow_the_mockup() {
        let r = |p: PairStatus| folder_row(&p, &n());
        assert_eq!(r(pair(4, 1, 20, false, false)), ("4 new carried · 1 removed at Lake house".into(), "1.4 GB".into(), ROW_TEXT));
        assert_eq!(r(pair(0, 2, 20, false, false)), ("2 removed at Lake house".into(), "1.4 GB".into(), ROW_TEXT));
        assert_eq!(r(pair(0, 0, 61, false, false)), (String::new(), "61% at Lake house".into(), ROW_BAR));
        assert_eq!(r(pair(0, 0, 100, true, false)), ("Up to date".into(), String::new(), ROW_DONE));
        assert_eq!(r(pair(4, 0, 50, false, true)).0, "Needs your OK to delete files");
        assert_eq!(r(pair(4, 0, 50, false, true)).2, ROW_ATTENTION);
    }

    fn status(phase: Phase, carried: u64, trips_behind: Option<u32>, pairs: Vec<PairStatus>) -> Status {
        Status { location: Location::Destination, phase, remote_ok: true, cap_bytes: 100, carried_bytes: carried, trips_behind, pairs, ..Default::default() }
    }

    #[test]
    fn idle_at_destination_says_what_is_left() {
        let done = || vec![pair(0, 0, 100, true, false)];
        let text = |s: Status| status_text(&s, &n());
        assert_eq!(text(status(Phase::UpToDate, 0, Some(0), done())), "Lake house is up to date");
        assert_eq!(text(status(Phase::Idle, 0, Some(3), done())), "Lake house is about 3 trips behind");
        assert_eq!(text(status(Phase::Idle, 0, Some(0), vec![pair(0, 0, 0, false, true)])), "Deletes at Lake house need your OK");
        assert_eq!(text(status(Phase::Idle, 0, None, vec![pair(0, 0, 0, false, false)])), "Lake house isn't fully up to date yet");
        // Still carrying here: the plain line.
        assert_eq!(text(status(Phase::Idle, 5, Some(3), done())), "Idle");
        assert_eq!(text(status(Phase::Unloading, 0, Some(3), done())), "Unloading at Lake house");
    }

    const GB: u64 = 1 << 30;

    fn sized(id: &str, carried: u64, pct: u8, source: u64) -> PairStatus {
        PairStatus { id: id.into(), carried_bytes: carried, destination_percent: pct, source_bytes: source, ..pair(0, 0, pct, pct == 100 && carried == 0, false) }
    }

    /// The screenshot that started trip-states.html: at the destination, laptop empty, one folder 90% there.
    #[test]
    fn delivered_says_how_far_the_destination_is() {
        let s = Status { cap_bytes: 350 * GB, ..status(Phase::Idle, 0, None, vec![sized("repositories", 0, 90, 700 * GB)]) };
        assert_eq!(status_text(&s, &n()), "Lake house is 90% caught up");
        assert_eq!(usual_detail(&s, &n()), "Laptop is empty; the next trip from HQ brings the last 70 GB");
        // More than one laptop's worth: no promise about the next trip.
        assert_eq!(usual_detail(&Status { cap_bytes: 50 * GB, ..s.clone() }, &n()), "Laptop is empty; 70 GB still to bring from HQ");
        // A folder without a listing yet, or only deletes waiting: no percent to stand behind.
        let unlisted = status(Phase::Idle, 0, None, vec![sized("a", 0, 100, GB), pair(0, 0, 0, false, false)]);
        assert_eq!((status_text(&unlisted, &n()).as_str(), meter(&unlisted, &n()).is_none()), ("Lake house isn't fully up to date yet", true));
        let deletes = status(Phase::Idle, 0, None, vec![sized("a", 0, 100, GB), sized("b", 0, 100, GB)].into_iter().map(|p| PairStatus { up_to_date: p.id == "a", ..p }).collect());
        assert_eq!((status_text(&deletes, &n()).as_str(), usual_detail(&deletes, &n()).as_str()), ("Lake house isn't fully up to date yet", ""));
        let (label, amount, segs, value) = meter(&s, &n()).unwrap();
        assert_eq!((label.as_str(), amount.as_str(), segs[0].tone, value), ("At Lake house", "630 of 700 GB", TONE_LANDED, 90.0));
        let r = &rows(&s, &|_| String::new(), &n())[0];
        assert_eq!((r.line.as_str(), r.tone, r.kind), ("70 GB still to bring", -1, ROW_BAR));
        // 99.9% there still isn't caught up.
        let s = status(Phase::Idle, 0, None, vec![sized("a", 0, 100, 999 * GB), sized("b", 0, 0, GB)]);
        assert_eq!(status_text(&s, &n()), "Lake house is 99% caught up");
    }

    #[test]
    fn a_trip_reads_as_loading_then_unloading() {
        let at = |location, phase, carried| Status { location, phase, cap_bytes: 350 * GB, ..status(phase, carried, Some(2), vec![]) };
        let mut s = at(Location::Source, Phase::Filling, 252 * GB);
        s.pairs = vec![sized("repositories", 190 * GB, 70, 700 * GB), sized("vm", 62 * GB, 50, 400 * GB), sized("photos", 0, 100, 90 * GB)];
        s.active_pair = "vm".into();
        assert_eq!(status_text(&s, &n()), "Loading at HQ");
        let (label, amount, segs, _) = meter(&s, &n()).unwrap();
        assert_eq!((label.as_str(), amount.as_str(), segs.len()), ("On the laptop", "252 of 350 GB", 2));
        let lines: Vec<_> = rows(&s, &|_| String::new(), &n()).into_iter().map(|r| (r.line.to_string(), r.tone, r.now)).collect();
        assert_eq!(lines[0], ("190 GB on board".into(), 0, false));
        assert_eq!(lines[1], ("Loading · 62 GB on board".into(), 1, true));
        assert_eq!(lines[2], ("Up to date".into(), -1, false));

        // Done loading, still at the source; then on the way.
        let loaded = Status { phase: Phase::Idle, active_pair: String::new(), ..s.clone() };
        // 252 of 350 GB: a fill that stopped early, which may start again here.
        assert_eq!(status_text(&loaded, &n()), "252 GB on board for Lake house");
        assert_eq!(status_text(&Status { carried_bytes: 340 * GB, ..loaded.clone() }, &n()), "Loaded. Take the laptop to Lake house");
        assert_eq!(usual_detail(&loaded, &n()), "Unloading starts when the laptop is at Lake house");
        let away = Status { location: Location::Elsewhere, phase: Phase::Waiting, ..loaded.clone() };
        assert_eq!(status_text(&away, &n()), "Carrying 252 GB to Lake house");
        assert_eq!(status_text(&Status { carried_bytes: 0, ..away }, &n()), "Waiting: not on a known network");

        // Unloading: the bar drains and the freed space follows it.
        let mut u = Status { location: Location::Destination, phase: Phase::Unloading, carried_bytes: 64 * GB, trip_bytes: 252 * GB, active_pair: "vm".into(), ..s };
        u.pairs[0].carried_bytes = 0;
        u.pairs[1].carried_bytes = 64 * GB;
        assert_eq!(status_text(&u, &n()), "Unloading at Lake house");
        assert_eq!(usual_detail(&u, &n()), "188 of 252 GB unloaded");
        let (label, amount, segs, _) = meter(&u, &n()).unwrap();
        assert_eq!((label.as_str(), amount.as_str()), ("Left on the laptop", "64 GB"));
        let freed = segs.last().unwrap();
        assert_eq!(freed.tone, TONE_FREED);
        assert!((freed.start - 64.0 / 350.0).abs() < 1e-6 && (freed.width - 188.0 / 350.0).abs() < 1e-6);
        assert_eq!(rows(&u, &|_| String::new(), &n())[1].line.as_str(), "Unloading · 64 GB left");
        // Deletions show on an emptied folder too.
        u.pairs[0].removed_at_destination = 3;
        assert_eq!(rows(&u, &|_| String::new(), &n())[0].line.as_str(), "210 GB still to bring · 3 removed at Lake house");
    }

    #[test]
    fn nothing_to_measure_hides_the_bar() {
        assert!(meter(&status(Phase::UpToDate, 0, Some(0), vec![sized("a", 0, 100, GB)]), &n()).is_none());
        // Before the first listing there's no size to compare with.
        assert!(meter(&status(Phase::Idle, 0, None, vec![pair(0, 0, 40, false, false)]), &n()).is_none());
    }

    #[test]
    fn remote_trouble_is_shown_where_it_matters() {
        // No VPN name; the remote address may be any VPN.
        let line = |l, ok| remote_line(l, ok, &n());
        assert_eq!(line(Location::Source, false).as_deref(), Some("Can't reach Lake house from here; nothing was carried"));
        assert_eq!(line(Location::Destination, false).as_deref(), Some("Can't reach HQ from here; deletes at Lake house wait"));
        assert_eq!(line(Location::Elsewhere, false), None);
        assert_eq!(line(Location::Source, true), None);
    }

    #[test]
    fn a_blocked_path_says_where_and_why() {
        let why = "Windows sends traffic for 192.0.2.10 through WireGuard, and Ferry can't confirm that link stays on this network.";
        let blocked = |location, phase, paused_until| Status {
            location,
            phase,
            paused_until,
            bulk_blocked: Some(why.into()),
            ..status(Phase::Idle, 0, None, vec![])
        };
        let s = blocked(Location::Source, Phase::Idle, None);
        assert_eq!((status_text(&s, &n()), detail_text(&s, String::new(), &n())), ("At HQ. Not copying".into(), why.into()));
        let s = blocked(Location::Destination, Phase::UpToDate, None);
        assert_eq!((status_text(&s, &n()), detail_text(&s, String::new(), &n())), ("At Lake house. Not copying".into(), why.into()));
        // Paused says so first; the reason still shows.
        assert_eq!(status_text(&blocked(Location::Source, Phase::Idle, Some(1)), &n()), "Paused");
        // Not blocked: the usual lines.
        let fine = status(Phase::Idle, 0, None, vec![]);
        assert_eq!(detail_text(&fine, "5 MB/s".into(), &n()), "5 MB/s");
        assert_eq!(
            detail_text(&Status { location: Location::Source, remote_ok: false, ..fine.clone() }, "x".into(), &n()),
            "Can't reach Lake house from here; nothing was carried"
        );
    }

    #[test]
    fn trusting_a_vpn_gets_a_quiet_line() {
        let s = status(Phase::Filling, 0, None, vec![]);
        assert_eq!(trust_note(&s), "");
        assert_eq!(trust_note(&Status { trusting_vpn: true, ..s }), "Copying through a VPN you trust on this network");
    }

    /// String literals in Rust or Slint source, comments skipped (raw strings and char literals too).
    fn literals(src: &str) -> Vec<String> {
        let b = src.as_bytes();
        let ident = |i: usize| b[i].is_ascii_alphanumeric() || b[i] == b'_';
        let (mut out, mut i) = (vec![], 0);
        while i < b.len() {
            let rest = &src[i..];
            if rest.starts_with("//") {
                i += rest.find('\n').unwrap_or(rest.len());
            } else if rest.starts_with("/*") {
                i += rest.find("*/").map_or(rest.len(), |e| e + 2);
            } else if b[i] == b'r' && !(i > 0 && ident(i - 1) && b[i - 1] != b'b') && matches!(b.get(i + 1), Some(b'"' | b'#')) {
                let hashes = rest[1..].bytes().take_while(|&c| c == b'#').count();
                if rest.as_bytes().get(1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let body = &rest[2 + hashes..];
                let end = body.find(&close).unwrap_or(body.len());
                out.push(body[..end].to_string());
                i += 2 + hashes + end + close.len();
            } else if b[i] == b'"' {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                out.push(src[i + 1..j.min(b.len())].to_string());
                i = j + 1;
            } else if b[i] == b'\'' && b.get(i + 1) == Some(&b'\\') {
                i += 3 + rest[3..].find('\'').map_or(rest.len(), |e| e + 1);
            } else if b[i] == b'\'' && b.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    fn sources(dir: &std::path::Path, ext: &str, skip: &str, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() && !p.ends_with(skip) {
                sources(&p, ext, skip, out);
            } else if p.extension().is_some_and(|x| x == ext) {
                out.push(p);
            }
        }
    }

    #[test]
    fn literal_scanner_skips_comments_and_reads_raw_strings() {
        let src = r##"// "no"
/* "no" */ let a = "y\"es"; let c = '\"'; let d = '\\'; let e = '\''; let u = '\u{E731}'; let r = r#"raw "x""#; fn f<'a>() {}"##;
        assert_eq!(literals(src), ["y\\\"es", "raw \"x\""]);
    }

    /// The remote-address help text is the one generic string that may name it.
    const REMOTE_HELP: &str = concat!(
        "How Ferry reaches this server when you're somewhere else, for example a ",
        "Tail",
        "scale, WireGuard or ZeroTier address. Leave empty if it's only reachable on its own network."
    );

    /// The only shown strings that may still say "office" or "home": the Name field placeholders,
    /// and the legacy Credential Manager targets existing installs keep their passwords under.
    /// Comments, serde aliases (old files and messages) and tests are not scanned.
    const OLD_NAMES_ALLOWED: [&str; 6] =
        ["e.g. Office", "e.g. Home", "Ferry/office", "Ferry/home", r"Ferry\u{2060}/\u{2060}office", r"Ferry\u{2060}/\u{2060}home"];

    #[test]
    fn no_office_or_home_in_user_strings() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        sources(&here.join("ui"), "slint", "-", &mut files);
        sources(&here.join("src"), "rs", "-", &mut files);
        sources(&here.join("../ferry/src"), "rs", "-", &mut files);
        sources(&here.join("../ferry-core/src"), "rs", "-", &mut files);
        assert!(files.len() > 20, "{files:?}");
        let mut allowed_seen = 0;
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            let code: String = code.lines().filter(|l| !l.trim_start().starts_with("#[serde(")).map(|l| format!("{l}\n")).collect();
            for lit in literals(&code) {
                if OLD_NAMES_ALLOWED.contains(&lit.as_str()) {
                    allowed_seen += 1;
                    continue;
                }
                let old = lit.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case("office") || w.eq_ignore_ascii_case("home"));
                assert!(!old, "{}: {lit:?}", f.display());
            }
        }
        assert!(allowed_seen >= OLD_NAMES_ALLOWED.len(), "the scan reads the files that hold the allowed strings");
    }

    #[test]
    fn no_tailscale_in_ui_strings() {
        let needle = ["tail", "scale"].concat();
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        sources(&here.join("ui"), "slint", "-", &mut files);
        sources(&here.join("src"), "rs", "-", &mut files);
        // The engine's user-facing strings too; only the VPN checkers may name their VPN.
        sources(&here.join("../ferry/src"), "rs", "vpn", &mut files);
        assert!(files.len() > 10, "{files:?}");
        let mut help_seen = false;
        for f in &files {
            for lit in literals(&std::fs::read_to_string(f).unwrap()) {
                if lit == REMOTE_HELP {
                    help_seen = true;
                    continue;
                }
                assert!(!lit.to_lowercase().contains(&needle), "{}: {lit:?}", f.display());
            }
        }
        assert!(help_seen, "the remote-address help text is in the UI, word for word");
    }

    #[test]
    fn trips_copy() {
        assert_eq!(trips_line(None, &n()), "");
        assert_eq!(trips_line(Some(0), &n()), "Lake house is caught up");
        assert_eq!(trips_line(Some(1), &n()), "Lake house is about 1 trip behind");
        assert_eq!(trips_line(Some(3), &n()), "Lake house is about 3 trips behind");
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((contrast((0x77, 0x77, 0x77), (255, 255, 255)) - 4.48).abs() < 0.01);
        assert_eq!(contrast((9, 9, 9), (9, 9, 9)), 1.0);
    }

    #[test]
    fn meter_fills_reach_3_to_1_on_the_track_for_any_accent() {
        // (ink, track) for light and dark from theme.slint.
        let modes = [((0x16, 0x19, 0x1c), (0xe6, 0xe8, 0xea)), ((0xee, 0xf0, 0xf3), (0x29, 0x2c, 0x2e))];
        let accents = [(0xfc, 0xe1, 0x00), (0x00, 0x78, 0xd4), (0x4c, 0x4a, 0x48), (0xff, 0xff, 0xff), (0, 0, 0), (0xc4, 0x6d, 0x00)];
        for (ink, track) in modes {
            for a in accents {
                for c in meter_colors(a, ink, track) {
                    assert!(contrast(c, track) >= 3.0, "{a:?} -> {c:?} on {track:?}: {}", contrast(c, track));
                }
            }
        }
        // An accent that already passes is left alone.
        assert_eq!(meter_colors((0x00, 0x5a, 0x9e), (0x16, 0x19, 0x1c), (0xe6, 0xe8, 0xea))[0], (0x00, 0x5a, 0x9e));
    }

    #[test]
    fn poll_outcomes_have_their_own_copy() {
        let down = classify(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert_eq!(problem_copy(&down), ("Ferry isn't running", String::new(), false));
        let failed = classify(Ok(Response::Err("settings: bad toml".into())));
        assert_eq!(problem_copy(&failed), ("Ferry couldn't read its status", "settings: bad toml".into(), true));
        assert_eq!(problem_copy(&Poll::Hung), ("Ferry isn't responding", String::new(), true));
    }

    #[test]
    fn slow_calls_report_a_timeout_then_finish() {
        let timed_out = std::cell::Cell::new(false);
        let r = with_timeout(|| { std::thread::sleep(Duration::from_millis(100)); 7 }, Duration::from_millis(10), || timed_out.set(true));
        assert_eq!((r, timed_out.get()), (Some(7), true));
        let timed_out = std::cell::Cell::new(false);
        assert_eq!(with_timeout(|| 8, Duration::from_secs(5), || timed_out.set(true)), Some(8));
        assert!(!timed_out.get());
    }

    #[test]
    fn gray_accents_count_as_neutral() {
        assert!(neutral((0x4C, 0x4A, 0x48))); // the "Storm" gray accent
        assert!(neutral((0xA6, 0xA5, 0xA1)));
        assert!(!neutral((0x00, 0x78, 0xD4))); // default blue
        assert!(!neutral((0xc4, 0x6d, 0x00))); // brand amber
    }

    #[test]
    fn accent_is_abgr_and_ink_follows_luminance() {
        assert_eq!(abgr_rgb(0xFF48_4A4C), (0x4C, 0x4A, 0x48));
        assert!(!dark_ink_on((0x4C, 0x4A, 0x48))); // dark gray: light text
        assert!(dark_ink_on((0xA6, 0xA5, 0xA1))); // light gray: dark text
        assert!(!dark_ink_on((0x00, 0x78, 0xD4))); // Windows default blue: white text
    }
}
