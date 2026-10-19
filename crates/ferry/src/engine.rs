//! The engine: a single-threaded state machine running inside the tray's message loop.
//! Anything that can block (location resolution, trips, Remember, TestConnection) runs on a
//! worker thread and reports back through the one `Input` channel.
use crate::log::{self, ActivityLog};
use crate::trip::job::{self, Kind, Report, TripResult};
use crate::trip::{purge_unknown, Event, UnloadReport};
use crate::win::tray::{self, Ev, Icon, MenuCmd};
use crate::update::{self, Outcome};
use crate::win::{http, net, power, smb, toast};
use ferry_core::backup;
use ferry_core::cargo::CargoManifest;
use ferry_core::config::{self, display_name, Config, Fingerprint, Server};
use ferry_core::deletes::{list_id, DeleteQueue};

/// The answer to a confirmation whose list isn't the one held now.
const LIST_CHANGED: &str = "The list of deletes changed; review it again.";
use ferry_core::entry::list_dir;
use ferry_core::ipc::{PairStatus, Phase, Request, Response, Side, Status};
use ferry_core::location::{locate, reason_text, Adapter, BulkPath, Located, Location, PathVerdict};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io, thread};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

const T_DEBOUNCE: usize = 1;
const T_RECHECK: usize = 2;
const T_PAUSE: usize = 3;
const T_QUIT: usize = 4;
const T_UPDATE: usize = 5;

/// How often the engine considers an automatic update check (it checks at most once a day).
const UPDATE_TICK_MS: u64 = 10 * 60_000;

/// At the destination, after an unload that was skipped or left something behind: one more try this long after.
const DESTINATION_RETRY_MS: u64 = 15 * 60_000;

pub enum Input {
    NetChanged,
    Timer(usize),
    TripDone(TripResult),
    Ipc(Request, Sender<Response>),
    TrayClick,
    TrayMenu(MenuCmd),
    PowerChanged,
    Located(Located),
    TripEvent(Event),
    Remembered(Side, Fingerprint, Sender<Response>),
    /// A line for the activity log from a worker thread.
    Log(String),
    /// An update check (and download) finished; `true` when the user asked for it.
    Update(update::Outcome, bool),
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub manifest: PathBuf,
    pub deletes: PathBuf,
    pub activity: PathBuf,
    /// Oversize files already toasted, so each is announced once.
    pub notified: PathBuf,
    /// Default cargo folder.
    pub cargo: PathBuf,
    /// Destination snapshots, one per pair (spec 4.4).
    pub snapshots: PathBuf,
    /// Downloaded updates, and the files that tell the next start how an install went.
    pub update: PathBuf,
}

impl Paths {
    /// Config in `%APPDATA%\Ferry`, state in `%LOCALAPPDATA%\Ferry`; both folders are created.
    pub fn new() -> io::Result<Self> {
        let dir = |var: &str| -> io::Result<PathBuf> {
            let base = std::env::var_os(var).ok_or_else(|| io::Error::other(format!("{var} is not set")))?;
            let d = PathBuf::from(base).join("Ferry");
            fs::create_dir_all(&d)?;
            Ok(d)
        };
        let (cfg, state) = (dir("APPDATA")?, dir("LOCALAPPDATA")?);
        Ok(Self {
            config: cfg.join("config.toml"),
            manifest: state.join("manifest.json"),
            deletes: state.join("deletes.json"),
            activity: state.join("activity.log"),
            notified: state.join("oversize-notified.json"),
            cargo: state.join("cargo"),
            snapshots: state.join("snapshots"),
            update: state.join("update"),
        })
    }
}

/// Sends inputs to the engine from any thread and wakes the message loop.
#[derive(Clone)]
pub struct Waker {
    tx: Sender<Input>,
    hwnd: usize,
}

