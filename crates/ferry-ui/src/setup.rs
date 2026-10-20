//! `ferry --setup`: the Settings-style window (mockup A) and the first run.
use super::*;
use ferry_core::backup;
use ferry_core::cargo::CargoManifest;
use ferry_core::config::{self, display_name, Config, Fingerprint, Pair, Server, GB};
use ferry_core::deletes::DeleteQueue;
use ferry_core::ipc::{Secret, Side};
use slint::{Model, SharedString};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

const PLAIN: i32 = 0;
const DONE: i32 = 1;
const PROBLEM: i32 = 2;
const WORKING: i32 = 3;

const NOT_RUNNING: &str = "Ferry isn't running. Start Ferry, then try again.";
/// What the engine answers when its loop doesn't reply within 10 s (`engine::ask`). A ConfirmDeletes
/// waits for a running trip to end, so for that request it means "queued", not "failed".
const ENGINE_TIMEOUT: &str = "Ferry did not answer in time.";
/// A confirmation that waits for the running trip.
const AFTER_TRIP: &str = "Will apply after the current trip.";
/// The engine's answer when the deletes it was asked to confirm aren't the ones queued now.
const LIST_CHANGED: &str = "The list of deletes changed; review it again.";

fn note(text: impl Into<SharedString>, kind: i32) -> Note {
    Note { text: text.into(), kind }
}

fn problem(text: impl Into<SharedString>) -> Note {
    note(text, PROBLEM)
}

// S.update-state
const UPDATE_UNKNOWN: i32 = 0;
const UPDATE_CURRENT: i32 = 1;
const UPDATE_AVAILABLE: i32 = 2;
const UPDATE_INSTALLING: i32 = 3;
const UPDATE_PORTABLE: i32 = 4;

/// The Updates row for a `CheckUpdate` answer: (state, newer version, note).
fn update_view(r: Result<Response, String>) -> (i32, String, Note) {
    match r {
        Ok(Response::Update { latest: None, .. }) => (UPDATE_CURRENT, String::new(), Note::default()),
        Ok(Response::Update { latest: Some(v), installable: true }) => (UPDATE_AVAILABLE, v, Note::default()),
        Ok(Response::Update { latest: Some(v), installable: false }) => {
            let n = note(format!("This copy of Ferry wasn't installed with the installer, so it can't update itself. Download Ferry {v} from the releases page."), PLAIN);
            (UPDATE_PORTABLE, v, n)
        }
        Ok(_) => (UPDATE_UNKNOWN, String::new(), problem("Ferry sent an unexpected reply.")),
        Err(e) => (UPDATE_UNKNOWN, String::new(), problem(e)),
    }
}

fn check_update(ui: &Setup) {
    ui.global::<S>().set_update_note(note("Checking for updates...", WORKING));
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = reply(ipc::call(&pipe(), &Request::CheckUpdate));
        let _ = w.upgrade_in_event_loop(move |ui| {
            let g = ui.global::<S>();
            let (state, v, n) = update_view(r);
            g.set_update_state(state);
            g.set_update_version(v.into());
            g.set_update_note(n);
        });
    });
}

fn install_update(ui: &Setup) {
    send(ui, vec![Request::InstallUpdate], |ui, r| {
        let g = ui.global::<S>();
        match r {
            Ok(()) => {
                g.set_update_state(UPDATE_INSTALLING);
                g.set_update_note(Note::default());
            }
            Err((_, e)) => g.set_update_note(problem(e)),
        }
    });
}

/// The Credential Manager target for a side's password. These are the legacy names for the
/// source and destination: existing installs already have their passwords stored under them.
fn target(side: Side) -> &'static str {
    match side {
        Side::Source => "Ferry/office",
        Side::Destination => "Ferry/home",
    }
}

fn server_mut(c: &mut Config, side: Side) -> &mut Server {
    match side {
        Side::Source => &mut c.source,
        Side::Destination => &mut c.destination,
    }
}

fn server(c: &Config, side: Side) -> &Server {
    match side {
        Side::Source => &c.source,
        Side::Destination => &c.destination,
    }
}

fn side_of(i: i32) -> Side {
    if i == 0 { Side::Source } else { Side::Destination }
}

// ---------- config file ----------

fn config_path() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Ferry").join("config.toml")).ok_or_else(|| "APPDATA is not set.".into())
}

pub(crate) fn load() -> Result<Config, String> {
    let path = config_path()?;
    config::load(&path).map_err(|e| format!("Can't read your settings in {}: {e}", path.display()))
}

/// Read the file fresh (the engine writes learned networks into it), change it, save it.
fn update(change: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<Config, String> {
    let path = config_path()?;
    let mut cfg = config::load(&path).map_err(|e| format!("Not saved. Can't read {}: {e}", path.display()))?;
    change(&mut cfg)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Not saved. Can't create {}: {e}", dir.display()))?;
    }
    config::save(&path, &cfg).map_err(|e| format!("Not saved: {e}"))?;
    Ok(cfg)
}

/// `%LOCALAPPDATA%\Ferry\{name}`: the engine's state files.
fn state_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Ferry").join(name))
}

/// Ids a new pair must not take: the config's, and any that still have cargo or queued deletes
/// on the laptop (the engine discards those, so a new folder must never inherit them).
fn taken_ids(c: &Config) -> Vec<String> {
    let m = state_path("manifest.json").and_then(|p| CargoManifest::load(&p).ok()).unwrap_or_default();
    let q = state_path("deletes.json").and_then(|p| DeleteQueue::load(&p).ok()).unwrap_or_default();
    let mut ids: Vec<String> = c.pairs.iter().map(|p| p.id.clone()).collect();
    ids.extend(m.items.into_iter().map(|i| i.pair));
    ids.extend(m.partial.map(|p| p.pair));
    ids.extend(q.pairs.into_keys());
    ids
}

/// The id a pair keeps after an edit: a new one when a path changed, so the engine
/// discards what was carried for the old paths instead of delivering it to the new ones.
pub fn edited_id(p: &Pair, source: &str, destination: &str, taken: &[String]) -> String {
    if p.source_path.eq_ignore_ascii_case(source) && p.destination_path.eq_ignore_ascii_case(destination) {
        p.id.clone()
    } else {
        pair_id(source, taken)
    }
}

/// Refuse a new cargo folder while the old one still holds cargo.
fn cargo_move_ok(old: &std::path::Path, new: &std::path::Path, carried: usize, n: &Names) -> Result<(), String> {
    if old != new && carried > 0 {
        return Err(format!("Unload at {} first, then change the cargo folder.", n.dest));
    }
    Ok(())
}

/// No servers and no folders yet: show the first run.
fn is_blank(c: &Config) -> bool {
    c.source.lan_host.is_empty() && c.destination.lan_host.is_empty() && c.pairs.is_empty()
}

// ---------- engine requests ----------

/// The engine's answer, with its errors (and "not running") as text.
fn reply(r: std::io::Result<Response>) -> Result<Response, String> {
    match r {
        Ok(Response::Err(e)) => Err(e),
        Ok(r) => Ok(r),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(NOT_RUNNING.into()),
        Err(e) => Err(e.to_string()),
    }
}

fn outcome(r: std::io::Result<Response>) -> Result<(), String> {
    match reply(r)? {
        Response::Ok => Ok(()),
        _ => Err("Ferry sent an unexpected reply.".into()),
    }
}

/// The first failing request's index and error.
type Sent = Result<(), (usize, String)>;

/// Send `reqs` in order on a worker thread, stopping at the first error; `done` runs on the UI thread.
fn send(ui: &Setup, reqs: Vec<Request>, done: impl FnOnce(&Setup, Sent) + Send + 'static) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = reqs.iter().enumerate().try_for_each(|(i, q)| outcome(ipc::call(&pipe(), q)).map_err(|e| (i, e)));
        drop(reqs); // any password goes with it
        let _ = w.upgrade_in_event_loop(move |ui| done(&ui, r));
    });
}

/// After a config save, how the engine's ReloadConfig went.
fn after_save(r: Result<(), String>, ok: &str) -> Note {
    match r {
        Ok(()) => note(ok, DONE),
        Err(e) if e == NOT_RUNNING => note("Saved. Ferry isn't running; it uses this when it starts.", PLAIN),
        Err(e) => problem(format!("Saved, but Ferry couldn't load it: {e}")),
    }
}

/// Server card result: [SaveCredential if a password was typed], ReloadConfig, [TestConnection].
fn server_outcome(name: &str, has_password: bool, tested: bool, r: Sent) -> Note {
    match r {
        Ok(()) if tested => note(format!("Ferry reached {name} and signed in."), DONE),
        Ok(()) => note("Saved.", DONE),
        Err((0, e)) if has_password && e == NOT_RUNNING => {
            problem("The address is saved, but Ferry isn't running, so the password wasn't stored. Start Ferry, then enter the password again.")
        }
        Err((0, e)) if has_password => problem(format!("The address is saved, but the password wasn't stored: {e}")),
        Err((i, e)) if i == usize::from(has_password) && !tested => after_save(Err(e), "Saved."),
        // The engine bounds the test itself (8 s, with its own message), so this means Ferry as a
        // whole didn't answer.
        Err((_, e)) if tested && e == ENGINE_TIMEOUT => {
            problem("Saved, but Ferry didn't answer the test. Try again in a moment.")
        }
        Err((_, e)) => problem(e),
    }
}

