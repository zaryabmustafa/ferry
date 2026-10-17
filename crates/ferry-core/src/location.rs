use crate::config::{Config, Fingerprint};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub index: u32,
    /// Windows' friendly name ("Ethernet", "Tailscale"); what VPN trust is bound to.
    pub alias: String,
    pub description: String,
    /// IANA ifType: 6 Ethernet, 53 proprietary virtual, 131 tunnel.
    pub if_type: u32,
    /// Windows marks it a hardware interface.
    pub hardware: bool,
    pub addrs: Vec<IpAddr>,
    /// Unicast addresses with their on-link prefix length.
    pub prefixes: Vec<(IpAddr, u8)>,
    pub gateway_macs: Vec<[u8; 6]>,
    pub up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Local,
    Vpn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    #[serde(alias = "Office")]
    Source,
    #[serde(alias = "Home")]
    Destination,
    Elsewhere,
}

/// A VPN path checker's answer for traffic to the server through a VPN adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathVerdict {
    Direct,
    NotDirect(String),
    Unknown,
}

/// How bulk copies would reach the server at a recognised location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkPath {
    Direct,
    ViaVpn { adapter: String, verdict: PathVerdict, trusted: bool },
    Blocked(String),
}

impl BulkPath {
    /// A checker's `NotDirect` is never overridden by trust.
    pub fn allows_bulk(&self) -> bool {
        match self {
            BulkPath::Direct => true,
            BulkPath::ViaVpn { verdict: PathVerdict::Direct, .. } => true,
            BulkPath::ViaVpn { verdict: PathVerdict::Unknown, trusted, .. } => *trusted,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub location: Location,
    /// The matched fingerprint's server LAN IP: bulk copies go exactly there (Ruling R35).
    pub server_ip: Option<IpAddr>,
    /// `None` only when `Elsewhere`.
    pub bulk: Option<BulkPath>,
    /// The matched fingerprint's recorded network name, if it has one.
    pub network_name: Option<String>,
    /// The interface Windows routed `server_ip` through when `bulk` was decided; `None` when
    /// `Elsewhere` or when there was no route.
    pub route_if: Option<u32>,
}

impl Located {
    pub const ELSEWHERE: Located = Located { location: Location::Elsewhere, server_ip: None, bulk: None, network_name: None, route_if: None };
}

const VPN_NAMES: [&str; 6] = ["tailscale", "wireguard", "wintun", "tap-windows", "zerotier", "openvpn"];

/// Spec 4.2: tunnels by type or driver name; a virtual adapter counts as local only when it's a
/// known virtual switch holding the default gateway (Hyper-V vEthernet), Ruling V4a.
pub fn classify(a: &Adapter) -> AdapterKind {
    let names = format!("{} {}", a.alias, a.description).to_lowercase();
    let vswitch = (a.description.contains("Hyper-V Virtual Ethernet") || a.alias.starts_with("vEthernet")) && !a.gateway_macs.is_empty();
    if matches!(a.if_type, 53 | 131) || VPN_NAMES.iter().any(|n| names.contains(n)) || (!a.hardware && !vswitch) {
        AdapterKind::Vpn
    } else {
        AdapterKind::Local
    }
}

/// Tailscale's adapter: by name, or by an address in its CGNAT (100.64.0.0/10)
/// or ULA (fd7a:115c:a1e0::/48) range. A hint only; `classify` decides VPN or not.
pub fn is_tailscale(a: &Adapter) -> bool {
    format!("{} {}", a.alias, a.description).to_lowercase().contains("tailscale")
        || a.addrs.iter().any(|ip| match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                o[0] == 100 && (o[1] & 0xc0) == 64
            }
            IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
        })
}

pub fn resolve_location(
    cfg: &Config,
    adapters: &[Adapter],
    best_if: &dyn Fn(IpAddr) -> Option<u32>,
    probe_445: &dyn Fn(IpAddr) -> bool,
    check_vpn: &dyn Fn(IpAddr, &Adapter, &Adapter) -> PathVerdict,
) -> Location {
    locate(cfg, adapters, best_if, probe_445, check_vpn).location
}