impl Waker {
    /// Never fails or panics: after shutdown the input is simply dropped.
    pub fn send(&self, i: Input) {
        if self.tx.send(i).is_ok() {
            tray::wake(self.hwnd);
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// IPC handler side: forward to the engine and wait for its answer.
fn ask(w: &Waker, req: Request) -> Response {
    let (tx, rx) = mpsc::channel();
    w.send(Input::Ipc(req, tx));
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Response::Err("Ferry did not answer in time.".into()))
}

/// Run the engine on this (main) thread until Quit; returns the process exit code.
pub fn run() -> i32 {
    let paths = match Paths::new() {
        Ok(p) => p,
        Err(e) => return fail(&format!("state folders: {e}")),
    };
    let mut log = match ActivityLog::open(&paths.activity) {
        Ok(l) => l,
        Err(e) => return fail(&format!("activity log: {e}")),
    };
    if !crate::win::single_instance() {
        return 0; // another engine is running
    }
    let hwnd = match tray::create() {
        Ok(h) => h,
        Err(e) => {
            log.line(&format!("tray icon failed: {e}"));
            return 1;
        }
    };
    let (tx, rx) = mpsc::channel();
    let waker = Waker { tx, hwnd: hwnd.0 as usize };
    let ipc = waker.clone();
    if let Err(e) = crate::ipc_server::serve(&ferry_core::ipc::pipe(), Arc::new(move |req| ask(&ipc, req))) {
        log.line(&format!("another engine owns the pipe, exiting: {e}"));
        tray::quit(hwnd);
        return 1;
    }
    if let Err(e) = toast::ensure_shortcut() {
        log.line(&format!("Start Menu shortcut (needed for notifications): {e}"));
    }
    let w = waker.clone();
    let watch = net::watch(Box::new(move || w.send(Input::NetChanged)));
    if let Err(e) = &watch {
        log.line(&format!("network change watch failed: {e}"));
    }
    let mut engine = Engine::new(hwnd, waker, paths, log);
    tray::run(move |ev| match ev {
        Ev::Wake => {
            while let Ok(i) = rx.try_recv() {
                engine.handle(i);
            }
        }
        Ev::Timer(id) => engine.handle(Input::Timer(id)),
        Ev::Click => engine.handle(Input::TrayClick),
        Ev::Menu(c) => engine.handle(Input::TrayMenu(c)),
        Ev::PowerChanged => engine.handle(Input::PowerChanged),
        Ev::Resumed => engine.handle(Input::NetChanged), // woke from sleep: maybe somewhere else
    });
    drop(watch);
    0
}

fn fail(msg: &str) -> i32 {
    eprintln!("ferry: {msg}");
    1
}

struct Trip {
    kind: Kind,
    cancel: Arc<AtomicBool>,
    first_progress: Option<Instant>,
    moved: u64,
    cur: (String, String),
    cur_done: u64,
}

pub struct Engine {
    hwnd: HWND,
    waker: Waker,
    paths: Paths,
    log: ActivityLog,
    cfg: Config,
    sched: Sched,
    paused_until: Option<i64>,
    trip: Option<Trip>,
    /// The first location has been resolved.
    started: bool,
    /// Toasts already shown at this location (cleared when the location changes).
    toasted: HashSet<String>,
    /// ConfirmDeletes that arrived during a trip: (pair, the paths queued then, the count the
    /// user saw, reply). Applied when the trip ends, only if the list is unchanged.
    confirms: Vec<(String, Vec<String>, u32, Sender<Response>)>,
    /// Per pair, this trip: (carried, deleted at the destination).
    counts: HashMap<String, (u32, u32)>,
    status: Status,
    /// This exe is the installed copy, so it may update itself.
    installed: bool,
    /// Unix time of the last update check that reached a verdict.
    update_ok_at: i64,
    /// No automatic check before this (an hour after a failed one).
    update_retry_at: i64,
    /// The last update check failed (so the next failure isn't logged again).
    update_failing: bool,
    /// An update check or download is running.
    updating: bool,
    /// A verified installer waiting for a moment when no trip runs: (version, path, its SHA-256,
    /// user asked).
    ready: Option<(String, PathBuf, String, bool)>,
    /// Checksum mismatches in a row: (version, count).
    bad_sums: (String, u32),
    /// The user chose "Update to X.Y.Z" while an automatic check was already running.
    manual_asked: bool,
}

impl Engine {
    fn new(hwnd: HWND, waker: Waker, paths: Paths, mut log: ActivityLog) -> Self {
        log.line("Ferry started");
        let mut attention = vec![];
        let cfg = config::load(&paths.config).unwrap_or_else(|e| {
            log.line(&format!("settings file unreadable, using defaults: {e}"));
            attention.push("The settings file could not be read".to_string());
            Config::default()
        });
        // Elsewhere and waiting; nothing known to be wrong until a trip says so.
        let status = Status { remote_ok: true, cap_bytes: cfg.options.cap_bytes, attention, ..Default::default() };
        let mut e = Engine {
            hwnd,
            waker,
            paths,
            log,
            cfg,
            sched: Sched::new(),
            paused_until: None,
            trip: None,
            started: false,
            toasted: HashSet::new(),
            confirms: vec![],
            counts: HashMap::new(),
            status,
            installed: update::installed(),
            update_ok_at: 0,
            update_retry_at: 0,
            update_failing: false,
            updating: false,
            ready: None,
            bad_sums: (String::new(), 0),
            manual_asked: false,
        };
        e.after_update();
        tray::set_timer(e.hwnd, T_UPDATE, UPDATE_TICK_MS);
        e.apply_autostart();
        e.refresh_files();
        e.resolve(); // first location without waiting for a network change
        e.update_tray();
        e
    }

    pub fn handle(&mut self, input: Input) {
        match input {
            Input::NetChanged => {
                self.sched.net_changed();
                tray::set_timer(self.hwnd, T_DEBOUNCE, 3000);
            }
            Input::Timer(T_DEBOUNCE) => {
                tray::kill_timer(self.hwnd, T_DEBOUNCE);
                if self.sched.debounce_fired() {
                    self.spawn_resolver();
                }
            }
            Input::Timer(T_RECHECK) => {
                // At the source it repeats; anywhere else it was a single retry.
                if self.sched.loc != Location::Source {
                    tray::kill_timer(self.hwnd, T_RECHECK);
                }
                self.trigger()
            }
            Input::Timer(T_PAUSE) => self.resume(),
            Input::Timer(T_QUIT) => tray::quit(self.hwnd),
            Input::Timer(T_UPDATE) => self.auto_update(),
            Input::Timer(_) => {}
            Input::PowerChanged if power::battery_saver_on() => self.evaluate(), // stops a running trip
            Input::PowerChanged => self.trigger(),
            Input::Log(line) => self.log.line(&line),
            Input::Located(l) => self.located(l),
            Input::TripEvent(e) => self.trip_event(e),
            Input::TripDone(r) => self.trip_done(r),
            Input::Ipc(req, reply) => self.ipc(req, reply),
            Input::Remembered(side, fp, reply) => self.remembered(side, fp, reply),
            Input::Update(o, manual) => self.update_done(o, manual),
            Input::TrayClick => self.spawn_ui("--flyout"),
            Input::TrayMenu(cmd) => match cmd {
                MenuCmd::Open => self.spawn_ui("--setup"),
                MenuCmd::Pause1h => self.pause(Some(60)),
                MenuCmd::PauseForever => self.pause(None),
                MenuCmd::Resume => self.resume(),
                MenuCmd::Quit => tray::quit(self.hwnd),
            },
        }
        self.update_tray();
    }

    fn paused(&self) -> bool {
        self.paused_until.is_some_and(|t| t > unix_now())
    }

    fn server(&self, side: Side) -> &Server {
        match side {
            Side::Source => &self.cfg.source,
            Side::Destination => &self.cfg.destination,
        }
    }

    /// Fresh location wanted (startup, settings changed).
    fn resolve(&mut self) {
        if self.sched.resolve() {
            self.spawn_resolver();
        }
    }

    /// A trip should run, but only after the location is re-checked.
    fn trigger(&mut self) {
        if self.sched.trigger() {
            self.spawn_resolver();
        }
    }

    /// `adapters()` can block for seconds (ARP), so resolve on a worker.
    fn spawn_resolver(&mut self) {
        let (cfg, w) = (self.cfg.clone(), self.waker.clone());
        thread::spawn(move || {
            // A place is recognised even when its bulk path is blocked; `located` decides on trips.
            let l = match net::adapters() {
                Ok(a) => locate(&cfg, &a, &net::best_interface, &net::probe_445, &crate::vpn::check),
                Err(_) => Located::ELSEWHERE,
            };
            w.send(Input::Located(l));
        });
    }

    fn located(&mut self, l: Located) {
        let loc = l.location;
        let resolved = self.sched.located(loc, l.bulk.as_ref().is_some_and(BulkPath::allows_bulk));
        if resolved == Resolved::Stale {
            return self.spawn_resolver(); // changed again meanwhile
        }
        let (blocked, trusting) = bulk_status(&l);
        if let Some(why) = blocked.as_ref().filter(|b| self.status.bulk_blocked.as_ref() != Some(*b)) {
            self.log.line(&format!("not copying at {}: {why}", place(&self.cfg, loc)));
        }
        self.status.bulk_blocked = blocked;
        self.status.trusting_vpn = trusting;
        let first = !std::mem::replace(&mut self.started, true);
        if first {
            crate::win::trim_memory(); // startup is done: drop its pages
        }
        match resolved {
            Resolved::Stale => {}
            Resolved::Same if first => self.log.line(&format!("location: {}", place(&self.cfg, loc))),
            Resolved::Same => {}
            Resolved::Changed => {
                self.log.line(&format!("network changed → {}", place(&self.cfg, loc)));
                self.toasted.clear();
                // A trip belongs to the place it started; a new place gets its own trip.
                if let Some(t) = &self.trip {
                    t.cancel.store(true, Ordering::Relaxed);
                }
                self.arm_recheck();
            }
        }
        self.arm_destination_retry();
        self.evaluate();
    }

    /// At the destination with copying blocked, nothing else looks again: one more check later (as after
    /// an undone unload), but only while there is work waiting.
    fn arm_destination_retry(&self) {
        if self.sched.loc != Location::Destination || self.sched.bulk {
            return;
        }
        let m = CargoManifest::load(&self.paths.manifest).unwrap_or_default();
        let q = DeleteQueue::load(&self.paths.deletes).unwrap_or_default();
        let snapped = |id: &str| crate::trip::snap_path(&self.paths.snapshots, id).exists();
        if destination_work_waiting(&self.cfg.pairs, &m, &q, &snapped) {
            tray::set_timer(self.hwnd, T_RECHECK, DESTINATION_RETRY_MS);
        }
    }

    /// The source re-check runs only while at the source (spec 6.7).
    fn arm_recheck(&self) {
        if self.sched.loc == Location::Source {
            let minutes = u64::from(self.cfg.options.recheck_minutes.max(1));
            tray::set_timer(self.hwnd, T_RECHECK, minutes * 60_000);
        } else {
            tray::kill_timer(self.hwnd, T_RECHECK);
        }
    }

    /// Spec 4.4: an unload at the destination records the destination snapshots. Without a remote address for the destination
    /// every visit records; with one, only a pair still missing its snapshot needs the trip.
    fn record_destination(&self) -> bool {
        self.cfg.destination.remote_host.is_empty()
            || self.cfg.pairs.iter().any(|p| p.enabled && !crate::trip::snap_path(&self.paths.snapshots, &p.id).exists())
    }

    fn evaluate(&mut self) {
        let m = CargoManifest::load(&self.paths.manifest).unwrap_or_default();
        let q = DeleteQueue::load(&self.paths.deletes).unwrap_or_default();
        let action = decide_action(
            self.sched.loc,
            self.sched.bulk,
            self.paused(),
            power::battery_saver_on(),
            self.cfg.options.pause_on_battery_saver,
            self.trip.is_some(),
            !m.items.is_empty(),
            q.pairs.values().any(|p| !p.paths.is_empty()),
            self.record_destination(),
        );
        match action {
            Action::Cancel => {
                if let Some(t) = &self.trip {
                    if !t.cancel.swap(true, Ordering::Relaxed) {
                        self.log.line("trip stopped");
                    }
                }
            }
            Action::StartFill if self.sched.may_start() => self.start(Kind::Fill),
            Action::StartUnload if self.sched.may_start() => self.start(Kind::Unload),
            _ => {}
        }
    }

    fn start(&mut self, kind: Kind) {
        // A disabled pair is still unloaded (Ruling R31), but only enabled ones are filled.
        if !self.cfg.pairs.iter().any(|p| p.enabled || kind == Kind::Unload) {
            return; // nothing set up yet: stay idle
        }
        self.sched.trip_started();
        let cancel = Arc::new(AtomicBool::new(false));
        let (cfg, paths, w, c) = (self.cfg.clone(), self.paths.clone(), self.waker.clone(), cancel.clone());
        let spawned = thread::Builder::new().name("trip".into()).spawn(move || {
            let ev = w.clone();
            let r = job::run(kind, &cfg, &paths, &c, &mut |e| ev.send(Input::TripEvent(e)));
            w.send(Input::TripDone(r));
        });
        if let Err(e) = spawned {
            self.log.line(&format!("could not start a trip: {e}"));
            return;
        }
        self.log.line(if kind == Kind::Fill { "fill started" } else { "unload started" });
        self.status.attention.clear();
        self.counts.clear();
        self.trip = Some(Trip { kind, cancel, first_progress: None, moved: 0, cur: Default::default(), cur_done: 0 });
    }

    fn trip_event(&mut self, e: Event) {
        let (src, dest) = (display_name(&self.cfg, Side::Source), display_name(&self.cfg, Side::Destination));
        match e {
            Event::Progress { pair, rel, done, total } => self.progress(pair, rel, done, total),
            Event::Carried { pair, rel, size } => {
                self.log.line(&format!("carried {rel} ({pair}, {})", mb(size)));
                self.status.carried_bytes += size;
                self.counts.entry(pair).or_default().0 += 1;
            }
            Event::Pushed { pair, rel, size } => {
                self.log.line(&format!("delivered {rel} to {dest} ({pair}, {})", mb(size)));
                self.status.carried_bytes = self.status.carried_bytes.saturating_sub(size);
            }
            Event::Deleted { pair, rel } => {
                self.log.line(&format!("deleted {rel} at {dest} ({pair})"));
                self.counts.entry(pair).or_default().1 += 1;
            }
            Event::Oversize { pair, rel, size } => {
                self.log.line(&format!("bigger than the cap, not carried: {rel} ({pair}, {})", mb(size)));
                self.attend(format!("{rel} is bigger than the cap"));
                if self.first_oversize(&pair, &rel) {
                    let body = format!("{rel} is bigger than the cap, so Ferry can't carry it. Raise the cap to include it.");
                    self.notify(None, "File too big to carry", &body);
                }
            }
            Event::HeldForConfirm { pair, count } => {
                self.log.line(&format!("{count} deletes at {dest} held for your OK ({pair})"));
                self.attend(format!("Confirm {count} deletes in {pair}"));
                let body = format!("{count} files would be removed at {dest} in {pair}. Open Ferry to confirm.");
                self.notify(Some(format!("held {pair}")), "Deletes need your OK", &body);
            }
            Event::Info { msg, .. } => self.log.line(&msg),
            Event::StateReset { manifest, aside } => {
                let what = if manifest { "cargo list" } else { "delete queue" };
                self.log.line(&format!("{what} was damaged and was reset; the old file is {}", aside.display()));
                if manifest {
                    self.attend("Ferry's cargo list was damaged and was reset. Files already on the laptop will be carried again.".into());
                }
            }
            Event::VerifyFailed { pair, rel } => {
                self.log.line(&format!("copy check failed, kept on the laptop: {rel} ({pair})"));
                self.attend(format!("{rel} failed its copy check"));
                let body = format!("{rel} did not match after copying to {dest}. Ferry kept the laptop copy and will try again.");
                self.notify(Some(format!("verify {pair} {rel}")), "Copy check failed", &body);
            }
            Event::Error { pair, msg } => {
                self.log.line(&if pair.is_empty() { format!("error: {msg}") } else { format!("error ({pair}): {msg}") });
                if let Some(a) = error_attention(&pair, &msg, &src, &dest) {
                    self.attend(a);
                }
                let unloading = self.trip.as_ref().is_some_and(|t| t.kind == Kind::Unload);
                if unloading && job::is_disk_full(&msg) {
                    self.attend(format!("{dest} is full"));
                    let body = "Unloading stopped. Your files stay on the laptop until there is room.";
                    self.notify(Some("disk full".into()), &format!("{dest} is full"), body);
                }
            }
        }
    }

    fn progress(&mut self, pair: String, rel: String, done: u64, total: u64) {
        let Some(t) = &mut self.trip else { return };
        if t.cur.0 != pair || t.cur.1 != rel || done < t.cur_done {
            t.cur = (pair, rel);
            t.cur_done = 0;
        }
        t.moved += done - t.cur_done;
        t.cur_done = done;
        let secs = t.first_progress.get_or_insert_with(Instant::now).elapsed().as_secs_f64();
        let rate = if secs >= 1.0 { (t.moved as f64 / secs) as u64 } else { 0 };
        self.status.rate_bps = rate;
        // ponytail: ETA of the current file only; a fill's load isn't known up front.
        self.status.eta_secs = (rate > 0).then(|| total.saturating_sub(done) / rate);
    }

    fn trip_done(&mut self, r: TripResult) {
        let trip = self.trip.take();
        let cancelled = trip.as_ref().is_some_and(|t| t.cancel.load(Ordering::Relaxed));
        let unload = trip.is_some_and(|t| t.kind == Kind::Unload);
        let lost = r.path_lost;
        let (src, dest) = (display_name(&self.cfg, Side::Source), display_name(&self.cfg, Side::Destination));
        // At the destination, anything left undone gets one more try later, not only at the next arrival.
        if retry_at_destination(unload, &r) && self.sched.loc == Location::Destination {
            tray::set_timer(self.hwnd, T_RECHECK, DESTINATION_RETRY_MS);
        }
        self.status.rate_bps = 0;
        self.status.eta_secs = None;
        if !matches!(r.report, Report::WrongPlace { .. }) {
            self.status.remote_ok = r.remote_ok;
        }
        match r.report {
            Report::WrongPlace { why, blocked_at } => {
                // Ruling R21: the worker re-checked and this is not where the trip belongs.
                self.log.line(&format!("trip not started: {why}"));
                if self.sched.blocked_at_start(blocked_at) {
                    self.trigger();
                } else {
                    // Blocked right here: show why and let the timers or a network change look
                    // again, rather than re-checking at once (a checker that answers differently
                    // each time would start and stop trips back to back).
                    self.status.bulk_blocked = Some(why);
                    self.status.trusting_vpn = false;
                    self.arm_destination_retry();
                }
            }
            Report::Fill(Ok(rep)) => {
                let done = if lost { format!("fill stopped, the path to {src} is no longer allowed") } else { "fill done".into() };
                self.log.line(&format!(
                    "{done}: {} carried ({}), {} deleted at {dest}",
                    rep.carried,
                    mb(rep.carried_bytes),
                    rep.deleted
                ));
                let trips = rep.outstanding_bytes.div_ceil(self.cfg.options.cap_bytes.max(1));
                self.status.trips_behind = Some(trips.min(u64::from(u32::MAX)) as u32);
            }
            Report::Unload(Ok(rep)) => {
                if let Some(line) = unload_line(&rep, lost, &src, &dest) {
                    self.log.line(&line);
                }
                if rep.first_records > 0 && self.cfg.options.destination_starts_empty {
                    match forget_destination_empty(&self.paths.config) {
                        Ok(Some(cfg)) => {
                            self.cfg = cfg;
                            self.log.line(&format!("recorded what's at {dest}; the '{dest} is empty' answer is no longer needed"));
                        }
                        Ok(None) => self.cfg.options.destination_starts_empty = false,
                        Err(e) => self.log.line(&format!("settings: {e}")),
                    }
                }
                if self.cfg.options.notify_unload_done && !cancelled && rep.failed == 0 && rep.pushed > 0 {
                    self.notify(None, "Unload finished", &format!("{} files delivered to {dest}.", rep.pushed));
                }
            }
            Report::Fill(Err(e)) | Report::Unload(Err(e)) => {
                self.log.line(&format!("trip failed: {e}"));
                self.attend(format!("The last trip failed: {e}"));
            }
            Report::Skipped { title, body } => {
                self.log.line(&format!("trip skipped: {title}. {body}"));
                self.attend(title.clone());
                self.notify(Some(format!("{title} {body}")), &title, &body);
            }
        }
        if lost {
            // Spec 4.3: the path stopped being allowed mid-trip; show why, start nothing until it's back.
            self.trigger();
        }
        for (pair, seen, count, reply) in std::mem::take(&mut self.confirms) {
            let r = self.confirm(&pair, Some(&seen), count, 0);
            if let Response::Err(e) = &r {
                self.log.line(&format!("delete confirmation for {pair} dropped: {e}"));
            }
            let _ = reply.send(r);
        }
        if self.install_ready() {
            return; // quitting for the installer
        }
        self.refresh_files();
        self.evaluate();
        if self.trip.is_none() {
            crate::win::trim_memory();
        }
    }

    fn attend(&mut self, s: String) {
        if !self.status.attention.contains(&s) {
            self.status.attention.push(s);
        }
    }

    /// Toast off-thread (WinRT can block). `key`: shown once per location.
    fn notify(&mut self, key: Option<String>, title: &str, body: &str) {
        if key.is_some_and(|k| !self.toasted.insert(k)) {
            return;
        }
        let (t, b, w) = (title.to_string(), body.to_string(), self.waker.clone());
        thread::spawn(move || {
            // WinRT needs COM on this thread; MTA never pumps messages.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if let Err(e) = toast::toast(&t, &b) {
                w.send(Input::Log(format!("notification failed: {e}")));
            }
        });
    }

    /// True the first time (pair, rel) is reported oversize, ever (remembered on disk).
    // ponytail: the set only grows; prune entries no longer at the source if it ever matters.
    fn first_oversize(&self, pair: &str, rel: &str) -> bool {
        let mut seen: BTreeSet<(String, String)> =
            fs::read(&self.paths.notified).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let new = seen.insert((pair.to_string(), rel.to_string()));
        if new {
            let _ = fs::write(&self.paths.notified, serde_json::to_vec(&seen).unwrap_or_default());
        }
        new
    }

    fn pause(&mut self, minutes: Option<u32>) {
        match minutes {
            Some(m) => {
                self.paused_until = Some(unix_now() + i64::from(m) * 60);
                tray::set_timer(self.hwnd, T_PAUSE, u64::from(m) * 60_000);
                self.log.line(&format!("paused for {m} minutes"));
            }
            None => {
                self.paused_until = Some(i64::MAX);
                tray::kill_timer(self.hwnd, T_PAUSE);
                self.log.line("paused until resumed");
            }
        }
        self.evaluate(); // stops a running trip
    }

    fn resume(&mut self) {
        tray::kill_timer(self.hwnd, T_PAUSE);
        if self.paused_until.take().is_some() {
            self.log.line("resumed");
        }
        self.trigger();
    }

    fn ipc(&mut self, req: Request, reply: Sender<Response>) {
        let r = match req {
            Request::Status => {
                self.update_tray();
                Response::Status(self.status.clone())
            }
            Request::Pause { minutes } => {
                self.pause(minutes);
                Response::Ok
            }
            Request::Resume => {
                self.resume();
                Response::Ok
            }
            Request::ConfirmDeletes { pair, count, list } if self.trip.is_some() => {
                // The trip owns deletes.json until it ends: approve only the list seen now,
                // and answer once it is applied or dropped.
                let q = DeleteQueue::load(&self.paths.deletes).unwrap_or_default();
                match q.pairs.get(&pair).filter(|p| !p.paths.is_empty()) {
                    Some(p) if list != 0 && list != list_id(&p.paths) => Response::Err(LIST_CHANGED.into()),
                    Some(p) => return self.confirms.push((pair, p.paths.clone(), count, reply)),
                    None => Response::Err(format!("No deletes are held for {pair}.")),
                }
            }
            Request::ConfirmDeletes { pair, count, list } => self.confirm(&pair, None, count, list),
            Request::ReloadConfig => self.reload(),
            Request::ActivityTail { n } => Response::Activity(log::tail(&self.paths.activity, n.min(5000))),
            // Never logged: neither the request nor an error that might echo it.
            Request::SaveCredential { target, user, password } => match smb::save_credential(&target, &user, &password.0) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Err(e.to_string()),
            },
            Request::Remember { location } => return self.remember(location, reply),
            Request::TestConnection { side } => return self.test_connection(side, reply),
            Request::PathStatus { side } => return self.path_status(side, reply),
            Request::CheckUpdate => return self.check_update(reply),
            Request::InstallUpdate => self.install_update(),
            // A running trip owns the state files.
            Request::Restore { .. } if self.trip.is_some() => Response::Err("Ferry is copying right now. Pause Ferry, then restore.".into()),
            Request::Restore { path } => self.restore(&path),
            Request::Quit => {
                self.log.line("quit requested");
                // Give the IPC thread a moment to deliver this reply before the process ends.
                tray::set_timer(self.hwnd, T_QUIT, 250);
                Response::Ok
            }
        };
        let _ = reply.send(r);
    }