fn confirm_outcome(r: Result<(), String>, n: &Names) -> Note {
    match r {
        Ok(()) => note(format!("Confirmed. Ferry deletes them at {} once everything new in this folder has arrived.", n.dest), DONE),
        Err(e) if e == ENGINE_TIMEOUT => note(AFTER_TRIP, DONE),
        Err(e) => problem(e),
    }
}

/// A confirmation left waiting for a trip: once no trip is running and the pair still waits for
/// an OK, the engine dropped it (the list changed), so the button comes back.
fn settle_held_note(n: Note, trip_running: bool) -> Note {
    if n.text == AFTER_TRIP && !trip_running { problem(LIST_CHANGED) } else { n }
}

// ---------- copy ----------

fn delete_label(count: u32, n: &Names) -> String {
    let dest = &n.dest;
    match count {
        0 => format!("Delete files at {dest}"),
        1 => format!("Delete 1 file at {dest}"),
        c => format!("Delete {c} files at {dest}"),
    }
}

/// The delete-confirmation rule, and why small folders always ask.
fn thresholds(pct: u8, count: u32, n: &Names) -> (String, String) {
    let dest = &n.dest;
    let rule = format!("Ferry asks before deleting at {dest} when one pass would remove more than {pct}% of a folder's files, or more than {count} files.");
    let why = match u32::from(pct) {
        0 => format!("So every delete at {dest} asks first."),
        p @ 1..=99 => format!(
            "Folders with fewer than {} files ask before every delete, because one file is already more than {p}% of them.",
            100u32.div_ceil(p)
        ),
        _ => String::new(),
    };
    (rule, why)
}

/// `\\host\a\b`, or just the path when the host isn't set yet.
fn unc_label(host: &str, path: &str) -> String {
    if host.is_empty() { path.to_string() } else { format!(r"\\{host}\{}", path.replace('/', r"\")) }
}

fn pair_paths(c: &Config, p: &Pair) -> String {
    format!("{} → {}", unc_label(&c.source.lan_host, &p.source_path), unc_label(&c.destination.lan_host, &p.destination_path))
}

/// Id of another pair (not `except`) with this source path; paths compare case-insensitively, like SMB.
fn duplicate_of<'a>(pairs: &'a [Pair], source: &str, except: Option<&str>) -> Option<&'a str> {
    pairs.iter().find(|p| Some(p.id.as_str()) != except && p.source_path.eq_ignore_ascii_case(source)).map(|p| p.id.as_str())
}

/// "YYYY-MM-DD HH:MM:SS  text" from the activity log.
fn split_line(l: &str) -> Line {
    match l.split_once("  ") {
        Some((t, x)) if t.len() == 19 => Line { time: t.into(), text: x.into() },
        _ => Line { time: "".into(), text: l.into() },
    }
}

fn closing(remembered: Option<Side>, n: &Names) -> String {
    match remembered {
        Some(side) => {
            let (here, other, card) =
                if side == Side::Source { (&n.src, &n.dest, "Destination server") } else { (&n.dest, &n.src, "Source server") };
            format!(
                "Ferry knows {here} now. It learns the network at {other} the first time you're there: open Ferry from the tray, go to Locations, and choose Remember this network under {card}."
            )
        }
        None => format!(
            "Ferry learns each place's network while you're on it. At {} and at {}, open Ferry from the tray, go to Locations, and choose Remember this network.",
            n.src, n.dest
        ),
    }
}

fn mac(f: &Fingerprint) -> String {
    f.gateway_mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// A learned network's name from Windows, or its router when Windows gave none.
fn net_name(f: &Fingerprint) -> String {
    if f.name.is_empty() { format!("Network {}", mac(f)) } else { f.name.clone() }
}

fn net_detail(f: &Fingerprint) -> String {
    format!("Router {}, server at {}", mac(f), f.server_lan_ip)
}

/// The engine's `Response::Path` verdict in plain words (spec 4.5). Problems are the ones that
/// stop copies.
fn path_line(verdict: &str, adapter: Option<String>) -> Note {
    let a = adapter.unwrap_or_default();
    if let Some(why) = verdict.strip_prefix("vpn-not-direct:") {
        return problem(format!("Through {a}: {why}, so Ferry won't copy files here"));
    }
    if let Some(why) = verdict.strip_prefix("blocked:") {
        return problem(why);
    }
    match verdict {
        "direct" => note("Direct on this network", DONE),
        "vpn-direct" => note(format!("Through {a}, direct on this network"), DONE),
        "vpn-unknown" => problem(format!("Through {a}: can't confirm it stays local, so Ferry won't copy files here")),
        "vpn-trusted" => note(format!("Through {a}, a VPN you trust for copies on this network"), PLAIN),
        "not-here" => note("Not on this server's network right now", PLAIN),
        _ => Note::default(),
    }
}

const NOT_NEEDED: &str = "Only needed when a VPN carries this server's traffic.";

/// What turning on "Trust this VPN for copies" leads to.
#[derive(Debug, PartialEq)]
enum Trust {
    /// Show the warning; trust this adapter if the user confirms.
    Confirm(String),
    /// The switch stays off and can't be turned on here; why.
    Disabled(String),
    /// The check failed; the switch stays usable.
    Retry(String),
}

/// An engine from before PathStatus can't answer it.
fn path_error(e: String) -> String {
    if e.contains("unknown variant") { "Update Ferry to check this path.".into() } else { e }
}

/// The server card's path line for the engine's answer; a plain line when it can't tell.
fn path_note(r: Result<Response, String>) -> Note {
    match r {
        Ok(Response::Path { verdict, adapter, .. }) => path_line(&verdict, adapter),
        Err(e) if e.contains("unknown variant") => note(path_error(e), PLAIN),
        _ => note("Ferry couldn't check this path right now.", PLAIN),
    }
}

/// Trust applies only to an unconfirmed VPN on this row's own network (router `mac`, server
/// `server`); a checker's verdict is never overridden, so a confirmed or refused path has nothing
/// to trust.
fn trust_answer(r: Result<Response, String>, mac: &str, server: &str) -> Trust {
    match r {
        Err(e) => Trust::Retry(path_error(e)),
        Ok(Response::Path { verdict, adapter: Some(a), router, server: s, .. }) if router == mac && s == server && !a.is_empty() => match verdict.as_str() {
            "vpn-unknown" | "vpn-trusted" => Trust::Confirm(a),
            "vpn-direct" => Trust::Disabled(format!("Not needed: Ferry checked that {a} keeps this server's traffic on this network.")),
            v => match v.strip_prefix("vpn-not-direct:") {
                Some(why) => Trust::Disabled(format!("Not available: {a} sends this server's traffic off this network ({why}).")),
                None => Trust::Disabled(NOT_NEEDED.into()),
            },
        },
        Ok(Response::Path { .. }) => Trust::Disabled(NOT_NEEDED.into()),
        Ok(_) => Trust::Retry("Ferry sent an unexpected reply.".into()),
    }
}

// ---------- window ----------

/// UI-thread state. Worker results come back through `upgrade_in_event_loop`, so they reach it too.
#[derive(Default)]
struct State {
    /// Last config read or written, for display.
    cfg: Config,
    /// Pair being edited (None: adding).
    edit: Option<String>,
    /// First run: the network learned in step 3.
    remembered: Option<Side>,
    /// First run: the folder added in step 4, so going Back edits it instead of adding another.
    first_pair: Option<String>,
    /// Pairs holding deletes for confirmation, with their count, from the last Status.
    held: Vec<(String, u32, u64)>,
    /// Per-pair result of "Delete N files at the destination".
    held_notes: HashMap<String, Note>,
    /// A learned network's trust switch while it isn't simply on or off: (state, hint, adapter
    /// to trust). States as in `NetRow`.
    trust: HashMap<TrustKey, (i32, String, String)>,
    /// Per side (source, destination): the latest path request, so an older answer is dropped.
    path_seq: [u64; 2],
    pairs: std::rc::Rc<VecModel<PairRow>>,
    source_nets: std::rc::Rc<VecModel<NetRow>>,
    destination_nets: std::rc::Rc<VecModel<NetRow>>,
}

/// (source, router MAC, server IP): a learned network, whatever its row.
type TrustKey = (bool, [u8; 6], std::net::IpAddr);

fn trust_key(side: Side, f: &Fingerprint) -> TrustKey {
    (side == Side::Source, f.gateway_mac, f.server_lan_ip)
}

const CHECKING: i32 = 1;
const ASKING: i32 = 2;
const UNAVAILABLE: i32 = 3;

thread_local! {
    static ST: RefCell<State> = RefCell::default();
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

pub fn run() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|a| {
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            // The exe's icon resource (assets/brand/ferry.rc).
            let icon = winit::window::Icon::from_resource(1, None).ok();
            a.with_window_icon(icon.clone()).with_taskbar_icon(icon)
        })
        .select()?;

    // Tells the engine a Settings window is open, so an automatic update waits for it to close.
    let _open = ferry_core::setup_window::hold(ferry_core::setup_window::NAME);
    let ui = Setup::new()?;
    let theme = ui.global::<Theme>();
    apply_theme(&theme);
    // Only the std ScrollView/ListView use the style palette.
    ui.global::<Palette>().set_color_scheme(if theme.get_dark() {
        slint::language::ColorScheme::Dark
    } else {
        slint::language::ColorScheme::Light
    });

    let g = ui.global::<S>();
    g.set_computer(std::env::var("COMPUTERNAME").unwrap_or_default().into());
    g.set_version(env!("CARGO_PKG_VERSION").into());
    let cargo_default = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Ferry").join("cargo"));
    g.set_cargo_default(cargo_default.map(|p| p.display().to_string()).unwrap_or_default().into());
    g.set_pairs(st(|s| s.pairs.clone()).into());
    g.set_source_nets(st(|s| s.source_nets.clone()).into());
    g.set_destination_nets(st(|s| s.destination_nets.clone()).into());
    match load() {
        Ok(cfg) => {
            g.set_first_run(is_blank(&cfg));
            show_servers(&g, &cfg);
            show_settings(&g, &cfg);
            st(|s| s.cfg = cfg);
            show_config(&ui);
        }
        Err(e) => g.set_load_note(problem(format!("{e}. Fix or delete the file, then open this window again. Nothing here can be saved until then."))),
    }
    wire(&ui);
    // Debug aid for screenshots: open on a page (0 Status ... 4 Settings), "edit" for the first
    // folder, or "destination" for the destination's Locations tab.
    match std::env::var("FERRY_PAGE").ok().filter(|_| cfg!(debug_assertions)).as_deref() {
        Some("destination") => {
            go(&ui, 2);
            g.set_locations_side(1);
        }
        Some("update") => {
            go(&ui, 4);
            check_update(&ui);
        }
        Some("edit") => {
            go(&ui, 1);
            open_pair(&ui, st(|s| s.cfg.pairs.first().cloned()));
        }
        Some(p) => {
            if let Ok(page) = p.parse() {
                go(&ui, page);
            }
        }
        None => {}
    }

    let busy = Arc::new(AtomicBool::new(false));
    let refresh = {
        let w = ui.as_weak();
        move || {
            let w = w.clone();
            poll(&busy, move |p| {
                let _ = w.upgrade_in_event_loop(move |ui| match p {
                    Poll::Status(s) => show_status(&ui, &s),
                    p => show_problem(&ui, &p),
                });
            });
        }
    };
    refresh();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), refresh);
    ui.run()
}

