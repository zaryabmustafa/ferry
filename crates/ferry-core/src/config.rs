use crate::ipc::Side;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const GB: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The server whose folders are mirrored. Older files call it `office`.
    #[serde(alias = "office")]
    pub source: Server,
    /// The server that becomes the exact copy. Older files call it `home`.
    #[serde(alias = "home")]
    pub destination: Server,
    pub pairs: Vec<Pair>,
    pub options: Options,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// What the user calls this place ("Office"); empty means the role name. See `display_name`.
    pub name: String,
    pub lan_host: String,
    /// Address of this server away from its LAN (a VPN or overlay name). Older files call it `tailscale_host`.
    #[serde(alias = "tailscale_host")]
    pub remote_host: String,
    pub credential_target: String,
    /// SMB user name, shown in setup. The password lives only in Credential Manager.
    pub user: String,
    pub fingerprints: Vec<Fingerprint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub gateway_mac: [u8; 6],
    pub server_lan_ip: IpAddr,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub trust_vpn: bool,
    #[serde(default)]
    pub trusted_adapter: String,
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self {
            gateway_mac: [0; 6],
            server_lan_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            name: String::new(),
            trust_vpn: false,
            trusted_adapter: String::new(),
        }
    }
}

/// Vec order = priority. `id` is a stable slug used for cargo subfolders.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pair {
    pub id: String,
    #[serde(alias = "office_path")]
    pub source_path: String,
    #[serde(alias = "home_path")]
    pub destination_path: String,
    pub enabled: bool,
    pub allow_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub cap_bytes: u64,
    pub cargo_dir: Option<PathBuf>,
    pub min_free_bytes: u64,
    pub recheck_minutes: u32,
    pub stable_minutes: u32,
    pub pause_on_battery_saver: bool,
    pub delete_confirm_percent: u8,
    pub delete_confirm_count: u32,
    pub notify_unload_done: bool,
    /// HKCU Run key, applied by the engine at start and on ReloadConfig.
    pub start_with_windows: bool,
    /// Treat a destination folder with nothing in it as expected (a first run), not as a wiped share.
    #[serde(alias = "home_starts_empty")]
    pub destination_starts_empty: bool,
    /// Check for a new release once a day and install it silently.
    pub auto_update: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cap_bytes: 50 * GB,
            cargo_dir: None,
            min_free_bytes: 10 * GB,
            recheck_minutes: 30,
            stable_minutes: 10,
            pause_on_battery_saver: true,
            delete_confirm_percent: 10,
            delete_confirm_count: 50,
            notify_unload_done: false,
            start_with_windows: true,
            destination_starts_empty: false,
            auto_update: true,
        }
    }
}

impl Server {
    /// `\\{lan_host}\{share_path}`
    pub fn lan_root(&self, share_path: &str) -> PathBuf {
        unc(&self.lan_host, share_path)
    }

    /// `\\{remote_host}\{share_path}`; `None` when no remote address is set.
    pub fn remote_root(&self, share_path: &str) -> Option<PathBuf> {
        (!self.remote_host.is_empty()).then(|| unc(&self.remote_host, share_path))
    }
}

/// The name the user gave a side, trimmed, or "Source" / "Destination" when there is none.
pub fn chosen_name(cfg: &Config, side: Side) -> String {
    let (srv, role) = match side {
        Side::Source => (&cfg.source, "Source"),
        Side::Destination => (&cfg.destination, "Destination"),
    };
    let name = srv.name.trim();
    if name.is_empty() { role } else { name }.to_string()
}

/// What Ferry calls a side in its messages: `chosen_name`, or, should both sides end up with the
/// same name (ignoring case), that name with "(source)" / "(destination)" so the sides never read
/// alike and the engine's message matching can't confuse them. Setup refuses such names.
pub fn display_name(cfg: &Config, side: Side) -> String {
    let name = chosen_name(cfg, side);
    if chosen_name(cfg, Side::Source).to_lowercase() != chosen_name(cfg, Side::Destination).to_lowercase() {
        return name;
    }
    let role = if side == Side::Source { "source" } else { "destination" };
    format!("{name} ({role})")
}

/// An IP as a UNC host name; IPv6 takes the `ipv6-literal.net` form Windows understands.
pub fn unc_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("{}.ipv6-literal.net", v6.to_string().replace(':', "-")),
    }
}

