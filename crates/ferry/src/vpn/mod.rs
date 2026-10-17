//! VPN path checkers (spec 4.3): is traffic Windows sends into a VPN adapter for the server's
//! LAN IP actually staying on this network? Each checker answers only for its own VPN's adapter.

pub mod tailscale;

use ferry_core::location::{is_tailscale, Adapter, PathVerdict};
use std::net::IpAddr;
use std::time::Instant;

/// What a VPN CLI printed on stdout; `timed_out` when it was killed at the deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOut {
    pub text: String,
    pub timed_out: bool,
}

/// Runs the VPN's CLI with `args` until the deadline; `None` when it can't start.
pub type Run<'a> = &'a dyn Fn(&[&str], Instant) -> Option<CliOut>;

/// `check_vpn` for `locate`: `vpn` is the adapter Windows routes `server_ip` through, `local`
/// the matched local adapter. Blocks for up to 5 s, so call it off the tray thread.
pub fn check(server_ip: IpAddr, vpn: &Adapter, local: &Adapter) -> PathVerdict {
    check_with(server_ip, vpn, local, &tailscale::run_cli)
}

fn check_with(server_ip: IpAddr, vpn: &Adapter, local: &Adapter, run: Run) -> PathVerdict {
    if is_tailscale(vpn) {
        tailscale::check(server_ip, vpn, local, run)
    } else {
        PathVerdict::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(alias: &str, desc: &str, addr: &str) -> Adapter {
        let ip: IpAddr = addr.parse().unwrap();
        Adapter {
            index: 30,
            alias: alias.into(),
            description: desc.into(),
            if_type: 53,
            hardware: false,
            addrs: vec![ip],
            prefixes: vec![(ip, 24)],
            gateway_macs: vec![],
            up: true,
        }
    }

    const STATUS: &str = include_str!("fixtures/tailscale_status.json");
    const OFFICE_DIRECT: &str = "pong from office-nas (100.64.0.2) via 192.0.2.10:41641 in 2ms\n";

    fn local() -> Adapter {
        Adapter { if_type: 6, hardware: true, ..adapter("Ethernet", "Ethernet Controller", "192.0.2.150") }
    }

    /// Tailscale's CLI as it looks at the office: office-nas direct on the LAN.
    fn office_cli(args: &[&str], _: Instant) -> Option<CliOut> {
        let text = if args[0] == "status" { STATUS } else { OFFICE_DIRECT };
        Some(CliOut { text: text.into(), timed_out: false })
    }

    #[test]
    fn non_tailscale_adapter_is_unknown() {
        let wg = adapter("WireGuard", "WireGuard Tunnel", "10.8.0.2");
        let never = |_: &[&str], _: Instant| -> Option<CliOut> { panic!("the CLI must not run for another VPN") };
        assert_eq!(check_with("192.0.2.10".parse().unwrap(), &wg, &local(), &never), PathVerdict::Unknown);
    }

    #[test]
    fn tailscale_adapter_gets_the_tailscale_verdict() {
        let ts = adapter("Tailscale", "Tailscale Tunnel", "100.64.0.1");
        assert_eq!(check_with("192.0.2.10".parse().unwrap(), &ts, &local(), &office_cli), PathVerdict::Direct);
        assert_eq!(check_with("192.0.2.10".parse().unwrap(), &ts, &local(), &|_, _| None), PathVerdict::Unknown, "no CLI");
    }

    #[test]
    fn another_vpn_in_the_cgnat_range_is_unknown_even_when_tailscale_is_direct() {
        // NetBird (or WARP) also uses 100.64.0.0/10; it isn't the adapter Tailscale reports as itself.
        let netbird = adapter("wt0", "WireGuard Tunnel", "100.80.0.2");
        assert_eq!(check_with("192.0.2.10".parse().unwrap(), &netbird, &local(), &office_cli), PathVerdict::Unknown);
    }
}