/// Hook every callback in `S` to its handler. Each holds a weak handle to the window.
fn wire(ui: &Setup) {
    let g = ui.global::<S>();
    macro_rules! on {
        ($set:ident, |$ui:ident $(, $arg:ident)*| $body:expr) => {{
            let w = ui.as_weak();
            g.$set(move |$($arg),*| {
                if let Some($ui) = w.upgrade() {
                    $body
                }
            });
        }};
    }
    on!(on_go, |ui, page| go(&ui, page));
    on!(on_toggle_pause, |ui| {
        let req = if ui.global::<S>().get_paused() { Request::Resume } else { Request::Pause { minutes: None } };
        send(&ui, vec![req], |_, _| {});
    });
    on!(on_start_engine, |_ui| {
        let _ = ferry(&[]);
    });

    on!(on_add_pair, |ui| open_pair(&ui, None));
    on!(on_edit_pair, |ui, i| {
        let p = st(|s| s.cfg.pairs.get(i as usize).cloned());
        open_pair(&ui, p);
    });
    on!(on_cancel_pair, |ui| ui.global::<S>().set_editing(false));
    on!(on_save_pair, |ui| save_pair(&ui));
    on!(on_remove_pair, |ui| remove_pair(&ui));
    on!(on_move_pair, |ui, i, delta| move_pair(&ui, i, delta));
    on!(on_toggle_pair, |ui, i| toggle_pair(&ui, i));
    g.on_preview_id(|source| {
        let ids = st(|s| taken_ids(&s.cfg));
        pair_id(share_path(&source).as_deref().unwrap_or(""), &ids).into()
    });

    on!(on_save_server, |ui, side| save_server(&ui, side_of(side), Then::Save));
    on!(on_test_server, |ui, side| save_server(&ui, side_of(side), Then::Test));
    on!(on_remember, |ui, side| remember(&ui, side_of(side)));
    on!(on_forget, |ui, side, i| forget(&ui, side_of(side), i));
    on!(on_trust, |ui, side, i| trust(&ui, side_of(side), i));
    on!(on_trust_confirm, |ui, side, i| trust_confirm(&ui, side_of(side), i));
    on!(on_trust_cancel, |ui, side, i| {
        if let Some(f) = fingerprint(side_of(side), i) {
            st(|s| s.trust.remove(&trust_key(side_of(side), &f)));
            show_config(&ui);
        }
    });
    on!(on_set_destination_empty, |ui, on| set_destination_empty(&ui, on));

    on!(on_confirm, |ui, id| confirm(&ui, id.to_string()));
    on!(on_refresh_activity, |ui| refresh_activity(&ui));

    on!(on_save_settings, |ui| save_settings(&ui));
    on!(on_check_update, |ui| check_update(&ui));
    on!(on_install_update, |ui| install_update(&ui));
    on!(on_export_backup, |ui| export_backup(&ui));
    on!(on_restore_backup, |ui| restore_backup(&ui));
    on!(on_choose_cargo, |ui| {
        let owner = ui.window().with_winit_window(win::hwnd).flatten();
        if let Some(p) = win::pick_folder(owner) {
            ui.global::<S>().set_cargo_dir(p.display().to_string().into());
        }
    });

    on!(on_next, |ui| {
        let g = ui.global::<S>();
        match g.get_step() {
            0 => save_server(&ui, Side::Source, Then::Next),
            1 => save_server(&ui, Side::Destination, Then::Next),
            3 => save_pair(&ui),
            _ => {
                g.set_first_run(false);
                go(&ui, 0);
            }
        }
    });
    on!(on_back, |ui| {
        let step = ui.global::<S>().get_step();
        if step > 0 {
            to_step(&ui, step - 1);
        }
    });
    on!(on_skip, |ui| {
        let g = ui.global::<S>();
        let step = g.get_step();
        match step {
            2 => g.set_network_skipped(true),
            3 => g.set_folder_skipped(true),
            _ => {}
        }
        to_step(&ui, step + 1);
    });
}

fn go(ui: &Setup, page: i32) {
    ui.global::<S>().set_page(page);
    if page == 2 {
        // A switch left unavailable earlier (maybe on another network) gets a fresh check.
        st(|s| s.trust.clear());
        show_config(ui);
        refresh_path(ui, Side::Source);
        refresh_path(ui, Side::Destination);
    }
    if page == 3 {
        // Open on what needs the user, if anything does.
        ui.global::<S>().set_activity_tab(if st(|s| s.held.is_empty()) { 1 } else { 0 });
        refresh_activity(ui);
    }
}

fn to_step(ui: &Setup, step: i32) {
    let g = ui.global::<S>();
    if step == 3 {
        let added = st(|s| s.first_pair.clone().and_then(|id| s.cfg.pairs.iter().find(|p| p.id == id).cloned()));
        open_pair(ui, added);
        // The first run shows the form itself; the Folders page stays on its list.
        g.set_editing(false);
    }
    if step == 4 {
        g.set_closing(st(|s| closing(s.remembered, &Names::of(&s.cfg))).into());
    }
    g.set_step(step);
}

// ---------- status and activity ----------

fn show_status(ui: &Setup, s: &Status) {
    let g = ui.global::<S>();
    let n = st(|st| Names::of(&st.cfg));
    let icon = match s.location {
        Location::Source => "\u{E731}",
        Location::Destination => "\u{E80F}",
        Location::Elsewhere => "\u{E707}",
    };
    g.set_running(true);
    g.set_place(n.at(s.location).unwrap_or_else(|| "Away from both networks".into()).into());
    g.set_place_icon(icon.into());
    // The place is the title here, so a blocked path needs only the second half.
    g.set_status(if blocked_place(s, &n).is_some() { "Not copying".into() } else { status_text(s, &n) }.into());
    // The amount carried is on the meter, so only speed and time left here.
    g.set_detail(detail_text(s, detail_line(0, 0, s.rate_bps, s.eta_secs), &n).into());
    g.set_attention(model(s.attention.iter().map(|a| a.as_str().into()).collect()));
    g.set_paused(s.paused_until.is_some());
    g.set_trips(trips_line(s.trips_behind, &n).into());
    g.set_show_meter(s.cap_bytes > 0);
    g.set_meter_value(meter_value(s));
    g.set_carried(format!("{} GB", gb(s.carried_bytes)).into());
    g.set_cap(format!("of {} GB carried", gb(s.cap_bytes)).into());
    g.set_segments(model(segments(s)));
    let mut legend: Vec<Legend> = s
        .pairs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.carried_bytes > 0)
        .map(|(i, p)| Legend { name: p.id.as_str().into(), size: size(p.carried_bytes).into(), tone: (i % 3) as i32 })
        .collect();
    legend.push(Legend { name: "free".into(), size: format!("{} GB", gb(s.cap_bytes.saturating_sub(s.carried_bytes))).into(), tone: 3 });
    g.set_legend(model(legend));
    let paths = |id: &str| st(|st| st.cfg.pairs.iter().find(|p| p.id == id).map(|p| pair_paths(&st.cfg, p)).unwrap_or_default());
    g.set_rows(model(rows(s, &paths, &n)));
    let trip_running = matches!(s.phase, Phase::Filling | Phase::Unloading);
    st(|st| {
        st.held = s.pairs.iter().filter(|p| p.needs_confirm).map(|p| (p.id.clone(), p.pending_deletes, p.deletes_list)).collect();
        let held = std::mem::take(&mut st.held_notes);
        st.held_notes = held
            .into_iter()
            .filter(|(id, _)| st.held.iter().any(|(h, ..)| h == id))
            .map(|(id, n)| (id, settle_held_note(n, trip_running)))
            .collect();
    });
    show_held(ui);
}