/// The location is recognised by a fingerprint's gateway MAC on an up local adapter plus a
/// reachable server; the route then decides bulk. A match that allows bulk wins (source before
/// destination, so a blocked source never hides a usable destination); else the first blocked
/// match, source before destination (on Wi-Fi and cable at once, the cable that allows bulk wins).
/// `check_vpn(server_ip, vpn_adapter, local_adapter)` runs at most once per (server IP, VPN
/// adapter) within one call (Ruling V4d).
pub fn locate(
    cfg: &Config,
    adapters: &[Adapter],
    best_if: &dyn Fn(IpAddr) -> Option<u32>,
    probe_445: &dyn Fn(IpAddr) -> bool,
    check_vpn: &dyn Fn(IpAddr, &Adapter, &Adapter) -> PathVerdict,
) -> Located {
    let seen = std::cell::RefCell::new(Vec::<(IpAddr, u32, PathVerdict)>::new());
    let check_vpn = |ip: IpAddr, vpn: &Adapter, local: &Adapter| {
        if let Some((.., v)) = seen.borrow().iter().find(|(i, x, _)| (*i, *x) == (ip, vpn.index)) {
            return v.clone();
        }
        let v = check_vpn(ip, vpn, local);
        seen.borrow_mut().push((ip, vpn.index, v.clone()));
        v
    };
    let mut blocked = None;
    for (location, srv) in [(Location::Source, &cfg.source), (Location::Destination, &cfg.destination)] {
        for fp in &srv.fingerprints {
            let locals = adapters
                .iter()
                .filter(|a| a.up && classify(a) == AdapterKind::Local && a.gateway_macs.contains(&fp.gateway_mac));
            for local in locals {
                if !probe_445(fp.server_lan_ip) {
                    break;
                }
                let route_if = best_if(fp.server_lan_ip);
                let bulk = bulk_path(fp, local, adapters, route_if, &check_vpn);
                let allows = bulk.allows_bulk();
                let l = Located {
                    location,
                    server_ip: Some(fp.server_lan_ip),
                    bulk: Some(bulk),
                    network_name: (!fp.name.is_empty()).then(|| fp.name.clone()),
                    route_if,
                };
                if allows {
                    return l;
                }
                blocked.get_or_insert(l);
            }
        }
    }
    blocked.unwrap_or(Located::ELSEWHERE)
}

fn bulk_path(
    fp: &Fingerprint,
    local: &Adapter,
    adapters: &[Adapter],
    route_if: Option<u32>,
    check_vpn: &dyn Fn(IpAddr, &Adapter, &Adapter) -> PathVerdict,
) -> BulkPath {
    let ip = fp.server_lan_ip;
    let Some(idx) = route_if else {
        return BulkPath::Blocked(format!("Windows has no route to {ip}."));
    };
    if idx == local.index {
        return BulkPath::Direct;
    }
    match adapters.iter().find(|a| a.index == idx) {
        Some(vpn) if classify(vpn) == AdapterKind::Vpn => BulkPath::ViaVpn {
            adapter: vpn.alias.clone(),
            verdict: check_vpn(ip, vpn, local),
            trusted: fp.trust_vpn && !vpn.alias.is_empty() && fp.trusted_adapter == vpn.alias,
        },
        Some(other) => BulkPath::Blocked(format!(
            "Windows sends traffic for {ip} through {}, not {}, the adapter on this network.",
            other.alias, local.alias
        )),
        None => BulkPath::Blocked(format!("Windows sends traffic for {ip} through an adapter Ferry can't see.")),
    }
}

/// Why bulk copies won't run here, for the user; `None` when they may.
pub fn reason_text(b: &BulkPath, ip: IpAddr) -> Option<String> {
    if b.allows_bulk() {
        return None;
    }
    match b {
        BulkPath::ViaVpn { adapter, verdict: PathVerdict::NotDirect(why), .. } => Some(format!(
            "Windows sends traffic for {ip} through {adapter}, and that link doesn't stay on this network ({why})."
        )),
        BulkPath::ViaVpn { adapter, .. } => Some(format!(
            "Windows sends traffic for {ip} through {adapter}, and Ferry can't confirm that link stays on this network."
        )),
        BulkPath::Blocked(why) => Some(why.clone()),
        BulkPath::Direct => None,
    }
}

