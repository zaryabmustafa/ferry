use ferry_core::config::Fingerprint;
use ferry_core::location::{pick_local, Adapter};
use std::ffi::c_void;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ptr::null_mut;
use std::time::Duration;
use std::panic::{catch_unwind, AssertUnwindSafe};
use windows::Win32::Foundation::{
    ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, ERROR_SUCCESS, HANDLE, NO_ERROR, WIN32_ERROR,
};
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, ConvertInterfaceIndexToLuid, ConvertInterfaceLuidToGuid, FreeMibTable,
    GetAdaptersAddresses, GetBestInterfaceEx, GetIfEntry2, GetIpNetTable2, NotifyIpInterfaceChange,
    ResolveIpNetEntry2, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IPFORWARD_ROW2,
    MIB_IPINTERFACE_ROW, MIB_IPNET_ROW2, MIB_IPNET_TABLE2, MIB_NOTIFICATION_TYPE,
    MIB_UNICASTIPADDRESS_ROW, NotifyRouteChange2, NotifyUnicastIpAddressChange,
};
use windows::core::GUID;
use windows::Win32::NetworkManagement::Ndis::{IfOperStatusUp, NET_LUID_LH};
use windows::Win32::Networking::NetworkListManager::{INetworkCostManager, INetworkListManager, NetworkListManager};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_INET,
};

/// All adapters with their unicast addresses and the MACs of their IPv4 gateways.
pub fn adapters() -> io::Result<Vec<Adapter>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut attempts = 0;
    let buf = loop {
        // u64 elements keep the buffer 8-byte aligned for IP_ADAPTER_ADDRESSES_LH.
        let mut buf = vec![0u64; size.div_ceil(8) as usize];
        let rc = unsafe {
            GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(buf.as_mut_ptr().cast()), &mut size)
        };
        attempts += 1;
        match rc {
            0 => break buf,
            rc if rc == ERROR_BUFFER_OVERFLOW.0 && attempts < 3 => continue,
            rc if rc == ERROR_NO_DATA.0 => return Ok(Vec::new()),
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
        }
    };

    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: GetAdaptersAddresses filled `buf` with a linked list of adapters whose
    // pointers all point into `buf`, which outlives this loop.
    unsafe {
        while let Some(a) = p.as_ref() {
            let if4 = a.Anonymous1.Anonymous.IfIndex;
            let index = if if4 != 0 { if4 } else { a.Ipv6IfIndex };
            let mut prefixes = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while let Some(ua) = u.as_ref() {
                prefixes.extend(sockaddr_ip(ua.Address.lpSockaddr).map(|ip| (ip, ua.OnLinkPrefixLength)));
                u = ua.Next;
            }
            let mut gateway_macs = Vec::new();
            let mut g = a.FirstGatewayAddress;
            while let Some(ga) = g.as_ref() {
                if let Some(IpAddr::V4(gw)) = sockaddr_ip(ga.Address.lpSockaddr) {
                    gateway_macs.extend(gateway_mac(if4, gw));
                }
                g = ga.Next;
            }
            out.push(Adapter {
                index,
                alias: a.FriendlyName.to_string().unwrap_or_default(),
                description: a.Description.to_string().unwrap_or_default(),
                if_type: a.IfType,
                hardware: is_hardware(index),
                addrs: prefixes.iter().map(|&(ip, _)| ip).collect(),
                prefixes,
                gateway_macs,
                up: a.OperStatus == IfOperStatusUp,
            });
            p = a.Next;
        }
    }
    Ok(out)
}

/// Interface index Windows would route `ip` through.
pub fn best_interface(ip: IpAddr) -> Option<u32> {
    let sa = SOCKADDR_INET::from(SocketAddr::new(ip, 0));
    let mut idx = 0u32;
    let rc = unsafe { GetBestInterfaceEx(&sa as *const SOCKADDR_INET as *const SOCKADDR, &mut idx) };
    (rc == 0).then_some(idx)
}

pub fn probe_445(ip: IpAddr) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip, 445), Duration::from_millis(1500)).is_ok()
}

/// "Remember this network": the gateway MAC of the local adapter on the server's subnet, else
/// of the first local adapter with a known gateway when Windows routes the server through it
/// (`pick_local`); the path check decides bulk separately.
pub fn capture(server_lan_ip: IpAddr) -> io::Result<Fingerprint> {
    let ads = adapters()?;
    let a = pick_local(&ads, server_lan_ip, &best_interface).map_err(io::Error::other)?;
    Ok(Fingerprint {
        gateway_mac: a.gateway_macs[0],
        server_lan_ip,
        name: network_name(a.index).unwrap_or_default(),
        ..Default::default()
    })
}

