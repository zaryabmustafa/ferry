//! The Tailscale checker (spec 4.3): find the peer that serves the server's IP in
//! `tailscale status --json`, then `tailscale ping` it to force path discovery and see whether
//! the link is a direct endpoint on this network.

use super::{CliOut, Run};
use ferry_core::location::{in_prefix, Adapter, PathVerdict};
use serde_json::Value;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// Status and ping together.
const BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub tailscale_ips: Vec<IpAddr>,
    /// Subnet routes this peer is the primary router for.
    pub routes: Vec<(IpAddr, u8)>,
    pub online: bool,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingPath {
    Direct(SocketAddr),
    /// Through a relay, e.g. `DERP(fra)`.
    Relayed(String),
    NoReply,
}

/// `vpn` is the adapter Windows routes `server_ip` through: Tailscale answers only when it holds
/// one of this device's Tailscale IPs (another VPN in 100.64.0.0/10 isn't Tailscale).
pub fn check(server_ip: IpAddr, vpn: &Adapter, local: &Adapter, run: Run) -> PathVerdict {
    let deadline = Instant::now() + BUDGET;
    let status = match run(&["status", "--json"], deadline) {
        Some(CliOut { text, timed_out: false }) => text,
        _ => return PathVerdict::Unknown,
    };
    let peers = parse_status(&status);
    // Not running, logged out, output Ferry can't read, or the route's adapter isn't this Tailscale.
    if peers.is_empty() || !self_ips(&status).iter().any(|ip| vpn.addrs.contains(ip)) {
        return PathVerdict::Unknown;
    }
    let peer = peer_for(&peers, server_ip);
    let ping = match peer.filter(|p| p.online).and_then(|p| p.tailscale_ips.first()) {
        Some(ip) => match run(&["ping", "-c", "3", &ip.to_string()], deadline) {
            None => return PathVerdict::Unknown,
            Some(CliOut { text, timed_out }) => {
                let ping = parse_ping(&text);
                if timed_out {
                    // Killed at the deadline: only a pong already showing the link leaves this
                    // network counts; anything else is unproven.
                    return match (&ping, verdict(peer, ping.clone(), local)) {
                        (PingPath::NoReply, _) | (_, PathVerdict::Direct) => PathVerdict::Unknown,
                        (_, v) => v,
                    };
                }
                ping
            }
        },
        None => PingPath::NoReply,
    };
    verdict(peer, ping, local)
}

/// This device's own Tailscale IPs (`Self.TailscaleIPs`).
pub fn self_ips(json: &str) -> Vec<IpAddr> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    v["Self"]["TailscaleIPs"].as_array().into_iter().flatten().filter_map(|s| s.as_str()?.parse().ok()).collect()
}

/// The peers in `tailscale status --json` (not this device); empty if it can't be read.
pub fn parse_status(json: &str) -> Vec<Peer> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    let strs = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    let prefix = |s: &str| {
        let (ip, len) = s.split_once('/')?;
        Some((ip.parse().ok()?, len.parse().ok()?))
    };
    v["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .map(|p| Peer {
            tailscale_ips: strs(&p["TailscaleIPs"]).iter().filter_map(|s| s.parse().ok()).collect(),
            // An exit node's default route doesn't make it the server's router.
            routes: strs(&p["PrimaryRoutes"]).iter().filter_map(|s| prefix(s)).filter(|&(_, len)| len > 0).collect(),
            online: p["Online"].as_bool().unwrap_or(false),
            host: p["HostName"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

/// The peer whose Tailscale IP is `ip`, else the one with the longest route holding it.
pub fn peer_for(peers: &[Peer], ip: IpAddr) -> Option<&Peer> {
    peers.iter().find(|p| p.tailscale_ips.contains(&ip)).or_else(|| {
        peers
            .iter()
            .filter_map(|p| {
                let best = p.routes.iter().filter(|&&(net, len)| in_prefix(ip, net, len)).map(|&(_, len)| len).max()?;
                Some(((best, p.online), p))
            })
            .max_by_key(|&(key, _)| key)
            .map(|(_, p)| p)
    })
}

/// The last `pong from host (ip) via <path> in <time>` line; the ping stops once it's direct.
pub fn parse_ping(out: &str) -> PingPath {
    let via = out
        .lines()
        .rev()
        .filter(|l| l.starts_with("pong from "))
        .find_map(|l| l.split_once(") via ")?.1.rsplit_once(" in ").map(|(via, _)| via.trim()));
    match via {
        None => PingPath::NoReply,
        Some(via) => via.parse().map_or_else(|_| PingPath::Relayed(via.to_owned()), PingPath::Direct),
    }
}

/// `Direct` only for an online peer reached at an endpoint inside the local adapter's subnet.
pub fn verdict(peer: Option<&Peer>, ping: PingPath, local: &Adapter) -> PathVerdict {
    let Some(peer) = peer else {
        return PathVerdict::NotDirect("no Tailscale device serves this address".into());
    };
    if !peer.online {
        return PathVerdict::NotDirect(format!("{} is offline", peer.host));
    }
    match ping {
        PingPath::Direct(at) if local.prefixes.iter().any(|&(net, len)| len > 0 && in_prefix(at.ip(), net, len)) => PathVerdict::Direct,
        PingPath::Direct(at) => PathVerdict::NotDirect(format!("connected over the internet at {}", at.ip())),
        PingPath::Relayed(via) => PathVerdict::NotDirect(format!("relayed through {via}")),
        PingPath::NoReply => PathVerdict::NotDirect(format!("{} didn't answer", peer.host)),
    }
}

/// `tailscale.exe` on PATH, else in `%ProgramFiles%\Tailscale`.
pub fn find_cli() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("tailscale.exe")).find(|f| f.is_file()));
    on_path.or_else(|| {
        let f = PathBuf::from(std::env::var_os("ProgramFiles")?).join("Tailscale").join("tailscale.exe");
        f.is_file().then_some(f)
    })
}