/// "Remember this network": the up local adapter with a gateway MAC whose subnet holds
/// `server`. Only when no local adapter's subnet holds it, the first local adapter with a
/// gateway MAC (Windows' adapter order), and only if Windows routes `server` through it
/// (`best_if`); the server's subnet without a resolved router is an error, not a fallback
/// (Ruling V4c).
pub fn pick_local<'a>(adapters: &'a [Adapter], server: IpAddr, best_if: &dyn Fn(IpAddr) -> Option<u32>) -> Result<&'a Adapter, String> {
    let locals = || adapters.iter().filter(|a| a.up && classify(a) == AdapterKind::Local);
    let mut on_subnet = locals().filter(|a| a.prefixes.iter().any(|&(net, len)| len > 0 && in_prefix(server, net, len))).peekable();
    if on_subnet.peek().is_some() {
        return on_subnet.find(|a| !a.gateway_macs.is_empty()).ok_or_else(|| {
            format!("Ferry can't see the router on the network that holds {server}. Try again in a moment.")
        });
    }
    let a = locals().find(|a| !a.gateway_macs.is_empty()).ok_or_else(|| {
        "This laptop isn't on a network with a router Ferry can see. Connect to that server's network, then try again.".to_string()
    })?;
    let route = best_if(server);
    if route == Some(a.index) {
        return Ok(a);
    }
    let via = route.and_then(|i| adapters.iter().find(|x| x.index == i && !x.alias.is_empty())).map_or("another network", |x| x.alias.as_str());
    Err(format!("This server isn't on this network. Windows reaches it through {via}."))
}