    /// Mark a pair's held deletes as confirmed; they apply at its next caught-up point.
    fn confirm(&mut self, pair: &str, seen: Option<&[String]>, count: u32, list: u64) -> Response {
        let mut q = match DeleteQueue::load(&self.paths.deletes) {
            Ok(q) => q,
            Err(e) => return Response::Err(format!("delete queue: {e}")),
        };
        if let Err(e) = approve(&mut q, pair, seen, count, list) {
            return Response::Err(e);
        }
        if let Err(e) = q.save(&self.paths.deletes) {
            return Response::Err(format!("delete queue: {e}"));
        }
        self.log.line(&format!("deletes confirmed for {pair}"));
        self.refresh_files();
        // Apply them now where they can run (at the destination the unload does it).
        self.trigger();
        Response::Ok
    }

    fn reload(&mut self) -> Response {
        match config::load(&self.paths.config) {
            Ok(c) => {
                self.cfg = c;
                self.log.line("settings reloaded");
                self.apply_autostart();
                // A running trip owns the state files; the next trip clears up after it.
                if self.trip.is_none() {
                    self.purge();
                }
                if self.sched.loc == Location::Source {
                    self.arm_recheck(); // the interval may have changed
                }
                self.refresh_files();
                self.resolve(); // fingerprints or servers may have changed
                Response::Ok
            }
            Err(e) => Response::Err(format!("settings: {e}")),
        }
    }