fn show_problem(ui: &Setup, p: &Poll) {
    let g = ui.global::<S>();
    let (status, detail, up) = problem_copy(p);
    g.set_running(up);
    g.set_place(status.into());
    g.set_place_icon("\u{E7BA}".into());
    g.set_status(detail.into());
    g.set_detail("".into());
    g.set_attention(ModelRc::default());
    g.set_show_meter(false);
    g.set_rows(ModelRc::default());
    st(|s| s.held.clear());
    show_held(ui);
}

fn show_held(ui: &Setup) {
    let held: Vec<Held> = st(|s| {
        let names = Names::of(&s.cfg);
        s.held
            .iter()
            .map(|(id, n, _)| {
                let note = s.held_notes.get(id).cloned().unwrap_or_default();
                // Hidden while a confirmation is out or done, so it can't be sent twice.
                let button = if note.kind == DONE || note.kind == WORKING { String::new() } else { delete_label(*n, &names) };
                Held { id: id.as_str().into(), button: button.into(), note }
            })
            .collect()
    });
    ui.global::<S>().set_held(model(held));
}

fn confirm(ui: &Setup, id: String) {
    // The count and fingerprint shown: the engine refuses if the queue no longer matches them.
    let (count, list) = st(|s| s.held.iter().find(|(h, ..)| *h == id).map_or((0, 0), |(_, n, l)| (*n, *l)));
    st(|s| s.held_notes.insert(id.clone(), note("Confirming...", WORKING)));
    show_held(ui);
    send(ui, vec![Request::ConfirmDeletes { pair: id.clone(), count, list }], move |ui, r| {
        st(|s| {
            let n = confirm_outcome(r.map_err(|(_, e)| e), &Names::of(&s.cfg));
            s.held_notes.insert(id, n)
        });
        show_held(ui);
        refresh_activity(ui);
    });
}

fn refresh_activity(ui: &Setup) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = ipc::call(&pipe(), &Request::ActivityTail { n: 200 });
        let _ = w.upgrade_in_event_loop(move |ui| {
            let g = ui.global::<S>();
            match r {
                Ok(Response::Activity(lines)) => {
                    g.set_lines(model(lines.iter().rev().map(|l| split_line(l)).collect()));
                    g.set_activity_note(Note::default());
                }
                other => g.set_activity_note(problem(outcome(other).err().unwrap_or_else(|| "Ferry sent an unexpected reply.".into()))),
            }
        });
    });
}

// ---------- folders ----------

/// Redraw folder rows and learned networks from the cached config. Rows change in place when
/// the count is the same, so keyboard focus stays in the list after a move or toggle.
fn show_config(ui: &Setup) {
    let (rows, source, destination) = st(|s| {
        let rows: Vec<PairRow> = s
            .cfg
            .pairs
            .iter()
            .map(|p| PairRow { id: p.id.as_str().into(), paths: pair_paths(&s.cfg, p).into(), enabled: p.enabled, allow_empty: p.allow_empty })
            .collect();
        let nets = |side: Side| -> Vec<NetRow> {
            server(&s.cfg, side)
                .fingerprints
                .iter()
                .map(|f| {
                    let (state, hint) = match s.trust.get(&trust_key(side, f)) {
                        Some((state, hint, _)) => (*state, hint.clone()),
                        None if f.trust_vpn => (0, format!("Copies may go through {} on this network.", f.trusted_adapter)),
                        None => (0, String::new()),
                    };
                    NetRow { name: net_name(f).into(), detail: net_detail(f).into(), trusted: f.trust_vpn, state, hint: hint.into() }
                })
                .collect()
        };
        (rows, nets(Side::Source), nets(Side::Destination))
    });
    let (pairs, source_nets, destination_nets) = st(|s| (s.pairs.clone(), s.source_nets.clone(), s.destination_nets.clone()));
    fill(&pairs, rows);
    let asking = |rows: &[NetRow]| rows.iter().position(|r| r.state == ASKING).map_or(-1, |i| i as i32);
    ui.global::<S>().set_source_asking(asking(&source));
    ui.global::<S>().set_destination_asking(asking(&destination));
    fill(&source_nets, source);
    fill(&destination_nets, destination);
    // Copy that names the sides follows the latest names and options.
    let g = ui.global::<S>();
    let (n, pct, count) = st(|s| (Names::of(&s.cfg), s.cfg.options.delete_confirm_percent, s.cfg.options.delete_confirm_count));
    let (rule, why) = thresholds(pct, count, &n);
    g.set_thresholds(rule.into());
    g.set_thresholds_why(why.into());
    g.set_destination_place_upper(n.dest.to_uppercase().into());
    g.set_source_place(n.src.into());
    g.set_destination_place(n.dest.into());
    g.set_pairs_on(st(|s| s.cfg.pairs.iter().filter(|p| p.enabled).count()) as i32);
    // Without remote addresses the destination is only known from a visit; the first run and
    // the destination's Locations tab both offer to skip that visit when it is empty.
    let (offline, empty) = st(|s| (s.cfg.source.remote_host.is_empty() && s.cfg.destination.remote_host.is_empty(), s.cfg.options.destination_starts_empty));
    g.set_sneakernet(offline);
    g.set_destination_empty(empty);
}

/// Replace a list's rows, in place when the count is the same, so keyboard focus stays in it
/// (and a row that grows, like a network's trust warning, is laid out as part of the page).
fn fill<T: Clone + 'static>(m: &VecModel<T>, rows: Vec<T>) {
    if m.row_count() == rows.len() {
        for (i, r) in rows.into_iter().enumerate() {
            m.set_row_data(i, r);
        }
    } else {
        m.set_vec(rows);
    }
}

/// Store a config just saved, redraw, and have the engine load it; `put` shows how that went.
fn saved(ui: &Setup, cfg: Config, ok: &str, put: fn(&S, Note)) {
    st(|s| s.cfg = cfg);
    show_config(ui);
    let ok = ok.to_string();
    send(ui, vec![Request::ReloadConfig], move |ui, r| put(&ui.global::<S>(), after_save(r.map_err(|(_, e)| e), &ok)));
}

fn open_pair(ui: &Setup, p: Option<Pair>) {
    let g = ui.global::<S>();
    let p = st(|s| {
        s.edit = p.as_ref().map(|p| p.id.clone());
        p.unwrap_or_default()
    });
    g.set_adding(st(|s| s.edit.is_none()));
    g.set_f_source(p.source_path.into());
    g.set_f_destination(if p.destination_path == g.get_f_source().as_str() { String::new() } else { p.destination_path }.into());
    g.set_f_allow_empty(p.allow_empty);
    g.set_f_id(p.id.into());
    g.set_f_note(Note::default());
    g.set_editing(true);
}

fn save_pair(ui: &Setup) {
    let g = ui.global::<S>();
    let paths = share_path(&g.get_f_source()).and_then(|source| {
        let destination = g.get_f_destination();
        let destination = if destination.trim().is_empty() { source.clone() } else { share_path(&destination)? };
        Ok((source, destination))
    });
    let (source, destination) = match paths {
        Ok(p) => p,
        Err(e) => return g.set_f_note(problem(e)),
    };
    let allow_empty = g.get_f_allow_empty();
    let edit = st(|s| s.edit.clone());
    let mut saved_id = None;
    let r = update(|c| {
        if let Some(other) = duplicate_of(&c.pairs, &source, edit.as_deref()) {
            return Err(format!("{source} is already in the list as {other}."));
        }
        if let Some(other) = ferry_core::config::overlapping_destination(&c.pairs, &destination, edit.as_deref()) {
            return Err(format!(
                "{destination} overlaps the destination of {other}. Two folders can't share a destination or sit inside each other's, or each would delete the other's files."
            ));
        }
        let taken = taken_ids(c);
        let id = match &edit {
            Some(id) => {
                let p = c.pairs.iter_mut().find(|p| &p.id == id).ok_or("This folder was removed in the meantime.")?;
                p.id = edited_id(p, &source, &destination, &taken);
                (p.source_path, p.destination_path, p.allow_empty) = (source, destination, allow_empty);
                p.id.clone()
            }
            None => {
                let id = pair_id(&source, &taken);
                c.pairs.push(Pair { id: id.clone(), source_path: source, destination_path: destination, enabled: true, allow_empty });
                id
            }
        };
        saved_id = Some(id);
        Ok(())
    });
    let cfg = match r {
        Ok(c) => c,
        Err(e) => return g.set_f_note(problem(e)),
    };
    st(|s| s.cfg = cfg);
    show_config(ui);
    g.set_editing(false);
    if g.get_first_run() {
        g.set_folder_skipped(false);
        st(|s| s.first_pair = saved_id);
        to_step(ui, 4);
    }
    // ponytail: the engine's TestConnection opens the first folder only; a per-folder test needs a new request.
    let src = st(|s| Names::of(&s.cfg).src);
    g.set_folders_note(note(format!("Saved. Checking {src}..."), WORKING));
    send(ui, vec![Request::ReloadConfig, Request::TestConnection { side: Side::Source }], move |ui, r| {
        ui.global::<S>().set_folders_note(match r {
            Ok(()) => note(format!("Saved. Ferry reached {src}."), DONE),
            Err((0, e)) => after_save(Err(e), "Saved."),
            Err((_, e)) => problem(format!("Saved, but Ferry couldn't reach {src}: {e}")),
        });
    });
}

