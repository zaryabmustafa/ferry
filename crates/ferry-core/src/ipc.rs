//! UI <-> engine protocol: one JSON line per request, one JSON line per response.
use crate::location::Location;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};

pub const PIPE: &str = r"\\.\pipe\ferry";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    #[serde(alias = "Office")]
    Source,
    #[serde(alias = "Home")]
    Destination,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Status,
    Pause { minutes: Option<u32> },
    Resume,
    Remember { location: Side },
    /// `count`: how many deletes the user was shown; 0 (an older UI) is not checked.
    ConfirmDeletes {
        pair: String,
        #[serde(default)]
        count: u32,
    },
    ReloadConfig,
    ActivityTail { n: usize },
    SaveCredential { target: String, user: String, password: String },
    TestConnection { side: Side },
    /// How bulk copies would reach this side's server from the network the laptop is on now.
    PathStatus { side: Side },
    /// Look for a newer release now (answered with `Update`).
    CheckUpdate,
    /// Download, verify and install the newest release; Ferry restarts.
    InstallUpdate,
    /// Replace the settings and laptop state with a backup file's (see `backup::restore`).
    Restore { path: String },
    Quit,
}

// Manual Debug so a password can never reach a log line.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Status => f.write_str("Status"),
            Request::Pause { minutes } => f.debug_struct("Pause").field("minutes", minutes).finish(),
            Request::Resume => f.write_str("Resume"),
            Request::Remember { location } => f.debug_struct("Remember").field("location", location).finish(),
            Request::ConfirmDeletes { pair, count } => {
                f.debug_struct("ConfirmDeletes").field("pair", pair).field("count", count).finish()
            }
            Request::ReloadConfig => f.write_str("ReloadConfig"),
            Request::ActivityTail { n } => f.debug_struct("ActivityTail").field("n", n).finish(),
            Request::SaveCredential { target, user, .. } => f
                .debug_struct("SaveCredential")
                .field("target", target)
                .field("user", user)
                .field("password", &"***")
                .finish(),
            Request::TestConnection { side } => f.debug_struct("TestConnection").field("side", side).finish(),
            Request::PathStatus { side } => f.debug_struct("PathStatus").field("side", side).finish(),
            Request::CheckUpdate => f.write_str("CheckUpdate"),
            Request::InstallUpdate => f.write_str("InstallUpdate"),
            Request::Restore { path } => f.debug_struct("Restore").field("path", path).finish(),
            Request::Quit => f.write_str("Quit"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairStatus {
    pub id: String,
    pub carried_bytes: u64,
    #[serde(alias = "home_percent")]
    pub destination_percent: u8,
    pub new_carried: u32,
    #[serde(alias = "removed_at_home")]
    pub removed_at_destination: u32,
    pub up_to_date: bool,
    pub needs_confirm: bool,
    /// Destination deletes held in the queue for this pair (what "Delete N files at {Dest}" confirms).
    #[serde(default)]
    pub pending_deletes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Filling,
    Unloading,
    UpToDate,
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub location: Location,
    pub phase: Phase,
    pub paused_until: Option<i64>,
    #[serde(alias = "tailscale_ok")]
    pub remote_ok: bool,
    /// Why bulk transfer is held back right now, if it is.
    #[serde(default)]
    pub bulk_blocked: Option<String>,
    /// True while a trusted VPN adapter stands in for the LAN.
    #[serde(default)]
    pub trusting_vpn: bool,
    pub cap_bytes: u64,
    pub carried_bytes: u64,
    pub rate_bps: u64,
    pub eta_secs: Option<u64>,
    /// ceil(outstanding_bytes / cap_bytes), computed by the engine.
    pub trips_behind: Option<u32>,
    pub pairs: Vec<PairStatus>,
    pub attention: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Status(Status),
    Ok,
    Activity(Vec<String>),
    Err(String),
    /// Answer to `PathStatus`. `verdict`: "direct", "vpn-direct", "vpn-unknown", "vpn-trusted",
    /// "vpn-not-direct:<reason>", "blocked:<reason>" or "not-here". `network`: the matched learned
    /// network's name (may be empty); `router`: its gateway MAC, "aa:bb:cc:dd:ee:ff"; `server`:
    /// its server LAN IP. Together they name the learned network (empty when "not-here").
    Path {
        network: String,
        adapter: Option<String>,
        verdict: String,
        #[serde(default)]
        router: String,
        #[serde(default)]
        server: String,
    },
    /// Answer to `CheckUpdate`: the newer release's version, if any, and whether this copy can
    /// install it (only installed copies can; a portable one is updated by hand).
    Update { latest: Option<String>, installable: bool },
}

/// JSON plus a trailing newline.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    let mut b = serde_json::to_vec(v).expect("ipc types always serialize");
    b.push(b'\n');
    b
}