    fn restore(&mut self, path: &str) -> Response {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return Response::Err(format!("Can't read {path}: {e}")),
        };
        let p = &self.paths;
        let files = backup::Files { config: &p.config, manifest: &p.manifest, deletes: &p.deletes, snapshots: &p.snapshots };
        let dest = display_name(&self.cfg, Side::Destination);
        match backup::restore(&text, &files, &p.cargo, &dest) {
            Ok(r) => {
                self.log.line(&format!("settings restored from a backup; {} carried files kept, {} no longer on the laptop", r.kept, r.dropped));
                self.reload()
            }
            Err(e) => Response::Err(e),
        }
    }

    /// Ruling R31: forget the cargo and queued deletes of folders no longer in the list.
    fn purge(&mut self) {
        let known: Vec<String> = self.cfg.pairs.iter().map(|p| p.id.clone()).collect();
        let cargo = job::cargo_dir(&self.cfg, &self.paths);
        let mut events = vec![];
        if let Err(e) = purge_unknown(&known, &cargo, &self.paths.manifest, &self.paths.deletes, &self.paths.snapshots, &mut |e| events.push(e)) {
            self.log.line(&format!("clearing cargo of removed folders: {e}"));
        }
        for e in events {
            self.trip_event(e);
        }
    }

    fn remember(&mut self, side: Side, reply: Sender<Response>) {
        let host = self.server(side).lan_host.clone();
        let w = self.waker.clone();
        thread::spawn(move || match capture_for(&host) {
            Ok(fp) => w.send(Input::Remembered(side, fp, reply)),
            Err(e) => {
                let _ = reply.send(Response::Err(e));
            }
        });
    }

    fn remembered(&mut self, side: Side, fp: Fingerprint, reply: Sender<Response>) {
        // Re-read first so a save from the UI since our last load is kept.
        let saved = config::load(&self.paths.config).and_then(|mut cfg| {
            let srv = match side {
                Side::Source => &mut cfg.source,
                Side::Destination => &mut cfg.destination,
            };
            merge_fingerprint(&mut srv.fingerprints, fp);
            config::save(&self.paths.config, &cfg)?;
            Ok(cfg)
        });
        let resp = match saved {
            Ok(cfg) => {
                self.cfg = cfg;
                self.log.line(&format!("remembered this network as {}", display_name(&self.cfg, side)));
                self.resolve();
                Response::Ok
            }
            Err(e) => Response::Err(format!("settings: {e}")),
        };
        let _ = reply.send(resp);
    }

    fn test_connection(&self, side: Side, reply: Sender<Response>) {
        let srv = self.server(side).clone();
        let path = self.cfg.pairs.first().map(|p| match side {
            Side::Source => p.source_path.clone(),
            Side::Destination => p.destination_path.clone(),
        });
        thread::spawn(move || {
            // WNetAddConnection2 can't be cancelled and may block for 20 s or more on a host that
            // drops packets, so wait for it only so long and answer well inside the IPC's 10 s.
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(test_server(&srv, path.as_deref()));
            });
            let r = rx.recv_timeout(TEST_LIMIT).unwrap_or_else(|_| Err(test_timeout_message()));
            let _ = reply.send(r.map_or_else(Response::Err, |()| Response::Ok));
        });
    }

    /// Locate (with the VPN checker, up to ~5 s) off the tray thread, for this side's networks only.
    fn path_status(&self, side: Side, reply: Sender<Response>) {
        let fps = self.server(side).fingerprints.clone();
        thread::spawn(move || {
            let r = match net::adapters() {
                Ok(a) => path_reply(&fps, &a, &net::best_interface, &net::probe_445, &crate::vpn::check),
                Err(e) => Response::Err(format!("Ferry couldn't read this laptop's network adapters: {e}")),
            };
            let _ = reply.send(r);
        });
    }

    fn spawn_ui(&mut self, arg: &str) {
        if let Err(e) = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg(arg).spawn()) {
            self.log.line(&format!("could not open the Ferry window: {e}"));
        }
    }

    /// At start: say how the update that ended the last run went.
    fn after_update(&mut self) {
        match update::after_restart(&self.paths.update, update::VERSION) {
            Some((v, true)) => {
                self.log.line(&format!("updated to {v}"));
                self.notify(None, &format!("Ferry updated to {v}"), "The new version is running.");
            }
            Some((v, false)) => self.log.line(&format!("update to {v} did not install")),
            None => {}
        }
    }

    /// A Settings window is open, however it was opened (it may hold unsaved edits).
    fn windows_open(&self) -> bool {
        ferry_core::setup_window::is_open(ferry_core::setup_window::NAME)
    }

    /// The 10-minute tick: install a waiting update, or check once a day when nothing is going on.
    fn auto_update(&mut self) {
        if self.install_ready() {
            return;
        }
        let now = unix_now();
        let (paused, trip, open) = (self.paused(), self.trip.is_some(), self.windows_open());
        let idle = !self.updating && self.ready.is_none() && now >= self.update_retry_at;
        let since = now - self.update_ok_at;
        // net::metered() is a COM call, so it is asked last.
        if !idle || !update::may_auto_check(self.cfg.options.auto_update, self.installed, paused, trip, open, since) || net::metered() {
            return;
        }
        self.spawn_fetch(false);
    }

    /// Look for a newer release on a worker and, when there is one, download and verify it. An
    /// automatic check passes over a version that failed before; one the user asked for doesn't.
    fn spawn_fetch(&mut self, manual: bool) {
        self.updating = true;
        let (w, dir) = (self.waker.clone(), self.paths.update.clone());
        thread::spawn(move || {
            let o = match update::check(15_000) {
                Outcome::Available(r) if !manual && update::skipped(&dir).as_deref() == Some(r.version.as_str()) => Outcome::UpToDate,
                Outcome::Available(r) => update::fetch(r, &dir),
                o => o,
            };
            w.send(Input::Update(o, manual));
        });
    }

    fn update_done(&mut self, o: Outcome, manual: bool) {
        self.updating = false;
        let manual = manual || std::mem::take(&mut self.manual_asked);
        if let Some(line) = update::outcome_line(&o, self.update_failing) {
            self.log.line(&line);
        }
        self.update_failing = matches!(o, Outcome::Failed(_));
        if let Some(why) = update::manual_failure(&o).filter(|_| manual) {
            self.notify(None, "Ferry wasn't updated", &why);
        }
        let now = unix_now();
        match o {
            Outcome::Failed(_) => self.update_retry_at = now + 3600,
            // A mismatch is usually a download cut short: try again in an hour, and give the
            // release up only after it fails three times in a row.
            Outcome::BadChecksum(v) if !update::give_up_after_mismatch(&mut self.bad_sums, &v) => self.update_retry_at = now + 3600,
            Outcome::BadChecksum(v) | Outcome::BadSignature(v) => {
                self.update_ok_at = now;
                update::skip(&self.paths.update, &v);
            }
            Outcome::Ready(r, path, sha256) => {
                self.update_ok_at = now;
                self.ready = Some((r.version, path, sha256, manual));
                self.install_ready();
            }
            Outcome::UpToDate | Outcome::Available(_) => self.update_ok_at = now,
        }
    }

    /// Runs a verified installer when no trip is running (and, unless the user asked, no Ferry
    /// window is open and Ferry isn't paused), then quits so it can replace this exe. True if it did.
    fn install_ready(&mut self) -> bool {
        let Some((v, path, sha256, manual)) = self.ready.take() else { return false };
        if self.trip.is_some() || (!manual && (self.paused() || self.windows_open())) {
            self.ready = Some((v, path, sha256, manual));
            return false;
        }
        // It may have waited hours in a folder the user can write to: what runs must still be
        // what the signed checksums named.
        if !http::sha256_file(&path).is_ok_and(|h| h == sha256) {
            let _ = fs::remove_file(&path);
            self.log.line(&format!("update {v} changed after it was checked; not installed"));
            if manual {
                self.notify(None, "Ferry wasn't updated", &format!("The downloaded Ferry {v} changed after it was checked, so it wasn't installed."));
            }
            return false;
        }
        let marker = self.paths.update.join("installing");
        let _ = fs::write(&marker, &v);
        self.log.line(&format!("installing update {v}"));
        match std::process::Command::new(&path).arg("/S").spawn() {
            Ok(_) => {
                tray::quit(self.hwnd);
                true
            }
            Err(e) => {
                self.log.line(&format!("could not start the update installer: {e}"));
                if manual {
                    self.notify(None, "Ferry wasn't updated", &format!("The installer didn't start: {e}"));
                }
                let _ = fs::remove_file(&marker);
                false
            }
        }
    }

    /// Settings' "Check for updates": answered from a worker, metered or not, switch on or off.
    fn check_update(&self, reply: Sender<Response>) {
        let installable = self.installed;
        thread::spawn(move || {
            // Four WinHTTP phases of 2.4 s each stay inside the IPC's 10 s.
            let r = match update::check(2_400) {
                Outcome::Available(r) => Response::Update { latest: Some(r.version), installable },
                Outcome::Failed(e) => Response::Err(format!("Ferry couldn't check for updates: {e}")),
                _ => Response::Update { latest: None, installable },
            };
            let _ = reply.send(r);
        });
    }

    /// Settings' "Update to X.Y.Z": download, verify and install now (once a running trip ends).
    fn install_update(&mut self) -> Response {
        if !self.installed {
            return Response::Err(
                "This copy of Ferry wasn't installed with the installer, so it can't update itself. Download the new version from the releases page.".into(),
            );
        }
        if let Some((v, path, sha256, _)) = self.ready.take() {
            // Already downloaded and verified, waiting for a window to close or the pause to end.
            self.ready = Some((v, path, sha256, true));
            self.install_ready();
        } else if self.updating {
            self.manual_asked = true;
        } else {
            self.log.line("update requested");
            self.spawn_fetch(true);
        }
        Response::Ok
    }

    fn apply_autostart(&mut self) {
        if let Err(e) = tray::set_autostart(self.cfg.options.start_with_windows) {
            self.log.line(&format!("start with Windows: {e}"));
        }
    }

    /// Per-pair status and carried bytes, from the manifest and delete queue on disk.
    fn refresh_files(&mut self) {
        let m = CargoManifest::load(&self.paths.manifest).unwrap_or_default();
        let q = DeleteQueue::load(&self.paths.deletes).unwrap_or_default();
        self.status.cap_bytes = self.cfg.options.cap_bytes;
        self.status.carried_bytes = m.carried_bytes();
        self.status.pairs = self
            .cfg
            .pairs
            .iter()
            .map(|p| (p, m.items.iter().filter(|i| i.pair == p.id).map(|i| i.size).sum::<u64>()))
            // A disabled pair still shows while it has cargo to deliver.
            .filter(|(p, carried)| p.enabled || *carried > 0)
            .map(|(p, carried)| {
                let d = q.pairs.get(&p.id);
                // Unknown (false) until the pair's first source listing.
                let up_to_date = carried == 0 && d.is_some_and(|d| d.outstanding == 0 && d.paths.is_empty());
                let (new_carried, removed_at_destination) = self.counts.get(&p.id).copied().unwrap_or_default();
                PairStatus {
                    id: p.id.clone(),
                    carried_bytes: carried,
                    // ponytail: no destination/source byte totals yet, so 100 when up to date, else 0.
                    destination_percent: if up_to_date { 100 } else { 0 },
                    new_carried,
                    removed_at_destination,
                    up_to_date,
                    needs_confirm: d.is_some_and(|d| d.needs_confirm && !d.confirmed),
                    pending_deletes: d.map_or(0, |d| u32::try_from(d.paths.len()).unwrap_or(u32::MAX)),
                    deletes_list: d.map_or(0, |d| list_id(&d.paths)),
                }
            })
            .collect();
    }

    fn update_tray(&mut self) {
        if !self.paused() {
            self.paused_until = None;
        }
        let paused = self.paused_until.is_some();
        let s = &mut self.status;
        s.location = self.sched.loc;
        s.paused_until = self.paused_until;
        s.phase = phase(self.trip.as_ref().map(|t| t.kind), self.sched.loc, paused, s);
        let icon = match s.phase {
            Phase::Filling => Icon::Filling,
            Phase::Unloading => Icon::Unloading,
            _ if !s.attention.is_empty() => Icon::Attention,
            _ if paused => Icon::Paused,
            _ => Icon::Idle,
        };
        let dest = display_name(&self.cfg, Side::Destination);
        let what = match s.phase {
            Phase::Filling => "filling the laptop".into(),
            Phase::Unloading => format!("unloading at {dest}"),
            Phase::UpToDate => format!("{dest} is up to date"),
            Phase::Waiting => "waiting for a known network".into(),
            Phase::Idle if paused => "paused".into(),
            Phase::Idle => "idle".to_string(),
        };
        let mut tip = format!("Ferry: {}, {what}", place(&self.cfg, self.sched.loc));
        if !s.attention.is_empty() {
            tip.push_str(" (needs attention)");
        }
        tray::set_icon(self.hwnd, icon, &tip);
    }
}