fn remove_pair(ui: &Setup) {
    let Some(id) = st(|s| s.edit.clone()) else { return };
    let g = ui.global::<S>();
    match update(|c| {
        c.pairs.retain(|p| p.id != id);
        Ok(())
    }) {
        Ok(cfg) => {
            g.set_editing(false);
            let ok = format!("Removed. Files already at {} stay there; files carried for it are discarded.", Names::of(&cfg).dest);
            saved(ui, cfg, &ok, |g: &S, n| g.set_folders_note(n));
        }
        Err(e) => g.set_f_note(problem(e)),
    }
}

fn move_pair(ui: &Setup, i: i32, delta: i32) {
    // Moving i down is moving i + 1 up.
    let j = (i + delta.max(0)) as usize;
    if j == 0 || j >= st(|s| s.cfg.pairs.len()) {
        return;
    }
    match update(|c| {
        move_up(&mut c.pairs, j);
        Ok(())
    }) {
        Ok(cfg) => saved(ui, cfg, "Saved.", |g: &S, n| g.set_folders_note(n)),
        Err(e) => ui.global::<S>().set_folders_note(problem(e)),
    }
}

fn toggle_pair(ui: &Setup, i: i32) {
    let Some(id) = st(|s| s.cfg.pairs.get(i as usize).map(|p| p.id.clone())) else { return };
    match update(|c| {
        if let Some(p) = c.pairs.iter_mut().find(|p| p.id == id) {
            p.enabled = !p.enabled;
        }
        Ok(())
    }) {
        Ok(cfg) => saved(ui, cfg, "Saved.", |g: &S, n| g.set_folders_note(n)),
        Err(e) => ui.global::<S>().set_folders_note(problem(e)),
    }
}

// ---------- locations ----------

#[derive(Clone, Copy, PartialEq)]
enum Then {
    Save,
    Test,
    /// First run: on to the next step once saved.
    Next,
}

fn show_servers(g: &S, c: &Config) {
    g.set_source_name(c.source.name.as_str().into());
    g.set_destination_name(c.destination.name.as_str().into());
    g.set_source_lan(c.source.lan_host.as_str().into());
    g.set_source_ts(c.source.remote_host.as_str().into());
    g.set_destination_lan(c.destination.lan_host.as_str().into());
    g.set_destination_ts(c.destination.remote_host.as_str().into());
    g.set_source_user(c.source.user.as_str().into());
    g.set_destination_user(c.destination.user.as_str().into());
}

fn put_server_note(g: &S, side: Side, n: Note) {
    match side {
        Side::Source => g.set_source_note(n),
        Side::Destination => g.set_destination_note(n),
    }
}

/// Save a server card: addresses to config.toml, the password to the engine (once; the field is
/// cleared), then reload, and test when asked.
fn save_server(ui: &Setup, side: Side, then: Then) {
    let g = ui.global::<S>();
    let (name, lan, ts, user, password) = match side {
        Side::Source => (g.get_source_name(), g.get_source_lan(), g.get_source_ts(), g.get_source_user(), g.get_source_pass()),
        Side::Destination => {
            (g.get_destination_name(), g.get_destination_lan(), g.get_destination_ts(), g.get_destination_user(), g.get_destination_pass())
        }
    };
    let (name, (lan, ts)) = match name_field(&name).and_then(|name| Ok((name, server_fields(&lan, &ts, &user, &password)?))) {
        Ok(f) => f,
        Err(e) => return put_server_note(&g, side, problem(e)),
    };
    let cfg = update(|c| {
        let srv = server_mut(c, side);
        (srv.name, srv.lan_host, srv.remote_host, srv.user, srv.credential_target) = (name, lan, ts, user.trim().into(), target(side).into());
        name_conflict(c, side).map_or(Ok(()), Err)
    });
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => return put_server_note(&g, side, problem(e)),
    };
    st(|s| s.cfg = cfg);
    show_config(ui);

    let has_password = !password.is_empty();
    let mut reqs = vec![];
    if has_password {
        reqs.push(Request::SaveCredential { target: target(side).into(), user: user.trim().into(), password: Secret(password.into()) });
        match side {
            Side::Source => g.set_source_pass("".into()),
            Side::Destination => g.set_destination_pass("".into()),
        }
    }
    reqs.push(Request::ReloadConfig);
    if then == Then::Test {
        reqs.push(Request::TestConnection { side });
    }
    put_server_note(&g, side, note(if then == Then::Test { "Testing the connection..." } else { "Saving..." }, WORKING));
    let from = g.get_step();
    send(ui, reqs, move |ui, r| {
        let g = ui.global::<S>();
        let name = st(|s| display_name(&s.cfg, side));
        let n = server_outcome(&name, has_password, then == Then::Test, r);
        let ok = n.kind != PROBLEM;
        put_server_note(&g, side, n);
        // Only if the user hasn't moved on (or back) while this was out.
        if then == Then::Next && ok && g.get_step() == from {
            to_step(ui, from + 1);
        }
    });
}

fn remember(ui: &Setup, side: Side) {
    let g = ui.global::<S>();
    let working = note("Looking at this network...", WORKING);
    put_server_note(&g, side, working.clone());
    g.set_remember_note(working);
    send(ui, vec![Request::Remember { location: side }], move |ui, r| {
        let g = ui.global::<S>();
        // The engine's message is shown exactly as sent.
        let n = match r {
            Ok(()) => {
                if let Ok(cfg) = load() {
                    st(|s| s.cfg = cfg);
                    show_config(ui);
                }
                refresh_path(ui, side);
                note(format!("Ferry will recognize this network as {}.", st(|s| display_name(&s.cfg, side))), DONE)
            }
            Err((_, e)) => problem(e),
        };
        let ok = n.kind == DONE;
        put_server_note(&g, side, n.clone());
        g.set_remember_note(n);
        if ok && g.get_first_run() && g.get_step() == 2 {
            g.set_network_skipped(false);
            st(|s| s.remembered = Some(side));
            to_step(ui, 3);
        }
    });
}

fn forget(ui: &Setup, side: Side, i: i32) {
    match update(|c| {
        let fps = &mut server_mut(c, side).fingerprints;
        if (i as usize) < fps.len() {
            fps.remove(i as usize);
        }
        Ok(())
    }) {
        Ok(cfg) => {
            let put: fn(&S, Note) = if side == Side::Source { |g: &S, n| g.set_source_note(n) } else { |g: &S, n| g.set_destination_note(n) };
            saved(ui, cfg, "Forgotten. Ferry no longer recognizes that network.", put);
        }
        Err(e) => put_server_note(&ui.global::<S>(), side, problem(e)),
    }
}

/// The server card's path line: "Checking..." until the engine answers (it may take seconds).
/// Nothing when no network is learned yet.
fn refresh_path(ui: &Setup, side: Side) {
    let put = move |g: &S, n: Note| match side {
        Side::Source => g.set_source_path(n),
        Side::Destination => g.set_destination_path(n),
    };
    let g = ui.global::<S>();
    // Bumped first, so an answer still on its way for an earlier check is dropped either way.
    let i = usize::from(side == Side::Destination);
    let seq = st(|s| {
        s.path_seq[i] += 1;
        s.path_seq[i]
    });
    if st(|s| server(&s.cfg, side).fingerprints.is_empty()) {
        return put(&g, Note::default());
    }
    put(&g, note("Checking...", WORKING));
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = reply(ipc::call(&pipe(), &Request::PathStatus { side }));
        let _ = w.upgrade_in_event_loop(move |ui| {
            if st(|s| s.path_seq[i]) == seq {
                put(&ui.global::<S>(), path_note(r));
            }
        });
    });
}

fn fingerprint(side: Side, i: i32) -> Option<Fingerprint> {
    st(|s| server(&s.cfg, side).fingerprints.get(usize::try_from(i).ok()?).cloned())
}

