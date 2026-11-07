//! Reading and changing the real network state: adapters (GetAdaptersAddresses), IPv4 interface
//! metrics (Get/SetIpInterfaceEntry), routes in the active store (GetIpForwardTable2,
//! Create/DeleteIpForwardEntry2) and the WCM policy that keeps Wi-Fi up beside Ethernet.
use crate::model::{Action, Adapter, Route, Snapshot};
use anyhow::{Result, anyhow, bail};
use std::net::Ipv4Addr;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, MIB_IPPROTO_NETMGMT, SOCKADDR_INET};

pub const POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy";
pub const POLICY_NAME: &str = "fMinimizeConnections";

fn wide(p: windows::core::PWSTR) -> String { if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } } }

fn v4(a: &SOCKADDR_INET) -> Ipv4Addr { unsafe { Ipv4Addr::from(u32::from_be(a.Ipv4.sin_addr.S_un.S_addr)) } }

fn sockaddr(ip: Ipv4Addr) -> SOCKADDR_INET {
    let mut s = SOCKADDR_INET::default();
    s.Ipv4.sin_family = AF_INET;
    s.Ipv4.sin_addr.S_un.S_addr = u32::from(ip).to_be();
    s
}

/// IPv4 interface metric of an adapter: (metric, automatic).
fn ip_interface(index: u32) -> Option<MIB_IPINTERFACE_ROW> {
    let mut row = MIB_IPINTERFACE_ROW::default();
    unsafe { InitializeIpInterfaceEntry(&mut row) };
    row.Family = AF_INET;
    row.InterfaceIndex = index;
    unsafe { GetIpInterfaceEntry(&mut row) }.is_ok().then_some(row)
}

pub fn adapters() -> Result<Vec<Adapter>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 32 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0u64; size as usize / 8 + 1];
        let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH), &mut size) };
        if r == 0 { break; }
        if r != 111 /* ERROR_BUFFER_OVERFLOW */ { bail!("GetAdaptersAddresses: {r}"); }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !p.is_null() {
        let a = unsafe { &*p };
        let index = unsafe { a.Anonymous1.Anonymous.IfIndex };
        if let Some(row) = ip_interface(index) {
            out.push(Adapter {
                index,
                alias: wide(a.FriendlyName),
                description: wide(a.Description),
                up: a.OperStatus == IfOperStatusUp,
                metric: row.Metric,
                auto_metric: row.UseAutomaticMetric,
                kind: crate::model::kind_of(a.IfType, &wide(a.Description)),
            });
        }
        p = a.Next;
    }
    Ok(out)
}

pub fn routes() -> Result<Vec<Route>> {
    let mut t: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    unsafe { GetIpForwardTable2(AF_INET, &mut t) }.ok().map_err(|e| anyhow!("GetIpForwardTable2: {e}"))?;
    let rows = unsafe { std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize) };
    let out = rows.iter().map(|r| Route {
        prefix: v4(&r.DestinationPrefix.Prefix), len: r.DestinationPrefix.PrefixLength,
        index: r.InterfaceIndex, next_hop: v4(&r.NextHop), metric: r.Metric,
    }).collect();
    unsafe { FreeMibTable(t as *const _) };
    Ok(out)
}

/// (value, key exists)
pub fn policy() -> (Option<u32>, bool) {
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
    match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(POLICY_KEY) {
        Ok(k) => (k.get_value::<u32, _>(POLICY_NAME).ok(), true),
        Err(_) => (None, false),
    }
}

pub fn snapshot() -> Result<Snapshot> {
    Ok(Snapshot { adapters: adapters()?, routes: routes()?, policy: policy().0, rules: crate::fw::rules() })
}

fn route_row(r: &Route) -> MIB_IPFORWARD_ROW2 {
    let mut row = MIB_IPFORWARD_ROW2::default();
    unsafe { InitializeIpForwardEntry(&mut row) };
    row.InterfaceIndex = r.index;
    row.DestinationPrefix.Prefix = sockaddr(r.prefix);
    row.DestinationPrefix.PrefixLength = r.len;
    row.NextHop = sockaddr(r.next_hop);
    row.Metric = r.metric;
    row.Protocol = MIB_IPPROTO_NETMGMT;
    row
}

fn set_metric(index: u32, auto: bool, metric: u32) -> Result<()> {
    let mut row = ip_interface(index).ok_or_else(|| anyhow!("interface {index} not found"))?;
    row.SitePrefixLength = 0; // must be 0 for IPv4 when setting
    row.UseAutomaticMetric = auto;
    if !auto { row.Metric = metric; }
    unsafe { SetIpInterfaceEntry(&mut row) }.ok().map_err(|e| anyhow!("SetIpInterfaceEntry: {e}"))
}

/// Runs one action (admin).
pub fn apply(a: &Action) -> Result<()> {
    match a {
        Action::SetPolicy => {
            use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
            let (k, _) = RegKey::predef(HKEY_LOCAL_MACHINE).create_subkey(POLICY_KEY)?;
            k.set_value(POLICY_NAME, &0u32)?;
            Ok(())
        }
        Action::SetMetric { index, metric, .. } => set_metric(*index, false, *metric),
        Action::RestoreMetric { index, auto, metric, .. } => set_metric(*index, *auto, *metric),
        Action::AddRoute(r) => unsafe { CreateIpForwardEntry2(&route_row(r)) }.ok().map_err(|e| anyhow!("CreateIpForwardEntry2: {e}")),
        Action::RemoveRoute(r) => unsafe { DeleteIpForwardEntry2(&route_row(r)) }.ok().map_err(|e| anyhow!("DeleteIpForwardEntry2: {e}")),
        Action::BlockRule { name, program, aliases, outbound } => crate::fw::block(name, program, aliases, *outbound),
        Action::DisableRule { name } => crate::fw::disable(name),
    }
}

/// Puts the policy back as recorded (uninstall / `keel net undo`).
pub fn restore_policy(existed: bool, old: Option<u32>) -> Result<()> {
    use winreg::{RegKey, enums::{HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS}};
    let hk = RegKey::predef(HKEY_LOCAL_MACHINE);
    match old {
        Some(v) => { hk.create_subkey(POLICY_KEY)?.0.set_value(POLICY_NAME, &v)?; }
        None => {
            if let Ok(k) = hk.open_subkey_with_flags(POLICY_KEY, KEY_ALL_ACCESS) {
                let _ = k.delete_value(POLICY_NAME);
                let info = k.query_info()?;
                if !existed && info.values == 0 && info.sub_keys == 0 { drop(k); let _ = hk.delete_subkey(POLICY_KEY); }
            }
        }
    }
    Ok(())
}