/// Runs the CLI without a console window; `None` when it can't start. On the deadline the child
/// is killed and what it printed so far comes back with `timed_out`.
pub fn run_cli(args: &[&str], deadline: Instant) -> Option<CliOut> {
    let mut cmd = Command::new(find_cli()?);
    cmd.args(args);
    run_until(cmd, deadline)
}

fn run_until(mut cmd: Command, deadline: Instant) -> Option<CliOut> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // Read on a thread so a large status can't fill the pipe; EOF comes when the CLI exits.
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    if let Ok(text) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        let _ = child.wait();
        return Some(CliOut { text, timed_out: false });
    }
    let _ = child.kill();
    let _ = child.wait();
    // The pipe closes with the child, so the reader hands over what it already had.
    let text = rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    Some(CliOut { text, timed_out: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = include_str!("fixtures/tailscale_status.json");
    const OFFICE_NAS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 10));
    const DIRECT_LAN: &str = "pong from office-nas (100.64.0.2) via 192.0.2.10:41641 in 2ms\n";
    const DERP: &str = "pong from home-nas (100.64.0.3) via DERP(fra) in 40ms\npong from home-nas (100.64.0.3) via DERP(fra) in 41ms\npong from home-nas (100.64.0.3) via DERP(fra) in 39ms\ndirect connection not established\n";
    const PUBLIC: &str = "pong from home-nas (100.64.0.3) via DERP(fra) in 40ms\npong from home-nas (100.64.0.3) via 203.0.113.200:41641 in 30ms\n";
    /// Newer CLIs add the peer API port inside the parentheses.
    const WITH_PORT: &str = "pong from office-nas (100.64.0.2, 45312) via 192.0.2.10:41641 in 2ms\n";

    fn tailscale() -> Adapter {
        Adapter {
            index: 25,
            alias: "Tailscale".into(),
            description: "Tailscale Tunnel".into(),
            if_type: 53,
            hardware: false,
            addrs: vec!["100.64.0.1".parse().unwrap()],
            prefixes: vec![("100.64.0.1".parse().unwrap(), 32)],
            gateway_macs: vec![],
            up: true,
        }
    }

    fn out(text: &str) -> Option<CliOut> {
        Some(CliOut { text: text.into(), timed_out: false })
    }

    /// A CLI whose status works and whose ping is killed at the deadline after printing `partial`.
    fn ping_times_out_after(partial: &'static str) -> impl Fn(&[&str], Instant) -> Option<CliOut> {
        move |args, _| match args[0] {
            "status" => out(STATUS),
            _ => Some(CliOut { text: partial.into(), timed_out: true }),
        }
    }

    fn ethernet() -> Adapter {
        Adapter {
            index: 2,
            alias: "Ethernet".into(),
            description: "Ethernet Controller".into(),
            if_type: 6,
            hardware: true,
            addrs: vec!["192.0.2.150".parse().unwrap()],
            prefixes: vec![("192.0.2.150".parse().unwrap(), 24)],
            gateway_macs: vec![[0x02, 0, 0, 0, 0, 1]],
            up: true,
        }
    }

    fn peers() -> Vec<Peer> {
        parse_status(STATUS)
    }

    fn host(peers: &[Peer], ip: &str) -> Option<String> {
        peer_for(peers, ip.parse().unwrap()).map(|p| p.host.clone())
    }

    fn not_direct(v: PathVerdict) -> String {
        match v {
            PathVerdict::NotDirect(why) => why,
            other => panic!("expected NotDirect, got {other:?}"),
        }
    }

    #[test]
    fn status_fixture_parses() {
        let ps = peers();
        assert_eq!(ps.len(), 3, "this device (Self) isn't a peer");
        let office = ps.iter().find(|p| p.host == "office-nas").unwrap();
        assert_eq!(office.routes, vec![("192.0.2.0".parse().unwrap(), 24)]);
        assert!(office.online);
        assert!(!ps.iter().find(|p| p.host == "home-nas").unwrap().online);
        assert!(parse_status("not json").is_empty());
    }

    #[test]
    fn peer_found_by_longest_prefix_route() {
        let ps = peers();
        assert_eq!(host(&ps, "192.0.2.10").as_deref(), Some("office-nas"));
        assert_eq!(host(&ps, "192.0.2.77").as_deref(), Some("office-nas"), "inside the /24");
        assert_eq!(host(&ps, "198.51.100.10").as_deref(), Some("home-nas"));
        assert_eq!(host(&ps, "203.0.113.4").as_deref(), Some("lab-nas"));
        assert_eq!(host(&ps, "10.0.0.1"), None);
        // A narrower route wins over a wider one.
        let mut ps = ps;
        ps.push(Peer { tailscale_ips: vec![], routes: vec![("192.0.2.0".parse().unwrap(), 28)], online: true, host: "NARROW".into() });
        assert_eq!(host(&ps, "192.0.2.10").as_deref(), Some("NARROW"));
    }

    #[test]
    fn peer_found_by_tailscale_ip() {
        let ps = peers();
        assert_eq!(host(&ps, "100.64.0.4").as_deref(), Some("lab-nas"));
        assert_eq!(host(&ps, "fd7a:115c:a1e0::3").as_deref(), Some("home-nas"));
    }

    #[test]
    fn ping_lines_parse() {
        assert_eq!(parse_ping(DIRECT_LAN), PingPath::Direct("192.0.2.10:41641".parse().unwrap()));
        assert_eq!(parse_ping(DERP), PingPath::Relayed("DERP(fra)".into()));
        assert_eq!(parse_ping(PUBLIC), PingPath::Direct("203.0.113.200:41641".parse().unwrap()), "the last pong wins");
        assert_eq!(
            parse_ping("pong from x (100.64.0.9) via [2001:db8::5]:41641 in 9ms\n"),
            PingPath::Direct("[2001:db8::5]:41641".parse().unwrap())
        );
        assert_eq!(parse_ping(WITH_PORT), PingPath::Direct("192.0.2.10:41641".parse().unwrap()));
        assert_eq!(parse_ping("ping \"100.64.0.3\" timed out\nno reply\n"), PingPath::NoReply);
    }

    #[test]
    fn ping_direct_lan_is_direct() {
        let ps = peers();
        let v = verdict(peer_for(&ps, OFFICE_NAS), parse_ping(DIRECT_LAN), &ethernet());
        assert_eq!(v, PathVerdict::Direct);
    }

    #[test]
    fn ping_derp_is_not_direct() {
        let ps = peers();
        let why = not_direct(verdict(peer_for(&ps, OFFICE_NAS), parse_ping(DERP), &ethernet()));
        assert!(why.contains("relayed"), "{why}");
    }

    #[test]
    fn ping_public_endpoint_is_not_direct() {
        let ps = peers();
        let why = not_direct(verdict(peer_for(&ps, OFFICE_NAS), parse_ping(PUBLIC), &ethernet()));
        assert!(why.contains("internet") && why.contains("203.0.113.200"), "{why}");
        // A default route on the local adapter isn't "this network".
        let default_route = Adapter { prefixes: vec![("0.0.0.0".parse().unwrap(), 0), ("192.0.2.150".parse().unwrap(), 24)], ..ethernet() };
        assert!(matches!(verdict(peer_for(&ps, OFFICE_NAS), parse_ping(PUBLIC), &default_route), PathVerdict::NotDirect(_)));
    }

    #[test]
    fn offline_peer_is_not_direct() {
        let ps = peers();
        let home = peer_for(&ps, "198.51.100.10".parse().unwrap());
        // Even a stale direct-looking answer can't make an offline peer direct.
        let local = Adapter { prefixes: vec![("198.51.100.50".parse().unwrap(), 24)], ..ethernet() };
        let why = not_direct(verdict(home, PingPath::Direct("198.51.100.10:41641".parse().unwrap()), &local));
        assert!(why.contains("offline"), "{why}");
        assert!(matches!(verdict(None, PingPath::NoReply, &ethernet()), PathVerdict::NotDirect(_)));
    }

    #[test]
    fn check_pings_the_peers_tailscale_ip_within_one_budget() {
        let calls = std::cell::RefCell::new(vec![]);
        let deadlines = std::cell::RefCell::new(vec![]);
        let run = |args: &[&str], d: Instant| -> Option<CliOut> {
            calls.borrow_mut().push(args.join(" "));
            deadlines.borrow_mut().push(d);
            out(if args[0] == "status" { STATUS } else { DIRECT_LAN })
        };
        assert_eq!(check(OFFICE_NAS, &tailscale(), &ethernet(), &run), PathVerdict::Direct);
        assert_eq!(*calls.borrow(), ["status --json", "ping -c 3 100.64.0.2"]);
        let d = deadlines.borrow();
        assert_eq!(d[0], d[1], "status and ping share one deadline");
    }

    #[test]
    fn check_is_unknown_when_the_cli_fails_or_times_out() {
        let ts = tailscale();
        assert_eq!(check(OFFICE_NAS, &ts, &ethernet(), &|_, _| None), PathVerdict::Unknown);
        let status_times_out = |_: &[&str], _: Instant| Some(CliOut { text: STATUS.into(), timed_out: true });
        assert_eq!(check(OFFICE_NAS, &ts, &ethernet(), &status_times_out), PathVerdict::Unknown);
        assert_eq!(check(OFFICE_NAS, &ts, &ethernet(), &ping_times_out_after("")), PathVerdict::Unknown, "no pong at all");
        assert_eq!(check(OFFICE_NAS, &ts, &ethernet(), &|_, _| out("{}")), PathVerdict::Unknown, "no peers to judge by");
    }

    #[test]
    fn a_timed_out_ping_keeps_evidence_that_the_link_leaves_this_network() {
        let ts = tailscale();
        let derp = "pong from office-nas (100.64.0.2) via DERP(fra) in 40ms\n";
        let why = not_direct(check(OFFICE_NAS, &ts, &ethernet(), &ping_times_out_after(derp)));
        assert!(why.contains("relayed"), "{why}");
        let public = "pong from office-nas (100.64.0.2) via 203.0.113.200:41641 in 30ms\n";
        let why = not_direct(check(OFFICE_NAS, &ts, &ethernet(), &ping_times_out_after(public)));
        assert!(why.contains("internet"), "{why}");
        // A direct LAN pong from a ping that then hung is not proof enough.
        assert_eq!(check(OFFICE_NAS, &ts, &ethernet(), &ping_times_out_after(DIRECT_LAN)), PathVerdict::Unknown);
    }

    #[test]
    fn an_adapter_that_isnt_this_tailscale_is_unknown() {
        let other = Adapter { addrs: vec!["100.80.0.2".parse().unwrap()], ..tailscale() };
        let run = |args: &[&str], _: Instant| -> Option<CliOut> {
            assert_eq!(args[0], "status", "no ping when the adapter isn't Tailscale's");
            out(STATUS)
        };
        assert_eq!(check(OFFICE_NAS, &other, &ethernet(), &run), PathVerdict::Unknown);
        assert_eq!(self_ips(STATUS), ["100.64.0.1".parse::<IpAddr>().unwrap(), "fd7a:115c:a1e0::1".parse().unwrap()]);
    }

    #[test]
    fn runner_kills_the_child_at_the_deadline() {
        let mut slow = Command::new("ping");
        slow.args(["-n", "30", "127.0.0.1"]);
        let start = Instant::now();
        let r = run_until(slow, start + Duration::from_millis(1500)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(4), "{:?}", start.elapsed());
        assert!(r.timed_out);
        assert!(r.text.contains("127.0.0.1"), "partial output kept: {:?}", r.text);
        let mut quick = Command::new("cmd");
        quick.args(["/c", "echo pong"]);
        let r = run_until(quick, Instant::now() + BUDGET).unwrap();
        assert_eq!((r.text.trim(), r.timed_out), ("pong", false));
        assert_eq!(run_until(Command::new("no-such-ferry-cli.exe"), Instant::now() + BUDGET), None);
    }

    #[test]
    fn check_never_pings_an_offline_peer() {
        let run = |args: &[&str], _: Instant| -> Option<CliOut> {
            assert_eq!(args[0], "status", "no ping for an offline peer");
            out(STATUS)
        };
        let why = not_direct(check("198.51.100.10".parse().unwrap(), &tailscale(), &ethernet(), &run));
        assert!(why.contains("home-nas is offline"), "{why}");
    }
}