/// The phase shown to the user. "Destination is up to date" only when it is: at the destination, nothing
/// carried, every pair up to date with no deletes waiting, and no trips behind.
pub fn phase(trip: Option<Kind>, loc: Location, paused: bool, s: &Status) -> Phase {
    let caught_up = !s.pairs.is_empty()
        && s.carried_bytes == 0
        && s.pairs.iter().all(|p| p.up_to_date && !p.needs_confirm && p.pending_deletes == 0)
        && s.trips_behind.unwrap_or(0) == 0;
    match (trip, loc) {
        (Some(Kind::Fill), _) => Phase::Filling,
        (Some(Kind::Unload), _) => Phase::Unloading,
        _ if paused => Phase::Idle,
        (None, Location::Elsewhere) => Phase::Waiting,
        (None, Location::Destination) if caught_up => Phase::UpToDate,
        _ => Phase::Idle,
    }
}

/// At the destination, whether to arm the one-off later retry after a trip: an unload left something
/// undone, or the path was lost mid-trip.
fn retry_at_destination(unload: bool, r: &TripResult) -> bool {
    r.path_lost
        || (unload
            && match &r.report {
                Report::Unload(Ok(rep)) => rep.failed > 0,
                Report::Unload(Err(_)) | Report::Skipped { .. } => true,
                _ => false,
            })
}

/// Activity's line for an unload; none for a destination visit that only refreshed the snapshots.
fn unload_line(rep: &UnloadReport, lost: bool, src: &str, dest: &str) -> Option<String> {
    if !lost && *rep == UnloadReport::default() {
        return None;
    }
    let done = if lost { format!("unload stopped, the path to {dest} is no longer allowed") } else { "unload done".into() };
    let mut line = format!(
        "{done}: {} delivered ({}), {} dropped as gone at {src}, {} failed, {} deleted at {dest}",
        rep.pushed,
        mb(rep.pushed_bytes),
        rep.discarded,
        rep.failed,
        rep.deleted
    );
    if rep.first_records > 0 {
        line += &format!(", recorded what's at {dest} for {} folders", rep.first_records);
    }
    Some(line)
}

/// Spec 4.4: "destination starts empty" covers only the pairs that existed when the user answered, so
/// it ends once a destination snapshot is recorded. Re-reads the settings first (keeping a save from the
/// UI); `None` when it was already off.
pub fn forget_destination_empty(path: &std::path::Path) -> io::Result<Option<Config>> {
    let mut cfg = config::load(path)?;
    if !cfg.options.destination_starts_empty {
        return Ok(None);
    }
    cfg.options.destination_starts_empty = false;
    config::save(path, &cfg)?;
    Ok(Some(cfg))
}

/// At the destination: whether an unload has anything to do, so a later retry is worth it. Cargo of a pair
/// still in the list (a disabled one still owes it), queued deletes, or an enabled pair whose
/// destination snapshot isn't recorded yet.
pub fn destination_work_waiting(pairs: &[ferry_core::config::Pair], m: &CargoManifest, q: &DeleteQueue, snapped: &dyn Fn(&str) -> bool) -> bool {
    let known = |id: &str| pairs.iter().any(|p| p.id == id);
    m.items.iter().any(|i| known(&i.pair))
        || q.pairs.iter().any(|(id, d)| known(id) && !d.paths.is_empty())
        || pairs.iter().any(|p| p.enabled && !snapped(&p.id))
}

/// Remember: a network already known (same router MAC and server IP) keeps its trust switch
/// and takes the current name; a new one is added.
pub fn merge_fingerprint(fps: &mut Vec<Fingerprint>, fp: Fingerprint) {
    match fps.iter_mut().find(|f| (f.gateway_mac, f.server_lan_ip) == (fp.gateway_mac, fp.server_lan_ip)) {
        Some(f) if !fp.name.is_empty() => f.name = fp.name,
        Some(_) => {}
        None => fps.push(fp),
    }
}

/// `Status.bulk_blocked` and `Status.trusting_vpn` for a resolved location.
pub fn bulk_status(l: &Located) -> (Option<String>, bool) {
    match (&l.bulk, l.server_ip) {
        (Some(b), Some(ip)) if l.location != Location::Elsewhere => (reason_text(b, ip), job::trusted_vpn(b).is_some()),
        _ => (None, false),
    }
}

/// `Response::Path` for one server's learned networks: the one the laptop is on now (one that
/// allows copies first, as `locate` picks), else "not-here".
pub fn path_reply(
    fps: &[Fingerprint],
    adapters: &[Adapter],
    best_if: &dyn Fn(std::net::IpAddr) -> Option<u32>,
    probe_445: &dyn Fn(std::net::IpAddr) -> bool,
    check_vpn: &dyn Fn(std::net::IpAddr, &Adapter, &Adapter) -> PathVerdict,
) -> Response {
    let mut found: Option<(&Fingerprint, BulkPath)> = None;
    for fp in fps {
        // ponytail: one locate per network, so a VPN check may repeat when two networks route the same way.
        let mut cfg = Config::default();
        cfg.source.fingerprints = vec![fp.clone()];
        let Some(bulk) = locate(&cfg, adapters, best_if, probe_445, check_vpn).bulk else { continue };
        let allows = bulk.allows_bulk();
        if found.is_none() || allows {
            found = Some((fp, bulk));
        }
        if allows {
            break;
        }
    }
    let Some((fp, bulk)) = found else {
        return Response::Path { network: String::new(), adapter: None, verdict: "not-here".into(), router: String::new(), server: String::new() };
    };
    let (adapter, verdict) = match bulk {
        BulkPath::Direct => (None, "direct".to_string()),
        BulkPath::Blocked(why) => (None, format!("blocked:{why}")),
        BulkPath::ViaVpn { adapter, verdict, trusted } => {
            let v = match verdict {
                PathVerdict::Direct => "vpn-direct".to_string(),
                PathVerdict::Unknown if trusted => "vpn-trusted".into(),
                PathVerdict::Unknown => "vpn-unknown".into(),
                PathVerdict::NotDirect(why) => format!("vpn-not-direct:{why}"),
            };
            (Some(adapter), v)
        }
    };
    let router = fp.gateway_mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
    Response::Path { network: fp.name.clone(), adapter, verdict, router, server: fp.server_lan_ip.to_string() }
}