/// Windows' name for the network on interface `if_index` ("OfficeNet"), from the Network List
/// Manager. COM is initialized on the calling thread for this call only.
pub fn network_name(if_index: u32) -> Option<String> {
    let mut luid = NET_LUID_LH::default();
    let mut guid = GUID::zeroed();
    // SAFETY: plain out-parameters. The interface GUID is the adapter id NLM reports.
    let ok = unsafe {
        ConvertInterfaceIndexToLuid(if_index, &mut luid) == NO_ERROR
            && ConvertInterfaceLuidToGuid(&luid, &mut guid) == NO_ERROR
    };
    if !ok {
        return None;
    }
    // RPC_E_CHANGED_MODE (COM already up in another mode) is fine: NLM works in either.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let name = nlm_name(guid); // releases every COM object before we uninitialize
    if init.is_ok() {
        unsafe { CoUninitialize() };
    }
    name
}

fn nlm_name(adapter: GUID) -> Option<String> {
    // SAFETY: COM calls on interfaces owned by this function.
    unsafe {
        let nlm: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok()?;
        let conns = nlm.GetNetworkConnections().ok()?;
        loop {
            let mut c = [None];
            let mut fetched = 0;
            conns.Next(&mut c, Some(&mut fetched)).ok()?;
            let c = c[0].take().filter(|_| fetched == 1)?; // S_FALSE with nothing: the end
            if c.GetAdapterId().is_ok_and(|id| id == adapter) {
                return c.GetNetwork().and_then(|n| n.GetName()).ok().map(|n| n.to_string());
            }
        }
    }
}

/// True when Windows says the internet connection costs money per byte (a metered Wi-Fi, a phone
/// hotspot, roaming). Unknown or failed reads count as unmetered.
pub fn metered() -> bool {
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: the interface is released before COM is uninitialized.
    let cost = unsafe {
        CoCreateInstance::<_, INetworkCostManager>(&NetworkListManager, None, CLSCTX_ALL).and_then(|m| {
            let mut c = 0u32;
            m.GetCost(&mut c, std::ptr::null()).map(|()| c)
        })
    };
    if init.is_ok() {
        unsafe { CoUninitialize() };
    }
    cost.is_ok_and(is_metered_cost)
}

/// NLM_CONNECTION_COST: anything set beyond UNRESTRICTED (0x1) means pay-per-use or limited;
/// UNKNOWN (0) does not.
fn is_metered_cost(cost: u32) -> bool {
    cost & !0x1 != 0
}

/// Windows' HardwareInterface flag (bit 0 of InterfaceAndOperStatusFlags).
fn is_hardware(if_index: u32) -> bool {
    let mut row = MIB_IF_ROW2 { InterfaceIndex: if_index, ..Default::default() };
    // SAFETY: `row` is a valid MIB_IF_ROW2 with its index set.
    let rc = unsafe { GetIfEntry2(&mut row) };
    rc == NO_ERROR && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0
}

type Callback = Box<dyn Fn() + Send + Sync>;

/// Live IP change subscription (interfaces, routes, unicast addresses); cancels
/// on drop. Do not drop it from inside the callback: cancelling waits for
/// running callbacks.
pub struct ChangeWatch {
    handles: Vec<HANDLE>,
    ctx: *mut Callback,
}

// SAFETY: the handles are opaque notification handles valid on any thread, and
// `ctx` points to a `Send + Sync` closure that only this struct frees.
unsafe impl Send for ChangeWatch {}

/// Calls `on_change` (on a system thread) whenever an IP interface, route or
/// unicast address changes. Route changes matter on their own: toggling
/// Tailscale's subnet routes moves the server's LAN IP without any interface
/// change; so do addresses (a DHCP lease arriving after link-up).
pub fn watch(on_change: Callback) -> io::Result<ChangeWatch> {
    // Double box: `Box<dyn Fn>` is a fat pointer; the context must be a thin one.
    let ctx = Box::into_raw(Box::new(on_change));
    // Dropping a partial watch on error cancels whatever registered and frees `ctx`.
    let mut w = ChangeWatch { handles: Vec::new(), ctx };
    let c = ctx as *const c_void;
    // SAFETY: `ctx` stays alive until `ChangeWatch::drop` has cancelled every
    // notification, which also waits for in-flight callbacks.
    let regs: [&dyn Fn(&mut HANDLE) -> WIN32_ERROR; 3] = [
        &|h| unsafe { NotifyIpInterfaceChange(AF_UNSPEC, Some(on_ip_change::<MIB_IPINTERFACE_ROW>), Some(c), false, h) },
        &|h| unsafe { NotifyRouteChange2(AF_UNSPEC, Some(on_ip_change::<MIB_IPFORWARD_ROW2>), c, false, h) },
        &|h| unsafe { NotifyUnicastIpAddressChange(AF_UNSPEC, Some(on_ip_change::<MIB_UNICASTIPADDRESS_ROW>), Some(c), false, h) },
    ];
    for reg in regs {
        let mut h = HANDLE::default();
        let rc = reg(&mut h);
        if rc != NO_ERROR {
            return Err(io::Error::from_raw_os_error(rc.0 as i32));
        }
        w.handles.push(h);
    }
    Ok(w)
}