/// The trust switch was flipped: off saves at once; on first checks this network's path, then
/// asks (spec 4.3).
fn trust(ui: &Setup, side: Side, i: i32) {
    let Some(f) = fingerprint(side, i) else { return };
    let key = trust_key(side, &f);
    if f.trust_vpn {
        return save_trust(ui, side, &f, None);
    }
    st(|s| s.trust.insert(key, (CHECKING, "Checking...".into(), String::new())));
    show_config(ui);
    let (w, router, server) = (ui.as_weak(), mac(&f), f.server_lan_ip.to_string());
    std::thread::spawn(move || {
        let r = reply(ipc::call(&pipe(), &Request::PathStatus { side }));
        let _ = w.upgrade_in_event_loop(move |ui| {
            // Cancelled or saved meanwhile: nothing to show.
            if st(|s| s.trust.get(&key).is_none_or(|t| t.0 != CHECKING)) {
                return;
            }
            let t = match trust_answer(r, &router, &server) {
                Trust::Confirm(a) => (ASKING, String::new(), a),
                Trust::Disabled(why) => (UNAVAILABLE, why, String::new()),
                Trust::Retry(e) => (0, e, String::new()),
            };
            st(|s| s.trust.insert(key, t));
            show_config(&ui);
        });
    });
}

fn trust_confirm(ui: &Setup, side: Side, i: i32) {
    let Some(f) = fingerprint(side, i) else { return };
    let adapter = st(|s| s.trust.get(&trust_key(side, &f)).filter(|t| t.0 == ASKING).map(|t| t.2.clone()));
    if let Some(a) = adapter {
        save_trust(ui, side, &f, Some(a));
    }
}

/// Store the switch with the network (`trusted_adapter` empty when off), then reload and show
/// the path again.
fn save_trust(ui: &Setup, side: Side, f: &Fingerprint, adapter: Option<String>) {
    let on = adapter.is_some();
    let r = update(|c| {
        let fp = server_mut(c, side)
            .fingerprints
            .iter_mut()
            .find(|x| (x.gateway_mac, x.server_lan_ip) == (f.gateway_mac, f.server_lan_ip))
            .ok_or("This network was forgotten in the meantime.")?;
        fp.trust_vpn = on;
        fp.trusted_adapter = adapter.unwrap_or_default();
        Ok(())
    });
    st(|s| s.trust.remove(&trust_key(side, f)));
    match r {
        Ok(cfg) => {
            st(|s| s.cfg = cfg);
            show_config(ui);
            let ok = if on { "Saved. Ferry trusts this VPN for copies on this network." } else { "Saved. Ferry no longer trusts this VPN for copies." };
            send(ui, vec![Request::ReloadConfig], move |ui, r| {
                put_server_note(&ui.global::<S>(), side, after_save(r.map_err(|(_, e)| e), ok));
                refresh_path(ui, side);
            });
        }
        Err(e) => {
            show_config(ui);
            put_server_note(&ui.global::<S>(), side, problem(e));
        }
    }
}

/// First run: "The destination server is empty right now" (spec 4.4's first-trip rule), set to what
/// the switch now shows; a save or reload problem shows under it.
fn set_destination_empty(ui: &Setup, on: bool) {
    let g = ui.global::<S>();
    match update(|c| {
        c.options.destination_starts_empty = on;
        Ok(())
    }) {
        Ok(cfg) => {
            g.set_destination_empty(cfg.options.destination_starts_empty);
            g.set_destination_empty_note(Note::default());
            st(|s| s.cfg = cfg);
            send(ui, vec![Request::ReloadConfig], |ui, r| {
                ui.global::<S>().set_destination_empty_note(after_save(r.map_err(|(_, e)| e), ""));
            });
        }
        Err(e) => g.set_destination_empty_note(problem(e)),
    }
}

// ---------- settings ----------

fn show_settings(g: &S, c: &Config) {
    let o = &c.options;
    g.set_cap_gb((o.cap_bytes / GB).to_string().into());
    g.set_free_gb((o.min_free_bytes / GB).to_string().into());
    g.set_cargo_dir(o.cargo_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default().into());
    g.set_battery(o.pause_on_battery_saver);
    g.set_autostart(o.start_with_windows);
    g.set_notify(o.notify_unload_done);
    g.set_auto_update(o.auto_update);
}

fn save_settings(ui: &Setup) {
    let g = ui.global::<S>();
    let fields = (|| {
        let cap = gb_field(&g.get_cap_gb()).map_err(|e| format!("Space for cargo: {e}"))?;
        if cap == 0 {
            return Err("Space for cargo must be at least 1 GB.".to_string());
        }
        let free = gb_field(&g.get_free_gb()).map_err(|e| format!("Keep free: {e}"))?;
        let dir = g.get_cargo_dir().trim().to_string();
        let dir = if dir.is_empty() { None } else { Some(PathBuf::from(dir)) };
        if dir.as_ref().is_some_and(|d| !d.is_absolute()) {
            return Err(r"Enter the cargo folder as a full path, like D:\Ferry cargo.".into());
        }
        Ok((cap, free, dir))
    })();
    let (cap, free, dir) = match fields {
        Ok(f) => f,
        Err(e) => return g.set_settings_note(problem(e)),
    };
    let (battery, autostart, notify, auto_update) = (g.get_battery(), g.get_autostart(), g.get_notify(), g.get_auto_update());
    match update(|c| {
        // Cargo already carried stays where it is, so it must be delivered before the folder moves.
        let default = state_path("cargo").unwrap_or_default();
        let carried = state_path("manifest.json").and_then(|p| CargoManifest::load(&p).ok()).map_or(0, |m| m.items.len());
        let (old, new) = (c.options.cargo_dir.as_ref().unwrap_or(&default), dir.as_ref().unwrap_or(&default));
        cargo_move_ok(old, new, carried, &Names::of(c))?;
        let o = &mut c.options;
        (o.cap_bytes, o.min_free_bytes, o.cargo_dir) = (cap, free, dir);
        (o.pause_on_battery_saver, o.start_with_windows, o.notify_unload_done) = (battery, autostart, notify);
        o.auto_update = auto_update;
        Ok(())
    }) {
        Ok(cfg) => saved(ui, cfg, "Saved.", |g: &S, n| g.set_settings_note(n)),
        Err(e) => g.set_settings_note(problem(e)),
    }
}

// ---------- backup ----------

fn export_backup(ui: &Setup) {
    let owner = ui.window().with_winit_window(win::hwnd).flatten();
    let Some(to) = win::pick_backup(owner, true, "Ferry backup.json") else { return };
    let r = (|| {
        let config = config_path()?;
        let (Some(manifest), Some(deletes), Some(snapshots)) = (state_path("manifest.json"), state_path("deletes.json"), state_path("snapshots"))
        else {
            return Err("LOCALAPPDATA is not set.".to_string());
        };
        let files = backup::Files { config: &config, manifest: &manifest, deletes: &deletes, snapshots: &snapshots };
        let text = backup::export(&files, env!("CARGO_PKG_VERSION")).map_err(|e| format!("Couldn't export: {e}"))?;
        std::fs::write(&to, text).map_err(|e| format!("Couldn't save {}: {e}", to.display()))
    })();
    ui.global::<S>().set_backup_note(match r {
        Ok(()) => note(format!("Saved to {}. It has your server addresses and user names, but no passwords.", to.display()), DONE),
        Err(e) => problem(e),
    });
}

/// The engine restores (it owns the state files), then the window shows the restored settings
/// and opens Locations to ask for the passwords, which a backup never holds.
fn restore_backup(ui: &Setup) {
    let owner = ui.window().with_winit_window(win::hwnd).flatten();
    let Some(from) = win::pick_backup(owner, false, "") else { return };
    ui.global::<S>().set_backup_note(note("Restoring...", WORKING));
    send(ui, vec![Request::Restore { path: from.to_string_lossy().into_owned() }], |ui, r| {
        let g = ui.global::<S>();
        let cfg = match r.map_err(|(_, e)| e).and_then(|()| load()) {
            Ok(c) => c,
            Err(e) => return g.set_backup_note(problem(e)),
        };
        show_servers(&g, &cfg);
        show_settings(&g, &cfg);
        st(|s| s.cfg = cfg);
        show_config(ui);
        g.set_backup_note(note("Restored.", DONE));
        g.set_first_run(false);
        go(ui, 2);
        for side in [Side::Source, Side::Destination] {
            let (set, name) = st(|s| (!server(&s.cfg, side).lan_host.is_empty(), display_name(&s.cfg, side)));
            if set {
                put_server_note(&g, side, problem(format!("Restored. Backups don't hold passwords: enter the password for {name}, then choose Save {name} server.")));
            }
        }
    });
}

// ---------- pure helpers ----------

/// Cargo id for a new pair: slug of the source path's last part, unique among `existing`.
pub fn pair_id(source_path: &str, existing: &[String]) -> String {
    let last = source_path.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or("");
    let dashed: String = last.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let slug = dashed.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let base = if slug.is_empty() { "folder".to_string() } else { slug };
    let taken = |id: &str| existing.iter().any(|e| e == id);
    (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| !taken(id)).expect("unbounded")
}

/// Swap item `i` with the one above it; `i == 0` (or out of range) leaves `v` alone.
pub fn move_up<T>(v: &mut [T], i: usize) {
    if i > 0 && i < v.len() {
        v.swap(i - 1, i);
    }
}