/// An attention row for a trip error the user should act on or know about, if any.
fn error_attention(pair: &str, msg: &str, src: &str, dest: &str) -> Option<String> {
    use crate::trip::{fill::visit_destination, record_failed, unload::folder_clash, unreachable};
    if msg.starts_with(&unreachable(dest)) {
        Some(format!("Can't reach {dest} from here for {pair}. Nothing was carried for it."))
    } else if msg == visit_destination(dest) {
        Some(msg.to_string())
    } else if let Some(why) = msg.strip_prefix(&record_failed(dest)) {
        Some(format!("Ferry couldn't record what's on {dest}: {why}"))
    } else if msg.starts_with("listing failed") {
        Some(format!("Couldn't list {pair}. Nothing was carried for it."))
    } else if msg.ends_with(&folder_clash(src, dest)) {
        Some(format!("A folder at {dest} has the same name as a file in {pair}. Rename or move it at {dest}."))
    } else {
        None
    }
}

/// What the user calls a place: a side's name, or "Elsewhere".
fn place(cfg: &Config, loc: Location) -> String {
    match loc {
        Location::Source => display_name(cfg, Side::Source),
        Location::Destination => display_name(cfg, Side::Destination),
        Location::Elsewhere => "Elsewhere".into(),
    }
}

/// "Remember this network": resolve the server's LAN host and capture the gateway fingerprint.
fn capture_for(host: &str) -> Result<Fingerprint, String> {
    if host.trim().is_empty() {
        return Err("Enter this server's LAN address first.".into());
    }
    let ips: Vec<_> = (host, 445).to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.map(|a| a.ip()).collect();
    let ip = ips.iter().find(|ip| ip.is_ipv4()).or(ips.first()).copied().ok_or_else(|| format!("{host}: no address"))?;
    // Spec 4.5: no route check here; the path status is shown separately.
    net::capture(ip).map_err(|e| e.to_string())
}

/// Connect to the server and list the first pair's root: on the LAN host if it answers on
/// 445, else at its remote address, if it has one. With no folder pair yet, the host's `IPC$` share checks the
/// sign-in alone (Ruling R22).
fn test_server(srv: &Server, path: Option<&str>) -> Result<(), String> {
    let attempt = |host: &str, root: Option<PathBuf>| -> io::Result<()> {
        match (path, root) {
            (Some(p), Some(root)) => {
                smb::connect(host, job::share_of(p), &srv.credential_target)?;
                list_dir(&root).map(drop)
            }
            _ => smb::connect(host, "IPC$", &srv.credential_target),
        }
    };
    let lan = if answers_445(&srv.lan_host) {
        attempt(&srv.lan_host, path.map(|p| srv.lan_root(p))).map_err(|e| denied_hint(e, &srv.user))
    } else {
        Err(io::Error::other("no answer on port 445"))
    };
    lan.or_else(|lan| {
        if srv.remote_host.is_empty() {
            return Err(format!("LAN: {lan}."));
        }
        attempt(&srv.remote_host, path.and_then(|p| srv.remote_root(p))).map_err(|remote| format!("LAN: {lan}. Remote address: {remote}."))
    })
}

/// Access denied after a sign-in reads like a server fault; say what usually causes it.
fn denied_hint(e: io::Error, user: &str) -> io::Error {
    if e.raw_os_error() != Some(5) {
        return e;
    }
    io::Error::other(format!(
        "the server refused this folder for {user}. Check the password (a server like Unraid signs a wrong password in as a guest instead of refusing it) and that {user} may use this share"
    ))
}

/// How long TestConnection waits for its sign-in and listing before it answers anyway.
const TEST_LIMIT: Duration = Duration::from_secs(8);

fn test_timeout_message() -> String {
    format!("The server didn't answer within {} seconds. Check that it's on and reachable, then test again.", TEST_LIMIT.as_secs())
}

/// The LAN host resolves and accepts a TCP connection on 445 within 1.5 s.
fn answers_445(host: &str) -> bool {
    !host.trim().is_empty()
        && (host, 445).to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|a| net::probe_445(a.ip()))
}