/// Whether `ip` lies in `net/len`.
pub fn in_prefix(ip: IpAddr, net: IpAddr, len: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(len.min(32))).unwrap_or(0);
            (u32::from(a) ^ u32::from(b)) & mask == 0
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(len.min(128))).unwrap_or(0);
            (u128::from(a) ^ u128::from(b)) & mask == 0
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Fingerprint;
    use std::net::Ipv4Addr;

    const SOURCE_GW: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
    const DESTINATION_WIFI_GW: [u8; 6] = [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x01];
    const DESTINATION_CABLE_GW: [u8; 6] = [0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x02];
    const SOURCE_SRV: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 10));
    const DESTINATION_SRV: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));

    fn adapter(index: u32, desc: &str, addr: &str, gw: &[[u8; 6]]) -> Adapter {
        let ip: IpAddr = addr.parse().unwrap();
        Adapter {
            index,
            alias: desc.into(),
            description: desc.into(),
            if_type: 6,
            hardware: true,
            addrs: vec![ip],
            prefixes: vec![(ip, 24)],
            gateway_macs: gw.to_vec(),
            up: true,
        }
    }

    fn tailscale() -> Adapter {
        Adapter { if_type: 53, hardware: false, ..adapter(9, "Tailscale Tunnel", "100.101.102.103", &[]) }
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        c.source.fingerprints = vec![Fingerprint { gateway_mac: SOURCE_GW, server_lan_ip: SOURCE_SRV, ..Default::default() }];
        c.destination.fingerprints = vec![
            Fingerprint { gateway_mac: DESTINATION_WIFI_GW, server_lan_ip: DESTINATION_SRV, ..Default::default() },
            Fingerprint { gateway_mac: DESTINATION_CABLE_GW, server_lan_ip: DESTINATION_SRV, ..Default::default() },
        ];
        c
    }

    #[test]
    fn tailscale_detected_by_name_and_range() {
        assert!(is_tailscale(&adapter(1, "TAILSCALE tunnel", "198.51.100.5", &[])));
        assert!(is_tailscale(&adapter(1, "Wintun Userspace Tunnel", "100.64.0.1", &[])));
        assert!(is_tailscale(&adapter(1, "x", "100.127.255.254", &[])));
        assert!(is_tailscale(&adapter(1, "x", "fd7a:115c:a1e0::1234", &[])));
        assert!(!is_tailscale(&adapter(1, "Intel Wi-Fi", "100.63.255.255", &[])));
        assert!(!is_tailscale(&adapter(1, "Intel Wi-Fi", "100.128.0.0", &[])));
        assert!(!is_tailscale(&adapter(1, "Intel Wi-Fi", "fd7a:115c:a1e1::1", &[])));
        assert!(!is_tailscale(&adapter(1, "Ethernet Controller", "198.51.100.5", &[])));
    }

    #[test]
    fn source_when_gateway_route_and_probe_match() {
        let ads = [adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW]), tailscale()];
        let loc = resolve_location(&cfg(), &ads, &|ip| (ip == SOURCE_SRV).then_some(4), &|ip| ip == SOURCE_SRV, &no_vpn);
        assert_eq!(loc, Location::Source);
    }

    #[test]
    fn source_but_no_bulk_when_route_goes_via_unchecked_tailscale() {
        // v2 (task 2 brief): the location is recognised; the route only decides bulk.
        let ads = [adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW]), tailscale()];
        let l = locate(&cfg(), &ads, &|_| Some(9), &|_| true, &|_, _, _| PathVerdict::Unknown);
        assert_eq!(l.location, Location::Source);
        assert!(!l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn elsewhere_when_probe_fails() {
        let ads = [adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW]), tailscale()];
        let loc = resolve_location(&cfg(), &ads, &|_| Some(4), &|_| false, &no_vpn);
        assert_eq!(loc, Location::Elsewhere);
    }

    #[test]
    fn elsewhere_when_gateway_unknown() {
        let ads = [adapter(4, "Intel Wi-Fi", "172.16.0.9", &[[1, 2, 3, 4, 5, 6]]), tailscale()];
        let loc = resolve_location(&cfg(), &ads, &|_| Some(4), &|_| true, &no_vpn);
        assert_eq!(loc, Location::Elsewhere);
    }

    #[test]
    fn second_fingerprint_matches() {
        let ads = [adapter(7, "Ethernet Controller", "10.0.0.20", &[DESTINATION_CABLE_GW]), tailscale()];
        let loc = resolve_location(&cfg(), &ads, &|ip| (ip == DESTINATION_SRV).then_some(7), &|ip| ip == DESTINATION_SRV, &no_vpn);
        assert_eq!(loc, Location::Destination);
    }

    #[test]
    fn tailscale_adapter_with_fingerprint_mac_is_ignored() {
        let ts = adapter(9, "Tailscale Tunnel", "198.51.100.50", &[SOURCE_GW]);
        let loc = resolve_location(&cfg(), &[ts], &|_| Some(9), &|_| true, &no_vpn);
        assert_eq!(loc, Location::Elsewhere);
    }

    #[test]
    fn source_wins_when_both_match() {
        let ads = [adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW, DESTINATION_WIFI_GW])];
        let loc = resolve_location(&cfg(), &ads, &|_| Some(4), &|_| true, &no_vpn);
        assert_eq!(loc, Location::Source);
    }

    #[test]
    fn locate_returns_the_matched_fingerprint_ip() {
        let mut c = cfg();
        let other: IpAddr = "10.0.0.6".parse().unwrap();
        c.destination.fingerprints[0].server_lan_ip = other;
        let ads = [adapter(7, "Ethernet Controller", "10.0.0.20", &[DESTINATION_CABLE_GW]), tailscale()];
        let unknown = |_: IpAddr, _: &Adapter, _: &Adapter| PathVerdict::Unknown;
        let r = locate(&c, &ads, &|_| Some(7), &|_| true, &unknown);
        assert_eq!((r.location, r.server_ip), (Location::Destination, Some(DESTINATION_SRV)), "the cable fingerprint matched, not the Wi-Fi one");
        let r = locate(&c, &ads, &|_| Some(7), &|_| false, &unknown);
        assert_eq!(r, Located::ELSEWHERE);
    }

    #[test]
    fn down_adapter_never_matches() {
        let mut wifi = adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW]);
        wifi.up = false;
        let loc = resolve_location(&cfg(), &[wifi], &|_| Some(4), &|_| true, &no_vpn);
        assert_eq!(loc, Location::Elsewhere);
    }

    fn no_vpn(_: IpAddr, _: &Adapter, _: &Adapter) -> PathVerdict {
        PathVerdict::Unknown
    }

    // The owner's machine: Ethernet idx 2 and Tailscale idx 25; the office server office-nas.
    const OWNER_GW: [u8; 6] = [0x00, 0x00, 0x5e, 0x00, 0x53, 0x01];
    const SOURCE_NAS: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));

    fn ethernet() -> Adapter {
        Adapter {
            index: 2,
            alias: "Ethernet".into(),
            description: "Ethernet Controller".into(),
            if_type: 6,
            hardware: true,
            addrs: vec!["192.0.2.150".parse().unwrap()],
            prefixes: vec![("192.0.2.150".parse().unwrap(), 24)],
            gateway_macs: vec![OWNER_GW],
            up: true,
        }
    }

    fn owner_tailscale() -> Adapter {
        Adapter {
            index: 25,
            alias: "Tailscale".into(),
            description: "Tailscale Tunnel".into(),
            if_type: 53,
            hardware: false,
            addrs: vec!["100.64.0.10".parse().unwrap()],
            prefixes: vec![("100.64.0.10".parse().unwrap(), 32)],
            gateway_macs: vec![],
            up: true,
        }
    }

    /// A tunnel that only its name identifies (if_type 6).
    fn vpn(index: u32, alias: &str, desc: &str) -> Adapter {
        Adapter { index, alias: alias.into(), description: desc.into(), if_type: 6, ..owner_tailscale() }
    }

    fn owner_cfg(trust_vpn: bool, trusted_adapter: &str) -> Config {
        let mut c = Config::default();
        c.source.fingerprints = vec![Fingerprint {
            gateway_mac: OWNER_GW,
            server_lan_ip: SOURCE_NAS,
            name: "OfficeNet".into(),
            trust_vpn,
            trusted_adapter: trusted_adapter.into(),
        }];
        c
    }

    fn owner_locate(cfg: &Config, ads: &[Adapter], route: u32, verdict: PathVerdict) -> Located {
        locate(cfg, ads, &|_| Some(route), &|ip| ip == SOURCE_NAS, &|ip, v, l| {
            assert_eq!((ip, l.index), (SOURCE_NAS, 2), "checker gets the server IP and the matched local adapter");
            assert_eq!(v.index, route, "checker gets the adapter the route uses");
            verdict.clone()
        })
    }

    #[test]
    fn classify_known_adapters() {
        let kind = |alias: &str, desc: &str, if_type: u32, hardware: bool, gw: bool| {
            classify(&Adapter {
                alias: alias.into(),
                description: desc.into(),
                if_type,
                hardware,
                gateway_macs: if gw { vec![OWNER_GW] } else { vec![] },
                ..ethernet()
            })
        };
        assert_eq!(classify(&owner_tailscale()), AdapterKind::Vpn);
        assert_eq!(kind("wg0", "WireGuard Tunnel", 6, false, false), AdapterKind::Vpn);
        assert_eq!(kind("Ethernet 3", "TAP-Windows Adapter V9", 6, true, true), AdapterKind::Vpn);
        assert_eq!(kind("ZT", "ZeroTier One Virtual Port", 6, false, false), AdapterKind::Vpn);
        assert_eq!(kind("tunnel", "Some Driver", 131, true, true), AdapterKind::Vpn);
        assert_eq!(kind("OpenVPN Data Channel Offload", "x", 6, true, true), AdapterKind::Vpn);
        assert_eq!(kind("Local Area Connection", "Wintun Userspace Tunnel", 6, true, true), AdapterKind::Vpn);
        assert_eq!(kind("vEthernet (Default Switch)", "Hyper-V Virtual Ethernet Adapter", 6, false, true), AdapterKind::Local);
        assert_eq!(kind("VMware Network Adapter VMnet8", "VMware Virtual Ethernet Adapter for VMnet8", 6, false, false), AdapterKind::Vpn);
        assert_eq!(classify(&ethernet()), AdapterKind::Local);
    }

    #[test]
    fn owner_source_via_tailscale_direct() {
        let l = owner_locate(&owner_cfg(false, ""), &[ethernet(), owner_tailscale()], 25, PathVerdict::Direct);
        assert_eq!((l.location, l.server_ip), (Location::Source, Some(SOURCE_NAS)));
        assert_eq!(l.network_name.as_deref(), Some("OfficeNet"));
        assert_eq!(l.bulk, Some(BulkPath::ViaVpn { adapter: "Tailscale".into(), verdict: PathVerdict::Direct, trusted: false }));
        assert!(l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn vpn_not_direct_blocks_but_locates() {
        let l = owner_locate(&owner_cfg(false, ""), &[ethernet(), owner_tailscale()], 25, PathVerdict::NotDirect("relayed".into()));
        assert_eq!((l.location, l.server_ip), (Location::Source, Some(SOURCE_NAS)));
        assert!(!l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn unknown_vpn_blocked_unless_trusted() {
        let ads = [ethernet(), vpn(30, "WireGuard", "WireGuard Tunnel")];
        let untrusted = owner_locate(&owner_cfg(false, "WireGuard"), &ads, 30, PathVerdict::Unknown);
        assert_eq!(untrusted.location, Location::Source);
        assert!(!untrusted.bulk.unwrap().allows_bulk());
        let trusted = owner_locate(&owner_cfg(true, "WireGuard"), &ads, 30, PathVerdict::Unknown);
        assert_eq!(trusted.bulk, Some(BulkPath::ViaVpn { adapter: "WireGuard".into(), verdict: PathVerdict::Unknown, trusted: true }));
        assert!(trusted.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn trust_never_overrides_not_direct() {
        let l = owner_locate(&owner_cfg(true, "Tailscale"), &[ethernet(), owner_tailscale()], 25, PathVerdict::NotDirect("relayed".into()));
        let bulk = l.bulk.unwrap();
        assert!(matches!(bulk, BulkPath::ViaVpn { trusted: true, .. }));
        assert!(!bulk.allows_bulk());
    }

    #[test]
    fn trust_bound_to_adapter_alias() {
        let ads = [ethernet(), vpn(30, "WireGuard", "WireGuard Tunnel"), vpn(31, "ZeroTier", "ZeroTier One Virtual Port")];
        let l = owner_locate(&owner_cfg(true, "WireGuard"), &ads, 31, PathVerdict::Unknown);
        assert_eq!(l.location, Location::Source);
        assert!(matches!(l.bulk, Some(BulkPath::ViaVpn { trusted: false, .. })));
        assert!(!l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn direct_route_allows_without_checker() {
        let l = locate(&owner_cfg(false, ""), &[ethernet(), owner_tailscale()], &|_| Some(2), &|_| true, &|_, _, _| {
            panic!("no checker on a direct route")
        });
        assert_eq!(l.bulk, Some(BulkPath::Direct));
        assert!(l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn route_through_another_local_adapter_or_none_is_blocked() {
        let wifi = Adapter { index: 7, gateway_macs: vec![[9; 6]], ..ethernet() };
        let l = locate(&owner_cfg(false, ""), &[ethernet(), wifi], &|_| Some(7), &|_| true, &no_vpn);
        assert_eq!(l.location, Location::Source);
        assert!(matches!(l.bulk, Some(BulkPath::Blocked(_))));
        let l = locate(&owner_cfg(false, ""), &[ethernet()], &|_| None, &|_| true, &no_vpn);
        assert!(matches!(l.bulk, Some(BulkPath::Blocked(_))));
    }

    #[test]
    fn a_fingerprint_with_bulk_beats_an_earlier_blocked_one() {
        // At the destination on Wi-Fi and cable at once; Windows routes the server via the cable.
        let ads = [adapter(4, "Intel Wi-Fi", "10.0.0.21", &[DESTINATION_WIFI_GW]), adapter(7, "Ethernet Controller", "10.0.0.20", &[DESTINATION_CABLE_GW])];
        let l = locate(&cfg(), &ads, &|_| Some(7), &|_| true, &no_vpn);
        assert_eq!((l.location, l.bulk), (Location::Destination, Some(BulkPath::Direct)));
    }

    #[test]
    fn a_destination_match_with_bulk_beats_a_blocked_source() {
        // Both networks in reach; the source path is blocked, the destination one is direct.
        let ads = [adapter(4, "Intel Wi-Fi", "198.51.100.50", &[SOURCE_GW]), adapter(7, "Ethernet Controller", "10.0.0.20", &[DESTINATION_CABLE_GW])];
        let route = |ip: IpAddr| Some(if ip == DESTINATION_SRV { 7 } else { 9 });
        let l = locate(&cfg(), &[ads[0].clone(), ads[1].clone(), tailscale()], &route, &|_| true, &no_vpn);
        assert_eq!((l.location, l.bulk, l.route_if), (Location::Destination, Some(BulkPath::Direct), Some(7)), "the route that was checked");
        // Both blocked: the source's blocked match, not Elsewhere and not the destination.
        let l = locate(&cfg(), &[ads[0].clone(), ads[1].clone(), tailscale()], &|_| Some(9), &|_| true, &no_vpn);
        assert_eq!((l.location, l.server_ip), (Location::Source, Some(SOURCE_SRV)));
        assert!(!l.bulk.unwrap().allows_bulk());
        // Only the destination recognised and blocked: the destination with its block.
        let l = locate(&cfg(), &[ads[1].clone(), tailscale()], &|_| Some(9), &|_| true, &no_vpn);
        assert_eq!(l.location, Location::Destination);
        assert!(!l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn reason_text_names_adapter_and_ip() {
        let via = |verdict, trusted| BulkPath::ViaVpn { adapter: "WireGuard".into(), verdict, trusted };
        assert_eq!(
            reason_text(&via(PathVerdict::Unknown, false), SOURCE_NAS).as_deref(),
            Some("Windows sends traffic for 192.0.2.10 through WireGuard, and Ferry can't confirm that link stays on this network.")
        );
        let relayed = reason_text(&via(PathVerdict::NotDirect("relayed".into()), true), SOURCE_NAS).unwrap();
        assert!(relayed.contains("192.0.2.10") && relayed.contains("WireGuard") && relayed.contains("relayed"), "{relayed}");
        assert_eq!(reason_text(&BulkPath::Blocked("No route.".into()), SOURCE_NAS).as_deref(), Some("No route."));
        assert_eq!(reason_text(&BulkPath::Direct, SOURCE_NAS), None);
        assert_eq!(reason_text(&via(PathVerdict::Direct, false), SOURCE_NAS), None);
        assert_eq!(reason_text(&via(PathVerdict::Unknown, true), SOURCE_NAS), None);
    }

    #[test]
    fn pick_local_prefers_the_servers_subnet() {
        let wifi = Adapter { index: 7, prefixes: vec![("10.1.1.5".parse().unwrap(), 24)], ..ethernet() };
        let ads = [wifi, owner_tailscale(), ethernet()];
        assert_eq!(pick_local(&ads, SOURCE_NAS, &|_| Some(2)).ok().map(|a| a.index), Some(2));
        // Server outside every local subnet: the first local adapter with a gateway MAC.
        assert_eq!(pick_local(&ads, "172.16.0.1".parse().unwrap(), &|_| Some(7)).ok().map(|a| a.index), Some(7));
        // A VPN's prefix never counts, and a local adapter needs a gateway MAC.
        let gwless = Adapter { gateway_macs: vec![], ..ethernet() };
        assert_eq!(pick_local(&[owner_tailscale(), gwless], "100.64.0.10".parse().unwrap(), &|_| Some(2)).ok(), None);
    }

    #[test]
    fn pick_local_fallback_needs_the_route_through_that_adapter() {
        // Server outside every local subnet, but Windows reaches it some other way: not this network.
        let wifi = Adapter { index: 7, alias: "Wi-Fi".into(), prefixes: vec![("10.1.1.5".parse().unwrap(), 24)], ..ethernet() };
        let ads = [wifi, owner_tailscale()];
        let far: IpAddr = "172.16.0.1".parse().unwrap();
        assert_eq!(
            pick_local(&ads, far, &|_| Some(25)),
            Err("This server isn't on this network. Windows reaches it through Tailscale.".into())
        );
        assert_eq!(
            pick_local(&ads, far, &|_| Some(99)),
            Err("This server isn't on this network. Windows reaches it through another network.".into())
        );
        assert_eq!(
            pick_local(&ads, far, &|_| None),
            Err("This server isn't on this network. Windows reaches it through another network.".into())
        );
        assert_eq!(pick_local(&ads, far, &|_| Some(7)).ok().map(|a| a.index), Some(7));
    }

    #[test]
    fn pick_local_ignores_zero_length_prefixes() {
        // A /0 prefix holds every address; it mustn't count as the server's subnet.
        let wifi = Adapter { index: 7, prefixes: vec![("10.1.1.5".parse().unwrap(), 0)], gateway_macs: vec![], ..ethernet() };
        let ads = [wifi, ethernet()];
        assert_eq!(pick_local(&ads, "172.16.0.1".parse().unwrap(), &|_| Some(2)).ok().map(|a| a.index), Some(2));
    }

    #[test]
    fn pick_local_wont_fall_back_when_the_servers_subnet_has_no_router_yet() {
        // Ruling V4c: the server's own network is here but its gateway MAC isn't resolved.
        let gwless = Adapter { gateway_macs: vec![], ..ethernet() };
        let wifi = Adapter { index: 7, prefixes: vec![("10.1.1.5".parse().unwrap(), 24)], ..ethernet() };
        assert_eq!(
            pick_local(&[wifi, gwless], SOURCE_NAS, &|_| Some(7)),
            Err("Ferry can't see the router on the network that holds 192.0.2.10. Try again in a moment.".into())
        );
    }

    #[test]
    fn only_a_known_virtual_switch_counts_as_local_when_not_hardware() {
        // Ruling V4a: a gateway MAC alone doesn't make a virtual adapter local.
        let virt = |alias: &str, desc: &str| classify(&Adapter { alias: alias.into(), description: desc.into(), hardware: false, ..ethernet() });
        assert_eq!(virt("VMware Network Adapter VMnet8", "VMware Virtual Ethernet Adapter for VMnet8"), AdapterKind::Vpn);
        assert_eq!(virt("Ethernet 4", "Some Unknown Virtual Miniport"), AdapterKind::Vpn);
        assert_eq!(virt("vEthernet (External)", "Some Bridge"), AdapterKind::Local);
        assert_eq!(virt("Ethernet 5", "Hyper-V Virtual Ethernet Adapter #2"), AdapterKind::Local);
        // A virtual switch without the default gateway (WSL's) isn't the host's link.
        let wsl = Adapter { alias: "vEthernet (WSL)".into(), description: "Hyper-V Virtual Ethernet Adapter".into(), hardware: false, gateway_macs: vec![], ..ethernet() };
        assert_eq!(classify(&wsl), AdapterKind::Vpn);
    }

    #[test]
    fn trust_needs_a_named_adapter() {
        // Ruling V4b: an empty alias never matches an empty trusted_adapter.
        let ads = [ethernet(), vpn(30, "", "WireGuard Tunnel")];
        let l = owner_locate(&owner_cfg(true, ""), &ads, 30, PathVerdict::Unknown);
        assert!(matches!(l.bulk, Some(BulkPath::ViaVpn { trusted: false, .. })));
        assert!(!l.bulk.unwrap().allows_bulk());
    }

    #[test]
    fn the_vpn_checker_runs_once_per_server_and_adapter() {
        // Ruling V4d: two fingerprints (Wi-Fi and cable) for one server, both routed via Tailscale.
        let mut c = owner_cfg(false, "");
        let wifi_gw = [9; 6];
        c.source.fingerprints.push(Fingerprint { gateway_mac: wifi_gw, ..c.source.fingerprints[0].clone() });
        let wifi = Adapter { index: 7, gateway_macs: vec![wifi_gw], ..ethernet() };
        let calls = std::cell::Cell::new(0);
        let l = locate(&c, &[ethernet(), wifi, owner_tailscale()], &|_| Some(25), &|_| true, &|_, _, _| {
            calls.set(calls.get() + 1);
            PathVerdict::NotDirect("relayed".into())
        });
        assert_eq!(l.location, Location::Source);
        assert_eq!(calls.get(), 1);
    }
}