/// Whole gigabytes typed in a field, as bytes.
pub fn gb_field(s: &str) -> Result<u64, String> {
    s.trim().parse::<u64>().ok().and_then(|n| n.checked_mul(GB)).ok_or_else(|| format!("Enter a whole number of GB, not \"{}\".", s.trim()))
}

/// A share-relative path as typed ("repositories", "backups/vm"), normalized to forward slashes.
pub fn share_path(s: &str) -> Result<String, String> {
    let p = s.trim().replace('\\', "/");
    if p.is_empty() {
        return Err("Enter a folder path, like repositories or backups/vm.".into());
    }
    if p.starts_with('/') || p.ends_with('/') {
        return Err("Leave out slashes at the start and end: repositories, not /repositories/.".into());
    }
    if p.contains(':') {
        return Err("Enter a path on the server's share, not a drive letter.".into());
    }
    if p.split('/').any(|part| part.is_empty() || part == "..") {
        return Err("A path can't contain .. or empty parts.".into());
    }
    Ok(p)
}

/// A server card's addresses as typed: the address on its network is required, the remote
/// address is optional (sneakernet without it, spec 4.4).
pub fn server_fields(lan: &str, remote: &str, user: &str, password: &str) -> Result<(String, String), String> {
    let lan = host_field(lan)?;
    if lan.is_empty() {
        return Err("Enter the server's address on its network, like 192.0.2.10.".into());
    }
    if !password.is_empty() && user.trim().is_empty() {
        return Err("Enter the user name that goes with this password.".into());
    }
    Ok((lan, host_field(remote)?))
}

/// What the user calls a place, as typed: trimmed, at most 24 characters, empty for the role name.
pub fn name_field(s: &str) -> Result<String, String> {
    let name = s.trim();
    if name.chars().count() > 24 {
        return Err("Keep the name to 24 characters or fewer.".into());
    }
    Ok(name.to_string())
}

/// Why `side`'s name can't be kept: it matches the other side's (ignoring case, after trimming,
/// with an empty name counting as its role name).
fn name_conflict(c: &Config, side: Side) -> Option<String> {
    let other = if side == Side::Source { Side::Destination } else { Side::Source };
    let (mine, theirs) = (config::chosen_name(c, side), config::chosen_name(c, other));
    (mine.to_lowercase() == theirs.to_lowercase()).then(|| format!("Pick a different name from {theirs}."))
}

