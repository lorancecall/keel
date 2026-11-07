//! Current TCP/UDP endpoints per pid (`GetExtendedTcpTable` / `GetExtendedUdpTable`, no admin).
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_UDP6ROW_OWNER_PID,
    MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Endpoint {
    pub proto: &'static str,
    pub local: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'static str>,
}

pub fn tcp_state(s: u32) -> &'static str {
    match s {
        1 => "CLOSED", 2 => "LISTEN", 3 => "SYN_SENT", 4 => "SYN_RCVD", 5 => "ESTABLISHED", 6 => "FIN_WAIT1",
        7 => "FIN_WAIT2", 8 => "CLOSE_WAIT", 9 => "CLOSING", 10 => "LAST_ACK", 11 => "TIME_WAIT", 12 => "DELETE_TCB",
        _ => "?",
    }
}

/// Port fields hold the port in network byte order in their low 16 bits.
pub fn port(raw: u32) -> u16 { u16::from_be(raw as u16) }

fn v4(addr: u32, p: u32) -> String { format!("{}:{}", Ipv4Addr::from(addr.to_ne_bytes()), port(p)) }
fn v6(addr: [u8; 16], p: u32) -> String { format!("[{}]:{}", Ipv6Addr::from(addr), port(p)) }

/// Calls a table getter twice (size, then data) and returns the rows.
fn table<R: Copy>(get: impl Fn(Option<*mut core::ffi::c_void>, &mut u32) -> u32) -> Vec<R> {
    let mut size = 0u32;
    get(None, &mut size);
    for _ in 0..3 {
        let mut buf: Vec<u32> = vec![0; size as usize / 4 + 256];
        size = (buf.len() * 4) as u32;
        if get(Some(buf.as_mut_ptr().cast()), &mut size) == 0 {
            let n = buf[0] as usize;
            let rows = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(1) as *const R, n) };
            return rows.to_vec();
        }
    }
    vec![]
}

/// Pid -> endpoints, IPv4 and IPv6.
pub fn endpoints() -> HashMap<u32, Vec<Endpoint>> {
    let mut m: HashMap<u32, Vec<Endpoint>> = HashMap::new();
    let (f4, f6) = (AF_INET.0 as u32, AF_INET6.0 as u32);
    unsafe {
        for r in table::<MIB_TCPROW_OWNER_PID>(|b, s| GetExtendedTcpTable(b, s, false, f4, TCP_TABLE_OWNER_PID_ALL, 0)) {
            let listen = r.dwState == 2;
            m.entry(r.dwOwningPid).or_default().push(Endpoint {
                proto: "tcp", local: v4(r.dwLocalAddr, r.dwLocalPort),
                remote: (!listen).then(|| v4(r.dwRemoteAddr, r.dwRemotePort)), state: Some(tcp_state(r.dwState)),
            });
        }
        for r in table::<MIB_TCP6ROW_OWNER_PID>(|b, s| GetExtendedTcpTable(b, s, false, f6, TCP_TABLE_OWNER_PID_ALL, 0)) {
            let listen = r.dwState == 2;
            m.entry(r.dwOwningPid).or_default().push(Endpoint {
                proto: "tcp6", local: v6(r.ucLocalAddr, r.dwLocalPort),
                remote: (!listen).then(|| v6(r.ucRemoteAddr, r.dwRemotePort)), state: Some(tcp_state(r.dwState)),
            });
        }
        for r in table::<MIB_UDPROW_OWNER_PID>(|b, s| GetExtendedUdpTable(b, s, false, f4, UDP_TABLE_OWNER_PID, 0)) {
            m.entry(r.dwOwningPid).or_default().push(Endpoint { proto: "udp", local: v4(r.dwLocalAddr, r.dwLocalPort), remote: None, state: None });
        }
        for r in table::<MIB_UDP6ROW_OWNER_PID>(|b, s| GetExtendedUdpTable(b, s, false, f6, UDP_TABLE_OWNER_PID, 0)) {
            m.entry(r.dwOwningPid).or_default().push(Endpoint { proto: "udp6", local: v6(r.ucLocalAddr, r.dwLocalPort), remote: None, state: None });
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ports_and_addresses() {
        assert_eq!(port(0x5000), 80); // 80 = 0x0050, stored big-endian in the low word
        assert_eq!(port(0xBB01), 443);
        assert_eq!(v4(u32::from_ne_bytes([127, 0, 0, 1]), 0x5000), "127.0.0.1:80");
        assert_eq!(tcp_state(5), "ESTABLISHED");
    }
}