pub fn decode<T: DeserializeOwned>(line: &[u8]) -> io::Result<T> {
    serde_json::from_slice(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Client side: send `req` to the pipe, read the one-line reply.
pub fn call(pipe: &str, req: &Request) -> io::Result<Response> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    let mut f = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(pipe) {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 10 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    };
    // Never send a password to a squatter: the pipe's server must be us.
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let ok = crate::sid::pipe_server_is_current_user(windows::Win32::Foundation::HANDLE(f.as_raw_handle()));
        if !ok.unwrap_or(false) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe server is not running as the current user"));
        }
    }
    f.write_all(&encode(req))?;
    let mut line = Vec::new();
    // 16 MiB cap on a reply; an Activity tail is the largest legitimate one.
    BufReader::new(Read::by_ref(&mut f).take(16 << 20)).read_until(b'\n', &mut line)?;
    decode(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
        let bytes = encode(&v);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(decode::<T>(&bytes).unwrap(), v);
    }

    #[test]
    fn every_request_round_trips() {
        for r in [
            Request::Status,
            Request::Pause { minutes: Some(30) },
            Request::Pause { minutes: None },
            Request::Resume,
            Request::Remember { location: Side::Destination },
            Request::ConfirmDeletes { pair: "docs".into(), count: 4 },
            Request::ReloadConfig,
            Request::ActivityTail { n: 50 },
            Request::SaveCredential { target: "t".into(), user: "u".into(), password: "p\nw".into() },
            Request::TestConnection { side: Side::Source },
            Request::PathStatus { side: Side::Destination },
            Request::CheckUpdate,
            Request::InstallUpdate,
            Request::Quit,
        ] {
            rt(r);
        }
    }

    #[test]
    fn status_round_trips() {
        let s = Status {
            location: Location::Destination,
            phase: Phase::Filling,
            paused_until: Some(1_700_000_000),
            remote_ok: true,
            bulk_blocked: None,
            trusting_vpn: false,
            cap_bytes: 10 << 30,
            carried_bytes: 5 << 30,
            rate_bps: 1_000_000,
            eta_secs: Some(90),
            trips_behind: None,
            pairs: vec![PairStatus {
                id: "docs".into(),
                carried_bytes: 1,
                destination_percent: 50,
                new_carried: 2,
                removed_at_destination: 3,
                up_to_date: false,
                needs_confirm: true,
                pending_deletes: 4,
            }],
            attention: vec!["x".into()],
        };
        rt(Response::Status(s));
        rt(Response::Ok);
        rt(Response::Activity(vec!["a".into()]));
        rt(Response::Err("e".into()));
        rt(Response::Update { latest: Some("0.2.3".into()), installable: true });
        rt(Response::Update { latest: None, installable: false });
        rt(Response::Path {
            network: "OfficeNet".into(),
            adapter: Some("WireGuard".into()),
            verdict: "vpn-unknown".into(),
            router: "00:00:5e:00:53:01".into(),
            server: "192.0.2.10".into(),
        });
    }

    #[test]
    fn path_without_router_still_decodes() {
        let r: Response = decode(br#"{"Path":{"network":"","adapter":null,"verdict":"not-here"}}"#).unwrap();
        assert_eq!(
            r,
            Response::Path { network: "".into(), adapter: None, verdict: "not-here".into(), router: "".into(), server: "".into() }
        );
    }

    #[test]
    fn pair_status_without_pending_deletes_still_decodes() {
        let p: PairStatus = serde_json::from_str(
            r#"{"id":"a","carried_bytes":0,"home_percent":0,"new_carried":0,"removed_at_home":0,"up_to_date":false,"needs_confirm":true}"#,
        )
        .unwrap();
        assert_eq!(p.pending_deletes, 0);
    }

    #[test]
    fn old_side_and_location_names_still_decode() {
        let r: Request = decode(br#"{"Remember":{"location":"Home"}}"#).unwrap();
        assert_eq!(r, Request::Remember { location: Side::Destination });
        let r: Request = decode(br#"{"TestConnection":{"side":"Office"}}"#).unwrap();
        assert_eq!(r, Request::TestConnection { side: Side::Source });
        let l: Location = serde_json::from_str(r#""Office""#).unwrap();
        assert_eq!(l, Location::Source);
        let l: Location = serde_json::from_str(r#""Home""#).unwrap();
        assert_eq!(l, Location::Destination);
        // Written with the new names.
        assert_eq!(serde_json::to_string(&Side::Destination).unwrap(), r#""Destination""#);
        assert_eq!(serde_json::to_string(&Location::Source).unwrap(), r#""Source""#);
    }

    #[test]
    fn confirm_without_count_still_decodes() {
        let r: Request = decode(br#"{"ConfirmDeletes":{"pair":"docs"}}"#).unwrap();
        assert_eq!(r, Request::ConfirmDeletes { pair: "docs".into(), count: 0 });
    }

    #[test]
    fn password_not_in_debug() {
        let r = Request::SaveCredential { target: "t".into(), user: "u".into(), password: "hunter2".into() };
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn status_reads_old_tailscale_ok() {
        let mut v = serde_json::to_value(Status {
            location: Location::Destination,
            phase: Phase::Idle,
            paused_until: None,
            remote_ok: true,
            bulk_blocked: None,
            trusting_vpn: false,
            cap_bytes: 0,
            carried_bytes: 0,
            rate_bps: 0,
            eta_secs: None,
            trips_behind: None,
            pairs: vec![],
            attention: vec![],
        })
        .unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("remote_ok");
        o.remove("bulk_blocked");
        o.remove("trusting_vpn");
        o.insert("tailscale_ok".into(), false.into());
        let s: Status = serde_json::from_value(v).unwrap();
        assert!(!s.remote_ok);
        assert_eq!((s.bulk_blocked, s.trusting_vpn), (None, false));
    }
}