/// A server name or address as typed; a leading `\\` is dropped.
pub fn host_field(s: &str) -> Result<String, String> {
    let h = s.trim().trim_start_matches('\\');
    if h.contains(['\\', '/']) || h.chars().any(char::is_whitespace) {
        return Err(format!("Enter just the server name or address, like 192.0.2.10 or nas01, not \"{}\".", s.trim()));
    }
    Ok(h.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_view_cases() {
        let up = update_view(Ok(Response::Update { latest: None, installable: true }));
        assert_eq!(up.0, UPDATE_CURRENT);
        let new = update_view(Ok(Response::Update { latest: Some("0.2.3".into()), installable: true }));
        assert_eq!((new.0, new.1.as_str()), (UPDATE_AVAILABLE, "0.2.3"));
        let portable = update_view(Ok(Response::Update { latest: Some("0.2.3".into()), installable: false }));
        assert_eq!(portable.0, UPDATE_PORTABLE);
        assert!(portable.2.text.contains("releases page"), "{}", portable.2.text);
        let err = update_view(Err("Ferry couldn't check for updates: timed out".into()));
        assert_eq!(err.0, UPDATE_UNKNOWN);
        assert_eq!(err.2.kind, PROBLEM);
        assert!(err.2.text.contains("timed out"));
    }

    fn n() -> Names {
        Names { src: "HQ".into(), dest: "Lake house".into() }
    }

    #[test]
    fn pair_id_is_unique_slug() {
        assert_eq!(pair_id("backups/vm", &["vm".into()]), "vm-2");
    }

    #[test]
    fn pair_id_slugs_the_last_part() {
        assert_eq!(pair_id("repositories", &[]), "repositories");
        assert_eq!(pair_id("Backups/VM Images", &[]), "vm-images");
        assert_eq!(pair_id("backups/vm", &["vm".into(), "vm-2".into()]), "vm-3");
        assert_eq!(pair_id("a/Ünï code!", &[]), "n-code");
        assert_eq!(pair_id("!!!", &[]), "folder");
    }

    #[test]
    fn move_pair_reorders() {
        let mut v = vec!['a', 'b', 'c'];
        move_up(&mut v, 1);
        assert_eq!(v, ['b', 'a', 'c']);
        move_up(&mut v, 0);
        assert_eq!(v, ['b', 'a', 'c']);
        move_up(&mut v, 3);
        assert_eq!(v, ['b', 'a', 'c']);
    }

    #[test]
    fn gb_field_parses() {
        assert_eq!(gb_field("50"), Ok(50 * GB));
        assert_eq!(gb_field(" 0 "), Ok(0));
        assert!(gb_field("abc").is_err());
        assert!(gb_field("").is_err());
        assert!(gb_field("-1").is_err());
        assert!(gb_field("99999999999999").is_err());
    }

    #[test]
    fn share_paths_are_validated() {
        assert_eq!(share_path("repositories"), Ok("repositories".into()));
        assert_eq!(share_path(" backups\\vm "), Ok("backups/vm".into()));
        for bad in ["", "  ", "/repositories", "repositories/", "\\\\office\\x", "a/../b", "..", "a//b", "c:/x"] {
            assert!(share_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn changing_a_path_gives_a_fresh_id() {
        let p = Pair { id: "repositories".into(), source_path: "repositories".into(), destination_path: "repositories".into(), ..Default::default() };
        let taken = vec!["repositories".to_string()];
        assert_eq!(edited_id(&p, "Repositories", "repositories", &taken), "repositories");
        assert_eq!(edited_id(&p, "repositories", "backups/repos", &taken), "repositories-2");
        assert_eq!(edited_id(&p, "archive", "repositories", &taken), "archive");
    }

    #[test]
    fn cargo_folder_moves_only_when_empty() {
        let (a, b) = (std::path::Path::new("C:/a"), std::path::Path::new("D:/b"));
        assert_eq!(cargo_move_ok(a, b, 3, &n()), Err("Unload at Lake house first, then change the cargo folder.".into()));
        assert_eq!(cargo_move_ok(a, b, 0, &n()), Ok(()));
        assert_eq!(cargo_move_ok(a, a, 3, &n()), Ok(()));
    }

    #[test]
    fn a_dropped_confirmation_brings_the_button_back() {
        let waiting = confirm_outcome(Err(ENGINE_TIMEOUT.into()), &n());
        assert_eq!(settle_held_note(waiting.clone(), true), waiting);
        let after = settle_held_note(waiting, false);
        assert_eq!(after, problem(LIST_CHANGED));
        assert_ne!(after.kind, DONE, "a problem note shows the button again");
        let done = confirm_outcome(Ok(()), &n());
        assert_eq!(done.text, "Confirmed. Ferry deletes them at Lake house once everything new in this folder has arrived.");
        assert_eq!(settle_held_note(done.clone(), false), done);
    }

    #[test]
    fn delete_button_names_the_count() {
        assert_eq!(delete_label(0, &n()), "Delete files at Lake house");
        assert_eq!(delete_label(1, &n()), "Delete 1 file at Lake house");
        assert_eq!(delete_label(4, &n()), "Delete 4 files at Lake house");
    }

    #[test]
    fn thresholds_explain_small_folders() {
        let (rule, why) = thresholds(10, 50, &n());
        assert_eq!(rule, "Ferry asks before deleting at Lake house when one pass would remove more than 10% of a folder's files, or more than 50 files.");
        assert_eq!(why, "Folders with fewer than 10 files ask before every delete, because one file is already more than 10% of them.");
        // 30%: 3 files (33%) ask, 4 files (25%) don't.
        assert!(thresholds(30, 50, &n()).1.starts_with("Folders with fewer than 4 files"));
        assert_eq!(thresholds(0, 50, &n()).1, "So every delete at Lake house asks first.");
        assert_eq!(thresholds(100, 50, &n()).1, "");
    }

    #[test]
    fn confirm_timeout_means_after_the_trip() {
        assert_eq!(confirm_outcome(Err(ENGINE_TIMEOUT.into()), &n()).text, "Will apply after the current trip.");
        assert_eq!(confirm_outcome(Err("No deletes are held for x.".into()), &n()), problem("No deletes are held for x."));
        assert_eq!(confirm_outcome(Ok(()), &n()).kind, DONE);
    }

    #[test]
    fn engine_errors_are_shown_verbatim() {
        let ts = "Ferry can't see the router on the network that holds 192.0.2.10. Try again in a moment.";
        assert_eq!(server_outcome("HQ", false, true, Err((1, ts.into()))), problem(ts));
        assert_eq!(outcome(Ok(Response::Err(ts.into()))), Err(ts.into()));
        assert_eq!(outcome(Err(std::io::ErrorKind::NotFound.into())), Err(NOT_RUNNING.into()));
    }

    #[test]
    fn server_outcomes() {
        let ok = |has_pw, tested| server_outcome("Lake house", has_pw, tested, Ok(()));
        assert_eq!(ok(true, true), note("Ferry reached Lake house and signed in.", DONE));
        assert_eq!(ok(false, false), note("Saved.", DONE));
        // Engine down: with a password that's a problem; without one the save still counts.
        assert_eq!(server_outcome("home", true, false, Err((0, NOT_RUNNING.into()))).kind, PROBLEM);
        assert_eq!(server_outcome("home", false, false, Err((0, NOT_RUNNING.into()))).kind, PLAIN);
        assert_eq!(server_outcome("home", true, false, Err((1, "bad".into()))), problem("Saved, but Ferry couldn't load it: bad"));
        assert_eq!(server_outcome("home", false, true, Err((0, NOT_RUNNING.into()))), problem(NOT_RUNNING));
        assert_eq!(
            server_outcome("home", true, true, Err((2, ENGINE_TIMEOUT.into()))),
            problem("Saved, but Ferry didn't answer the test. Try again in a moment.")
        );
    }

    #[test]
    fn same_source_path_is_a_duplicate() {
        let pairs = vec![Pair { id: "repositories".into(), source_path: "Repositories".into(), ..Default::default() }];
        assert_eq!(duplicate_of(&pairs, "repositories", None), Some("repositories"));
        assert_eq!(duplicate_of(&pairs, "repositories", Some("repositories")), None); // editing itself
        assert_eq!(duplicate_of(&pairs, "backups/vm", None), None);
    }

    #[test]
    fn paths_and_log_lines() {
        assert_eq!(unc_label("office-nas", "backups/vm"), r"\\office-nas\backups\vm");
        assert_eq!(unc_label("", "backups/vm"), "backups/vm");
        assert_eq!(split_line("2026-10-16 10:00:01  settings reloaded"), Line { time: "2026-10-16 10:00:01".into(), text: "settings reloaded".into() });
        assert_eq!(split_line("odd"), Line { time: "".into(), text: "odd".into() });
    }

    #[test]
    fn path_status_copy() {
        let line = |verdict: &str, adapter: Option<&str>| path_line(verdict, adapter.map(str::to_string));
        assert_eq!(line("direct", None), note("Direct on this network", DONE));
        assert_eq!(line("vpn-direct", Some("Tailnet")), note("Through Tailnet, direct on this network", DONE));
        assert_eq!(
            line("vpn-unknown", Some("WireGuard")),
            problem("Through WireGuard: can't confirm it stays local, so Ferry won't copy files here")
        );
        assert_eq!(
            line("vpn-not-direct:relayed through a relay server", Some("WireGuard")),
            problem("Through WireGuard: relayed through a relay server, so Ferry won't copy files here")
        );
        assert_eq!(line("vpn-trusted", Some("WireGuard")), note("Through WireGuard, a VPN you trust for copies on this network", PLAIN));
        assert_eq!(line("not-here", None), note("Not on this server's network right now", PLAIN));
        assert_eq!(line("blocked:No route.", None), problem("No route."));
        assert_eq!(line("something new", None), Note::default());
    }

    #[test]
    fn network_label_falls_back_to_mac() {
        let f = |name: &str| ferry_core::config::Fingerprint {
            gateway_mac: [0x00, 0x00, 0x5e, 0x00, 0x53, 0x01],
            server_lan_ip: "192.0.2.10".parse().unwrap(),
            name: name.into(),
            ..Default::default()
        };
        assert_eq!(net_name(&f("OfficeNet")), "OfficeNet");
        assert_eq!(net_name(&f("")), "Network 00:00:5e:00:53:01");
        assert_eq!(net_detail(&f("OfficeNet")), "Router 00:00:5e:00:53:01, server at 192.0.2.10");
    }

    #[test]
    fn remote_address_may_be_empty() {
        assert_eq!(server_fields("192.0.2.10", "", "", ""), Ok(("192.0.2.10".into(), String::new())));
        assert_eq!(server_fields(" 192.0.2.10 ", " office-nas ", "u", "p"), Ok(("192.0.2.10".into(), "office-nas".into())));
        assert!(server_fields("", "office-nas", "", "").is_err(), "the address on its network is required");
        assert!(server_fields("192.0.2.10", "office nas", "", "").is_err());
        assert!(server_fields("192.0.2.10", "", "", "p").is_err(), "a password needs a user name");
    }

    #[test]
    fn trust_needs_an_unconfirmed_vpn_on_this_network() {
        let (mac, ip) = ("00:00:5e:00:53:01", "192.0.2.10");
        let path = |verdict: &str, adapter: Option<&str>, router: &str, server: &str| {
            Ok(Response::Path {
                network: "OfficeNet".into(),
                adapter: adapter.map(Into::into),
                verdict: verdict.into(),
                router: router.into(),
                server: server.into(),
            })
        };
        let answer = |r| trust_answer(r, mac, ip);
        assert_eq!(answer(path("vpn-unknown", Some("WireGuard"), mac, ip)), Trust::Confirm("WireGuard".into()));
        let not_needed = Trust::Disabled(NOT_NEEDED.into());
        assert_eq!(NOT_NEEDED, "Only needed when a VPN carries this server's traffic.");
        assert_eq!(answer(path("direct", None, mac, ip)), not_needed);
        assert_eq!(answer(path("not-here", None, "", "")), not_needed);
        // On another learned network of this server: not this row's network.
        assert_eq!(answer(path("vpn-unknown", Some("WireGuard"), "aa:bb:cc:00:00:01", ip)), not_needed);
        // Same router, but the row was learned for another server address.
        assert_eq!(answer(path("vpn-unknown", Some("WireGuard"), mac, "192.0.2.11")), not_needed);
        assert!(matches!(answer(path("vpn-direct", Some("WireGuard"), mac, ip)), Trust::Disabled(_)));
        assert!(matches!(answer(path("vpn-not-direct:relayed", Some("WireGuard"), mac, ip)), Trust::Disabled(_)));
        assert_eq!(answer(path("vpn-unknown", None, mac, ip)), not_needed, "trust needs a named adapter");
        assert_eq!(answer(Err("Ferry isn't running.".into())), Trust::Retry("Ferry isn't running.".into()));
        assert_eq!(answer(Err(OLD_ENGINE.into())), Trust::Retry("Update Ferry to check this path.".into()));
    }

    /// What an engine from before PathStatus answers.
    const OLD_ENGINE: &str = "bad request: unknown variant `PathStatus`, expected one of `Status`, `Pause`";

    #[test]
    fn path_line_says_when_it_cannot_tell() {
        let path = Response::Path { network: "".into(), adapter: None, verdict: "direct".into(), router: "".into(), server: "".into() };
        assert_eq!(path_note(Ok(path)), note("Direct on this network", DONE));
        assert_eq!(path_note(Err(OLD_ENGINE.into())), note("Update Ferry to check this path.", PLAIN));
        assert_eq!(path_note(Err(NOT_RUNNING.into())), note("Ferry couldn't check this path right now.", PLAIN));
        assert_eq!(path_note(Ok(Response::Ok)), note("Ferry couldn't check this path right now.", PLAIN));
    }

    #[test]
    fn new_copy_uses_three_dots() {
        for src in [include_str!("setup.rs"), include_str!("lib.rs"), include_str!("../ui/setup/pages.slint"), include_str!("../ui/setup.slint")] {
            assert!(!src.contains('\u{2026}'), "use ... rather than the ellipsis character");
        }
    }

    #[test]
    fn names_are_trimmed_and_kept_short() {
        assert_eq!(name_field("  Lake house "), Ok("Lake house".into()));
        assert_eq!(name_field(""), Ok(String::new()));
        assert_eq!(name_field(&"x".repeat(24)), Ok("x".repeat(24)));
        assert!(name_field(&"x".repeat(25)).is_err());
        assert_eq!(name_field(&format!("  {}  ", "é".repeat(24))), Ok("é".repeat(24)), "characters, not bytes");
    }

    #[test]
    fn a_name_must_differ_from_the_other_side() {
        let mut c = Config::default();
        c.destination.name = "source".into();
        assert_eq!(name_conflict(&c, Side::Destination).as_deref(), Some("Pick a different name from Source."));
        assert_eq!(name_conflict(&c, Side::Source).as_deref(), Some("Pick a different name from source."));
        c.source.name = "Office".into();
        assert_eq!(name_conflict(&c, Side::Destination), None);
        c.destination.name = " office ".into();
        assert_eq!(name_conflict(&c, Side::Destination).as_deref(), Some("Pick a different name from Office."));
    }

    #[test]
    fn closing_names_both_places() {
        assert_eq!(
            closing(Some(Side::Source), &n()),
            "Ferry knows HQ now. It learns the network at Lake house the first time you're there: open Ferry from the tray, go to Locations, and choose Remember this network under Destination server."
        );
        assert!(closing(None, &n()).contains("At HQ and at Lake house, open Ferry"));
    }

    #[test]
    fn hosts_are_validated() {
        assert_eq!(host_field(" 192.0.2.10 "), Ok("192.0.2.10".into()));
        assert_eq!(host_field("\\\\office-nas"), Ok("office-nas".into()));
        assert_eq!(host_field(""), Ok(String::new()));
        for bad in ["office nas", "office/nas", "\\\\office\\share"] {
            assert!(host_field(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