impl Drop for ChangeWatch {
    fn drop(&mut self) {
        // SAFETY: cancelling blocks until running callbacks return; once every
        // registration is cancelled nothing else can reach `ctx`.
        // Count (not `all`) so every handle is cancelled even after a failure.
        let failed = self
            .handles
            .iter()
            .filter(|&&h| unsafe { CancelMibChangeNotify2(h) } != NO_ERROR)
            .count();
        if failed == 0 {
            drop(unsafe { Box::from_raw(self.ctx) });
        }
        // Otherwise leak it: a still-registered callback may yet read `ctx`.
    }
}

unsafe extern "system" fn on_ip_change<Row>(
    ctx: *const c_void,
    _row: *const Row,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: `ctx` is the `*mut Callback` leaked by `watch`, alive until cancelled.
    let f = unsafe { &*(ctx as *const Callback) };
    // A panic must not unwind across FFI (that aborts the process).
    let _ = catch_unwind(AssertUnwindSafe(f));
}

/// MAC of IPv4 gateway `gw` on interface `if_index`: resolve (ARP) first, else
/// look it up in the neighbor table. All-zero MACs count as unknown.
fn gateway_mac(if_index: u32, gw: Ipv4Addr) -> Option<[u8; 6]> {
    let mut row = MIB_IPNET_ROW2 { InterfaceIndex: if_index, ..Default::default() };
    row.Address = SOCKADDR_INET::from(SocketAddr::new(gw.into(), 0));
    if unsafe { ResolveIpNetEntry2(&mut row, None) } == ERROR_SUCCESS {
        if let Some(mac) = row_mac(&row) {
            return Some(mac);
        }
    }
    let mut table: *mut MIB_IPNET_TABLE2 = null_mut();
    if unsafe { GetIpNetTable2(AF_INET, &mut table) } != ERROR_SUCCESS {
        return None;
    }
    // SAFETY: on success `table` holds `NumEntries` rows until FreeMibTable.
    unsafe {
        let rows = std::slice::from_raw_parts(
            std::ptr::addr_of!((*table).Table).cast::<MIB_IPNET_ROW2>(),
            (*table).NumEntries as usize,
        );
        let mac = rows
            .iter()
            .filter(|r| r.InterfaceIndex == if_index)
            .filter(|r| sockaddr_ip(&r.Address as *const SOCKADDR_INET as *const SOCKADDR) == Some(IpAddr::V4(gw)))
            .find_map(row_mac);
        FreeMibTable(table as *const c_void);
        mac
    }
}

fn row_mac(row: &MIB_IPNET_ROW2) -> Option<[u8; 6]> {
    let mac: [u8; 6] = row.PhysicalAddress[..6].try_into().unwrap();
    (row.PhysicalAddressLength == 6 && mac != [0; 6]).then_some(mac)
}

/// SAFETY: `sa` must be null or point to a valid SOCKADDR_IN / SOCKADDR_IN6.
unsafe fn sockaddr_ip(sa: *const SOCKADDR) -> Option<IpAddr> {
    let fam = unsafe { sa.as_ref()?.sa_family };
    unsafe {
        if fam == AF_INET {
            let s = &*(sa as *const SOCKADDR_IN);
            Some(Ipv4Addr::from(s.sin_addr).into())
        } else if fam == AF_INET6 {
            let s = &*(sa as *const SOCKADDR_IN6);
            Some(Ipv6Addr::from(s.sin6_addr).into())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metered_cost_flags() {
        assert!(!is_metered_cost(0x1), "unrestricted");
        assert!(!is_metered_cost(0x0), "unknown: no reason to hold back");
        assert!(is_metered_cost(0x2), "fixed");
        assert!(is_metered_cost(0x4), "variable");
        assert!(is_metered_cost(0x2 | 0x10000), "fixed, over the data limit");
        assert!(is_metered_cost(0x1 | 0x20000), "roaming");
    }
}