/// Approve a pair's held deletes, only as the user saw them: `count` is how many the UI showed
/// (0, from an older UI, is not checked), and `seen` (a confirmation that waited for a trip)
/// the paths queued when it arrived.
pub fn approve(q: &mut DeleteQueue, pair: &str, seen: Option<&[String]>, count: u32, list: u64) -> Result<(), String> {
    match q.pairs.get_mut(pair) {
        Some(p) if !p.paths.is_empty() => {
            let shown = (count == 0 || p.paths.len() == count as usize) && (list == 0 || list == list_id(&p.paths));
            if !shown || seen.is_some_and(|s| s != p.paths.as_slice()) {
                return Err(LIST_CHANGED.into());
            }
            p.confirmed = true;
            p.needs_confirm = false;
            Ok(())
        }
        _ => Err(format!("No deletes are held for {pair}.")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// The network changed again while resolving: ignore this and resolve again.
    Stale,
    Same,
    Changed,
}

/// When to resolve and when a trip may start. Pure, so the timing rules are tested.
#[derive(Debug)]
pub struct Sched {
    /// Last resolved location.
    pub loc: Location,
    /// Its bulk path allows copies (spec 4.2).
    pub bulk: bool,
    /// A trip should start once the location is known to be current.
    due: bool,
    resolving: bool,
    resolve_again: bool,
    /// The network-change debounce timer is armed.
    debounce: bool,
}

impl Sched {
    pub fn new() -> Self {
        Self { loc: Location::Elsewhere, bulk: false, due: false, resolving: false, resolve_again: false, debounce: false }
    }

    pub fn net_changed(&mut self) {
        self.debounce = true;
    }

    /// True: spawn a resolver.
    pub fn debounce_fired(&mut self) -> bool {
        self.debounce = false;
        self.resolve()
    }

    /// True: spawn a resolver. False: one is running and its result will be discarded.
    pub fn resolve(&mut self) -> bool {
        if self.resolving {
            self.resolve_again = true;
            return false;
        }
        self.resolving = true;
        true
    }

    /// Source re-check, resume, saver off, aborted trip: a trip is wanted, from a fresh location.
    pub fn trigger(&mut self) -> bool {
        self.due = true;
        self.resolve()
    }

    /// A resolver finished. `Stale` keeps `resolving` set: the caller spawns another.
    pub fn located(&mut self, loc: Location, bulk: bool) -> Resolved {
        if std::mem::take(&mut self.resolve_again) {
            return Resolved::Stale;
        }
        self.resolving = false;
        // A path that turns allowed (or blocked) at the same place decides afresh.
        if bulk != self.bulk {
            self.bulk = bulk;
            self.due = true;
        }
        if loc == self.loc {
            return Resolved::Same;
        }
        self.loc = loc;
        self.due = true;
        Resolved::Changed
    }

    /// Never start from a location that may be stale.
    pub fn may_start(&self) -> bool {
        self.due && !self.resolving && !self.debounce
    }

    pub fn trip_started(&mut self) {
        self.due = false;
    }

    /// A trip didn't start. `blocked_at`: the place its own check found the laptop at, when
    /// only the path was blocked there. False: that's where we are; remember the block, so only
    /// a timer or a network change that finds the path allowed starts the next trip. True:
    /// anything else (somewhere else, or we've moved since): re-check now.
    pub fn blocked_at_start(&mut self, blocked_at: Option<Location>) -> bool {
        if blocked_at != Some(self.loc) {
            return true;
        }
        self.bulk = false;
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    StartFill,
    StartUnload,
    Cancel,
}

/// What the engine should do now. Pure; the caller only starts a trip when one is due.
#[allow(clippy::too_many_arguments)]
pub fn decide_action(
    loc: Location,
    bulk_allowed: bool,
    paused: bool,
    battery_saver: bool,
    pause_on_saver: bool,
    trip_running: bool,
    has_cargo: bool,
    pending_deletes: bool,
    record_destination: bool,
) -> Action {
    let blocked = paused || (battery_saver && pause_on_saver);
    if trip_running {
        return if blocked || loc == Location::Elsewhere || !bulk_allowed { Action::Cancel } else { Action::None };
    }
    match loc {
        // Spec 4.2: a known place with a blocked path only shows why (Status.bulk_blocked).
        _ if blocked || !bulk_allowed => Action::None,
        Location::Source => Action::StartFill,
        Location::Destination if has_cargo || pending_deletes || record_destination => Action::StartUnload,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Location::*;

    // decide_action(loc, bulk_allowed, paused, saver, pause_on_saver, trip_running, has_cargo, pending_deletes, record_home)
    #[test]
    fn source_starts_fill() {
        assert_eq!(decide_action(Source, true, false, false, true, false, false, false, false), Action::StartFill);
    }

    #[test]
    fn connection_test_answers_inside_the_ipc_bound() {
        // engine::ask gives up after 10 s; the test must answer before that.
        assert!(TEST_LIMIT < Duration::from_secs(10));
        assert!(test_timeout_message().starts_with("The server didn't answer within 8 seconds."));
    }

    #[test]
    fn destination_with_cargo_starts_unload() {
        assert_eq!(decide_action(Destination, true, false, false, true, false, true, false, false), Action::StartUnload);
    }

    #[test]
    fn destination_unloads_to_record_the_snapshot() {
        assert_eq!(decide_action(Destination, true, false, false, true, false, false, false, true), Action::StartUnload);
        assert_eq!(decide_action(Destination, false, false, false, true, false, false, false, true), Action::None, "blocked path");
        assert_eq!(decide_action(Source, true, false, false, true, false, false, false, true), Action::StartFill);
    }

    #[test]
    fn destination_with_only_held_deletes_unloads() {
        assert_eq!(decide_action(Destination, true, false, false, true, false, false, true, false), Action::StartUnload);
    }

    #[test]
    fn destination_with_nothing_does_nothing() {
        assert_eq!(decide_action(Destination, true, false, false, true, false, false, false, false), Action::None);
    }

    #[test]
    fn leaving_cancels_running_trip() {
        assert_eq!(decide_action(Elsewhere, true, false, false, true, true, true, false, false), Action::Cancel);
    }

    #[test]
    fn pause_and_battery_saver_block() {
        assert_eq!(decide_action(Source, true, true, false, true, false, false, false, false), Action::None);
        assert_eq!(decide_action(Destination, true, false, true, true, false, true, true, false), Action::None);
        // saver on but the option is off: work goes ahead
        assert_eq!(decide_action(Source, true, false, true, false, false, false, false, false), Action::StartFill);
    }

    #[test]
    fn blocked_bulk_does_not_start_trips() {
        assert_eq!(decide_action(Source, false, false, false, true, false, false, false, false), Action::None);
        assert_eq!(decide_action(Destination, false, false, false, true, false, true, true, false), Action::None);
        // A path found blocked while a trip runs stops it.
        assert_eq!(decide_action(Source, false, false, false, true, true, false, false, false), Action::Cancel);
        assert_eq!(decide_action(Destination, false, false, false, true, true, true, false, false), Action::Cancel);
    }

    #[test]
    fn blocked_location_is_shown_and_starts_a_trip_once_unblocked() {
        let act = |s: &Sched| decide_action(s.loc, s.bulk, false, false, true, false, true, false, false);
        let mut s = Sched::new();
        assert!(s.resolve());
        assert_eq!(s.located(Source, false), Resolved::Changed);
        assert_eq!(s.loc, Source, "the place is recognised");
        assert_eq!(act(&s), Action::None);
        // The source re-check finds it still blocked: still nothing.
        assert!(s.trigger());
        assert_eq!(s.located(Source, false), Resolved::Same);
        assert_eq!(act(&s), Action::None);
        // A later resolve (a plain network change) finds the path allowed: the fill starts.
        s.net_changed();
        assert!(s.debounce_fired());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(s.may_start());
        assert_eq!(act(&s), Action::StartFill);
    }

    #[test]
    fn a_path_that_turns_allowed_after_a_trip_starts_another() {
        // A fill ran and ended; then the path was blocked and later came back, with no
        // re-check timer in between.
        let mut s = settled(Source);
        assert!(s.resolve());
        assert_eq!(s.located(Source, false), Resolved::Same);
        assert!(s.resolve());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(s.may_start());
        // While nothing changes, nothing more starts.
        s.trip_started();
        assert!(s.resolve());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(!s.may_start());
    }

    #[test]
    fn destination_retry_after_undone_unloads_and_lost_paths() {
        use crate::trip::{FillReport, UnloadReport};
        let res = |report, path_lost| TripResult { report, remote_ok: true, path_lost };
        let unload = |failed| Report::Unload(Ok(UnloadReport { failed, ..Default::default() }));
        let fill = || Report::Fill(Ok(FillReport::default()));
        assert!(!retry_at_destination(true, &res(unload(0), false)));
        assert!(retry_at_destination(true, &res(unload(1), false)));
        assert!(retry_at_destination(true, &res(Report::Skipped { title: "t".into(), body: "b".into() }, false)));
        assert!(retry_at_destination(true, &res(unload(0), true)), "the path was lost mid-unload");
        assert!(retry_at_destination(false, &res(fill(), true)));
        assert!(!retry_at_destination(false, &res(fill(), false)));
    }

    #[test]
    fn remembering_a_known_network_updates_it_in_place() {
        let ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
        let fp = |mac: u8, name: &str| Fingerprint { gateway_mac: [mac; 6], server_lan_ip: ip, name: name.into(), ..Default::default() };
        let trusted = |f: Fingerprint| Fingerprint { trust_vpn: true, trusted_adapter: "WireGuard".into(), ..f };
        let mut fps = vec![trusted(fp(1, ""))];
        merge_fingerprint(&mut fps, fp(1, "OfficeNet"));
        assert_eq!(fps, [trusted(fp(1, "OfficeNet"))]);
        merge_fingerprint(&mut fps, fp(1, ""));
        assert_eq!(fps, [trusted(fp(1, "OfficeNet"))], "an unnamed capture keeps the known name");
        merge_fingerprint(&mut fps, fp(2, "Guest"));
        assert_eq!(fps, [trusted(fp(1, "OfficeNet")), fp(2, "Guest")]);
    }

    #[test]
    fn status_reports_block_reason() {
        let ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
        let at = |location, bulk| Located { location, server_ip: Some(ip), bulk: Some(bulk), network_name: None, route_if: Some(2) };
        let via = |verdict, trusted| BulkPath::ViaVpn { adapter: "WireGuard".into(), verdict, trusted };
        let (blocked, trusting) = bulk_status(&at(Source, via(PathVerdict::Unknown, false)));
        assert_eq!(
            blocked.as_deref(),
            Some("Windows sends traffic for 192.0.2.10 through WireGuard, and Ferry can't confirm that link stays on this network.")
        );
        assert!(!trusting);
        assert_eq!(bulk_status(&at(Destination, via(PathVerdict::Unknown, true))), (None, true));
        assert_eq!(bulk_status(&at(Source, via(PathVerdict::Direct, false))), (None, false));
        assert_eq!(bulk_status(&at(Source, BulkPath::Direct)), (None, false));
        assert_eq!(bulk_status(&at(Source, BulkPath::Blocked("no route".into()))), (Some("no route".into()), false));
        let away = Located::ELSEWHERE;
        assert_eq!(bulk_status(&away), (None, false));
    }

    #[test]
    fn path_status_names_the_network_adapter_and_verdict() {
        use ferry_core::location::Adapter;
        let ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
        let gw = [0x00, 0x00, 0x5e, 0x00, 0x53, 0x01];
        let eth = Adapter {
            index: 2,
            alias: "Ethernet".into(),
            description: "Ethernet Controller".into(),
            if_type: 6,
            hardware: true,
            addrs: vec!["192.0.2.150".parse().unwrap()],
            prefixes: vec![("192.0.2.150".parse().unwrap(), 24)],
            gateway_macs: vec![gw],
            up: true,
        };
        let wg = Adapter { index: 30, alias: "WireGuard".into(), description: "WireGuard Tunnel".into(), hardware: false, gateway_macs: vec![], ..eth.clone() };
        let fp = |trust_vpn| Fingerprint { gateway_mac: gw, server_lan_ip: ip, name: "OfficeNet".into(), trust_vpn, trusted_adapter: "WireGuard".into() };
        let ads = [eth.clone(), wg];
        let at = |fps: &[Fingerprint], route: u32, v: PathVerdict| path_reply(fps, &ads, &|_| Some(route), &|_| true, &|_, _, _| v.clone());
        let path = |adapter: Option<&str>, verdict: &str| Response::Path {
            network: "OfficeNet".into(),
            adapter: adapter.map(Into::into),
            verdict: verdict.into(),
            router: "00:00:5e:00:53:01".into(),
            server: "192.0.2.10".into(),
        };
        assert_eq!(at(&[fp(false)], 2, PathVerdict::Unknown), path(None, "direct"));
        assert_eq!(at(&[fp(false)], 30, PathVerdict::Direct), path(Some("WireGuard"), "vpn-direct"));
        assert_eq!(at(&[fp(false)], 30, PathVerdict::Unknown), path(Some("WireGuard"), "vpn-unknown"));
        assert_eq!(at(&[fp(true)], 30, PathVerdict::Unknown), path(Some("WireGuard"), "vpn-trusted"));
        assert_eq!(at(&[fp(true)], 30, PathVerdict::NotDirect("relayed".into())), path(Some("WireGuard"), "vpn-not-direct:relayed"));
        assert_eq!(
            at(&[fp(false)], 7, PathVerdict::Unknown),
            path(None, "blocked:Windows sends traffic for 192.0.2.10 through an adapter Ferry can't see.")
        );
        let away = Response::Path { network: String::new(), adapter: None, verdict: "not-here".into(), router: String::new(), server: String::new() };
        assert_eq!(path_reply(&[fp(false)], &ads, &|_| Some(2), &|_| false, &|_, _, _| PathVerdict::Unknown), away);
        assert_eq!(at(&[], 2, PathVerdict::Unknown), away);
    }

    /// Settled at `loc` with its trip already started.
    fn settled(loc: Location) -> Sched {
        let mut s = Sched::new();
        assert!(s.resolve());
        assert_eq!(s.located(loc, true), Resolved::Changed);
        s.trip_started();
        s
    }

    #[test]
    fn a_trip_blocked_where_the_engine_is_waits_for_a_timer_or_network_change() {
        // The engine saw the source path allowed; the trip's own check found it blocked there.
        let mut s = settled(Source);
        assert!(!s.blocked_at_start(Some(Source)), "no immediate re-check");
        assert!(!s.bulk);
        assert!(!s.may_start());
        // A plain network change that finds the path allowed starts the fill.
        s.net_changed();
        assert!(s.debounce_fired());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(s.may_start());
        // The trip found the laptop somewhere else: re-check now, as before.
        let mut s = settled(Source);
        assert!(s.blocked_at_start(None));
        assert!(s.bulk, "where the trip belongs is unknown until the fresh resolve");
        // Blocked at the source, but the engine has since resolved the destination: re-check.
        let mut s = settled(Destination);
        assert!(s.blocked_at_start(Some(Source)));
        assert!(s.bulk);
    }

    #[test]
    fn destination_retry_only_while_work_waits() {
        use ferry_core::cargo::CargoItem;
        use ferry_core::config::Pair;
        use ferry_core::deletes::PairDeletes;
        let pairs = [Pair { id: "on".into(), enabled: true, ..Default::default() }, Pair { id: "off".into(), enabled: false, ..Default::default() }];
        let snap = |_: &str| true;
        let none = CargoManifest::default();
        assert!(!destination_work_waiting(&pairs, &none, &DeleteQueue::default(), &snap), "nothing waiting: no retry");
        assert!(destination_work_waiting(&pairs, &none, &DeleteQueue::default(), &|id| id != "on"), "a pair without a snapshot");
        assert!(!destination_work_waiting(&pairs, &none, &DeleteQueue::default(), &|id| id != "off"), "a disabled pair needs no snapshot");
        let cargo = |pair: &str| CargoManifest { items: vec![CargoItem { pair: pair.into(), rel: "a".into(), size: 1, mtime: 1, hash: 1 }], ..Default::default() };
        assert!(destination_work_waiting(&pairs, &cargo("off"), &DeleteQueue::default(), &snap), "cargo owed by a disabled pair");
        assert!(!destination_work_waiting(&pairs, &cargo("gone"), &DeleteQueue::default(), &snap), "cargo of a removed pair isn't delivered");
        let mut q = DeleteQueue::default();
        q.pairs.insert("on".into(), PairDeletes { paths: vec!["x".into()], ..Default::default() });
        assert!(destination_work_waiting(&pairs, &none, &q, &snap), "queued deletes");
    }

    #[test]
    fn a_first_destination_snapshot_retires_the_destination_is_empty_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.options.destination_starts_empty = true;
        config::save(&path, &c).unwrap();
        // The UI saved a change since the engine last loaded: it's kept.
        c.options.recheck_minutes = 42;
        config::save(&path, &c).unwrap();
        let after = forget_destination_empty(&path).unwrap().expect("the answer was set");
        assert!(!after.options.destination_starts_empty);
        assert_eq!(after.options.recheck_minutes, 42);
        let on_disk = config::load(&path).unwrap();
        assert!(!on_disk.options.destination_starts_empty);
        assert_eq!(on_disk.options.recheck_minutes, 42);
        assert!(forget_destination_empty(&path).unwrap().is_none(), "nothing to write the second time");
    }

    #[test]
    fn wake_with_stale_source_and_timer_never_fills_before_resolving() {
        let mut s = settled(Source);
        s.net_changed(); // woke from sleep, now at the destination
        assert!(s.trigger()); // the source re-check timer fires first
        assert!(!s.may_start(), "a stale Office must not start a fill");
        assert!(!s.debounce_fired()); // a resolver is already running: its result is redone
        assert_eq!(s.located(Source, true), Resolved::Stale);
        assert!(!s.may_start());
        assert_eq!(s.located(Destination, true), Resolved::Changed);
        assert_eq!(s.loc, Destination);
        assert!(s.may_start());
    }

    #[test]
    fn armed_debounce_blocks_start() {
        let mut s = settled(Source);
        assert!(s.trigger());
        assert_eq!(s.located(Source, true), Resolved::Same);
        s.net_changed();
        assert!(!s.may_start());
        assert!(s.debounce_fired());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(s.may_start());
    }

    #[test]
    fn unchanged_resolve_does_not_start_another_fill() {
        let mut s = settled(Source);
        s.net_changed();
        assert!(s.debounce_fired());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(!s.may_start());
    }

    #[test]
    fn source_recheck_starts_after_a_fresh_resolve() {
        let mut s = settled(Source);
        assert!(s.trigger());
        assert!(!s.may_start());
        assert_eq!(s.located(Source, true), Resolved::Same);
        assert!(s.may_start());
    }

    fn queue(paths: &[&str]) -> DeleteQueue {
        let mut q = DeleteQueue::default();
        q.pairs.insert(
            "p".into(),
            ferry_core::deletes::PairDeletes { paths: paths.iter().map(|s| s.to_string()).collect(), ..Default::default() },
        );
        q
    }

    #[test]
    fn deferred_confirm_with_changed_paths_is_dropped() {
        let mut q = queue(&["a.zip", "b.zip"]);
        let seen = vec!["a.zip".to_string()];
        assert!(approve(&mut q, "p", Some(&seen), 0, 0).unwrap_err().contains("changed"));
        assert!(!q.pairs["p"].confirmed);
    }

    #[test]
    fn deferred_confirm_with_same_paths_applies() {
        let mut q = queue(&["a.zip"]);
        approve(&mut q, "p", Some(&["a.zip".to_string()]), 0, 0).unwrap();
        assert!(q.pairs["p"].confirmed);
        assert!(approve(&mut queue(&[]), "p", None, 0, 0).is_err());
        assert!(approve(&mut q, "other", None, 0, 0).is_err());
    }

    #[test]
    fn confirm_clears_the_held_flag_and_checks_the_count_shown() {
        let mut q = queue(&["a.zip", "b.zip"]);
        q.pairs.get_mut("p").unwrap().needs_confirm = true;
        // The UI showed 3 files but 2 are queued now: the user didn't see this list.
        assert!(approve(&mut q, "p", None, 3, 0).unwrap_err().contains("changed"));
        assert!(q.pairs["p"].needs_confirm && !q.pairs["p"].confirmed);
        approve(&mut q, "p", None, 2, 0).unwrap();
        assert!(q.pairs["p"].confirmed);
        assert!(!q.pairs["p"].needs_confirm, "the Delete button goes away once confirmed");
    }

    #[test]
    fn confirm_checks_the_list_shown_not_just_its_size() {
        let shown = list_id(&["a.zip".to_string(), "b.zip".to_string()]);
        // A fill in between swapped one file for another: same count, different list.
        let mut q = queue(&["a.zip", "c.zip"]);
        assert!(approve(&mut q, "p", None, 2, shown).unwrap_err().contains("changed"));
        assert!(!q.pairs["p"].confirmed);
        let mut q = queue(&["a.zip", "b.zip"]);
        approve(&mut q, "p", None, 2, shown).unwrap();
        assert!(q.pairs["p"].confirmed);
        assert_ne!(list_id(&[]), 0, "0 means no fingerprint was sent");
    }

    fn status(pairs: Vec<PairStatus>, carried: u64, trips_behind: Option<u32>) -> Status {
        Status { location: Destination, phase: Phase::Idle, remote_ok: true, cap_bytes: 100, carried_bytes: carried, trips_behind, pairs, ..Default::default() }
    }

    fn pair_status(up_to_date: bool, needs_confirm: bool, pending_deletes: u32) -> PairStatus {
        PairStatus {
            id: "p".into(),
            carried_bytes: 0,
            destination_percent: 0,
            new_carried: 0,
            removed_at_destination: 0,
            up_to_date,
            needs_confirm,
            pending_deletes,
            deletes_list: 0,
        }
    }

    #[test]
    fn up_to_date_only_when_destination_really_is() {
        let ok = || vec![pair_status(true, false, 0)];
        assert_eq!(phase(None, Destination, false, &status(ok(), 0, Some(0))), Phase::UpToDate);
        assert_eq!(phase(None, Destination, false, &status(ok(), 0, None)), Phase::UpToDate);
        // Delivered, but the source still has more than one trip's worth.
        assert_eq!(phase(None, Destination, false, &status(ok(), 0, Some(2))), Phase::Idle);
        assert_eq!(phase(None, Destination, false, &status(vec![pair_status(false, false, 0)], 0, Some(0))), Phase::Idle);
        assert_eq!(phase(None, Destination, false, &status(vec![pair_status(true, true, 3)], 0, Some(0))), Phase::Idle);
        assert_eq!(phase(None, Destination, false, &status(ok(), 5, Some(0))), Phase::Idle);
        assert_eq!(phase(None, Destination, false, &status(vec![], 0, Some(0))), Phase::Idle);
        assert_eq!(phase(Some(Kind::Unload), Destination, false, &status(ok(), 0, None)), Phase::Unloading);
        assert_eq!(phase(None, Source, false, &status(ok(), 0, None)), Phase::Idle);
    }

    #[test]
    fn unreachable_destination_and_failed_listings_need_attention() {
        // Ruling V3: no VPN name in the row; the remote address may be any VPN, or none.
        assert_eq!(
            error_attention("docs", "can't reach Lake house: The network path was not found. (os error 53)", "HQ", "Lake house").as_deref(),
            Some("Can't reach Lake house from here for docs. Nothing was carried for it.")
        );
        assert!(error_attention("docs", "listing failed: access denied", "HQ", "Lake house").unwrap().starts_with("Couldn't list docs"));
        assert_eq!(
            error_attention("docs", "a.zip: a folder at Lake house has the same name as a file at HQ", "HQ", "Lake house").as_deref(),
            Some("A folder at Lake house has the same name as a file in docs. Rename or move it at Lake house.")
        );
        assert_eq!(error_attention("docs", "a.zip: access denied (os error 5)", "HQ", "Lake house"), None);
    }

    #[test]
    fn failed_destination_record_needs_attention() {
        let msg = format!("{}access denied (os error 5)", crate::trip::record_failed("Lake house"));
        assert_eq!(error_attention("docs", &msg, "HQ", "Lake house").unwrap(), "Ferry couldn't record what's on Lake house: access denied (os error 5)");
        let visit = crate::trip::fill::visit_destination("Lake house");
        assert_eq!(visit, "Visit Lake house once so Ferry can record what's there.");
        assert_eq!(error_attention("", &visit, "HQ", "Lake house").as_deref(), Some(visit.as_str()));
    }

    #[test]
    fn test_connection_explains_access_denied() {
        let e = denied_hint(io::Error::from_raw_os_error(5), "alex");
        assert!(e.to_string().contains("refused this folder for alex"), "{e}");
        assert!(e.to_string().contains("guest"), "{e}");
        assert_eq!(denied_hint(io::Error::from_raw_os_error(67), "alex").raw_os_error(), Some(67));
    }

    #[test]
    fn a_visit_that_only_refreshed_the_snapshot_logs_nothing() {
        use crate::trip::UnloadReport;
        let line = |rep: &UnloadReport, lost| unload_line(rep, lost, "HQ", "Lake house");
        assert_eq!(line(&UnloadReport::default(), false), None);
        assert_eq!(
            line(&UnloadReport::default(), true).unwrap(),
            "unload stopped, the path to Lake house is no longer allowed: 0 delivered (0.0 MB), 0 dropped as gone at HQ, 0 failed, 0 deleted at Lake house"
        );
        let first = UnloadReport { first_records: 2, ..Default::default() };
        assert!(line(&first, false).unwrap().ends_with("recorded what's at Lake house for 2 folders"));
        let deleted = UnloadReport { deleted: 1, ..Default::default() };
        assert!(line(&deleted, false).unwrap().contains("1 deleted at Lake house"));
    }
}