/// `\\{host}\{share_path}`
pub fn unc(host: &str, share_path: &str) -> PathBuf {
    PathBuf::from(format!(r"\\{}\{}", host, share_path.replace('/', r"\")))
}

/// Id of another pair (not `except`) whose destination is `dest`, holds it or sits inside it.
/// Two such pairs would each see the other's files as extra and delete them, so they are refused.
/// Compared like SMB paths: case-insensitive, `/` and `\` alike.
pub fn overlapping_destination<'a>(pairs: &'a [Pair], dest: &str, except: Option<&str>) -> Option<&'a str> {
    let norm = |s: &str| format!("{}\\", crate::entry::fold(s.replace('/', "\\").trim_matches('\\')));
    let d = norm(dest);
    pairs
        .iter()
        .find(|p| Some(p.id.as_str()) != except && {
            let o = norm(&p.destination_path);
            d.starts_with(&o) || o.starts_with(&d)
        })
        .map(|p| p.id.as_str())
}

/// Missing file -> `Config::default()`.
pub fn load(path: &Path) -> io::Result<Config> {
    match fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e),
    }
}

/// Writes a temp file beside `path`, flushes it, then renames over it.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    let s = toml::to_string_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    crate::cargo::write_atomic(path, s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let o = Options::default();
        assert_eq!(o.min_free_bytes, 10 * GB);
        assert_eq!(o.recheck_minutes, 30);
        assert_eq!(o.stable_minutes, 10);
        assert!(o.pause_on_battery_saver);
        assert_eq!(o.delete_confirm_percent, 10);
        assert_eq!(o.delete_confirm_count, 50);
        assert!(!o.notify_unload_done);
        assert!(o.start_with_windows);
    }

    #[test]
    fn old_config_has_auto_update_on() {
        let c: Config = toml::from_str("[options]
start_with_windows = false
").unwrap();
        assert!(c.options.auto_update);
        assert!(!c.options.start_with_windows);
    }

    #[test]
    fn populated_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut c = Config::default();
        c.source.fingerprints.push(Fingerprint { gateway_mac: [1, 2, 3, 4, 5, 255], server_lan_ip: "198.51.100.10".parse().unwrap(), ..Default::default() });
        c.options.cargo_dir = Some(PathBuf::from(r"D:\ferry cargo"));
        save(&p, &c).unwrap();
        assert_eq!(load(&p).unwrap(), c);
    }

    #[test]
    fn server_user_is_optional_in_older_files() {
        let c: Config = toml::from_str("[office]\nlan_host = \"192.0.2.10\"\n").unwrap();
        assert_eq!((c.source.lan_host.as_str(), c.source.user.as_str()), ("192.0.2.10", ""));
    }

    #[test]
    fn missing_file_loads_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn ip_hosts_for_unc() {
        assert_eq!(unc_host("198.51.100.10".parse().unwrap()), "198.51.100.10");
        assert_eq!(unc_host("fe80::1".parse().unwrap()), "fe80--1.ipv6-literal.net");
    }

    #[test]
    fn roots_are_unc() {
        let s = Server { lan_host: "198.51.100.10".into(), remote_host: "office-nas".into(), ..Default::default() };
        assert_eq!(s.lan_root("repositories"), PathBuf::from(r"\\198.51.100.10\repositories"));
        assert_eq!(s.remote_root("backups/vm"), Some(PathBuf::from(r"\\office-nas\backups\vm")));
        assert_eq!(Server::default().remote_root("repositories"), None);
    }

    #[test]
    fn old_config_with_tailscale_host_loads() {
        let c: Config = toml::from_str(
            r#"
[office]
lan_host = "192.0.2.10"
tailscale_host = "office-nas"
fingerprints = [{ gateway_mac = [1, 2, 3, 4, 5, 6], server_lan_ip = "192.0.2.10" }]

[home]
lan_host = "198.51.100.100"
tailscale_host = "home-nas"
fingerprints = [{ gateway_mac = [6, 5, 4, 3, 2, 1], server_lan_ip = "198.51.100.100" }]
"#,
        )
        .unwrap();
        assert_eq!((c.source.remote_host.as_str(), c.destination.remote_host.as_str()), ("office-nas", "home-nas"));
        let f = &c.source.fingerprints[0];
        assert_eq!((f.name.as_str(), f.trust_vpn, f.trusted_adapter.as_str()), ("", false, ""));
        assert!(!c.options.destination_starts_empty);
    }

    #[test]
    fn config_with_every_old_name_loads() {
        let c: Config = toml::from_str(
            r#"
[office]
lan_host = "192.0.2.10"
tailscale_host = "office-nas"

[home]
lan_host = "198.51.100.100"

[[pairs]]
id = "repositories"
office_path = "repositories"
home_path = "backups/repos"
enabled = true

[options]
home_starts_empty = true
"#,
        )
        .unwrap();
        assert_eq!((c.source.lan_host.as_str(), c.source.remote_host.as_str()), ("192.0.2.10", "office-nas"));
        assert_eq!(c.destination.lan_host, "198.51.100.100");
        assert_eq!((c.source.name.as_str(), c.destination.name.as_str()), ("", ""));
        let p = &c.pairs[0];
        assert_eq!((p.source_path.as_str(), p.destination_path.as_str()), ("repositories", "backups/repos"));
        assert!(c.options.destination_starts_empty);
    }

    #[test]
    fn save_writes_the_new_names_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut c = Config::default();
        c.source.name = "Office".into();
        c.source.lan_host = "192.0.2.10".into();
        c.source.remote_host = "office-nas".into();
        c.destination.name = "Home".into();
        c.destination.lan_host = "198.51.100.100".into();
        c.pairs.push(Pair { id: "r".into(), source_path: "repositories".into(), destination_path: "backups/repos".into(), enabled: true, allow_empty: false });
        c.options.destination_starts_empty = true;
        save(&p, &c).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        for new in ["[source]", "[destination]", "source_path", "destination_path", "destination_starts_empty", "remote_host"] {
            assert!(text.contains(new), "{new} missing in {text}");
        }
        for old in ["[office]", "[home]", "office_path", "home_path", "home_starts_empty", "tailscale_host"] {
            assert!(!text.contains(old), "{old} still in {text}");
        }
        assert_eq!(load(&p).unwrap(), c);
    }

    #[test]
    fn equal_names_are_told_apart() {
        let mut c = Config::default();
        c.destination.name = " source ".into();
        assert_eq!(chosen_name(&c, Side::Destination), "source");
        assert_eq!((display_name(&c, Side::Source), display_name(&c, Side::Destination)), ("Source (source)".into(), "source (destination)".into()));
        c.source.name = "Hütte".into();
        c.destination.name = "HÜTTE".into();
        assert_eq!((display_name(&c, Side::Source), display_name(&c, Side::Destination)), ("Hütte (source)".into(), "HÜTTE (destination)".into()));
        c.destination.name = "Lake house".into();
        assert_eq!(display_name(&c, Side::Source), "Hütte");
    }

    #[test]
    fn display_name_is_the_trimmed_name_or_the_role() {
        let mut c = Config::default();
        assert_eq!((display_name(&c, Side::Source), display_name(&c, Side::Destination)), ("Source".into(), "Destination".into()));
        c.source.name = "  Lake house ".into();
        c.destination.name = "   ".into();
        assert_eq!((display_name(&c, Side::Source), display_name(&c, Side::Destination)), ("Lake house".into(), "Destination".into()));
    }

    #[test]
    fn overlapping_destinations_are_found() {
        let pair = |id: &str, d: &str| Pair { id: id.into(), destination_path: d.into(), ..Default::default() };
        let pairs = vec![pair("a", "backups"), pair("b", "Media/Photos")];
        assert_eq!(overlapping_destination(&pairs, "backups/vm", None), Some("a")); // inside
        assert_eq!(overlapping_destination(&pairs, r"BACKUPS\", None), Some("a")); // same
        assert_eq!(overlapping_destination(&pairs, "media", None), Some("b")); // holds it
        assert_eq!(overlapping_destination(&pairs, "backups2", None), None); // a prefix is not a parent
        assert_eq!(overlapping_destination(&pairs, "media/photos-old", None), None);
        assert_eq!(overlapping_destination(&pairs, "backups", Some("a")), None); // editing itself
    }
}
