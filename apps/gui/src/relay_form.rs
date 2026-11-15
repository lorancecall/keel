//! Pure helpers for the optional LAN relay editor; no service or firewall calls.
use std::net::Ipv4Addr;

pub fn choices(wired: &[String], saved: &str) -> (Vec<String>, Vec<String>, usize) {
    let mut labels = vec!["Select Ethernet adapter".to_string()];
    let mut aliases = vec![String::new()];
    for alias in wired {
        if !aliases.iter().any(|a| a.eq_ignore_ascii_case(alias)) { labels.push(alias.clone()); aliases.push(alias.clone()); }
    }
    let at = aliases.iter().position(|a| a.eq_ignore_ascii_case(saved)).unwrap_or_else(|| {
        labels.push(format!("{saved} (not connected)")); aliases.push(saved.to_string()); aliases.len() - 1
    });
    (labels, aliases, at)
}

pub fn add_host(hosts: &mut Vec<String>, text: &str) -> Result<(), String> {
    let ip: Ipv4Addr = text.trim().parse().map_err(|_| "Enter one exact private IPv4 address, not a subnet or hostname".to_string())?;
    if !ip.is_private() { return Err("Only private IPv4 addresses (10.x, 172.16-31.x, 192.168.x) are allowed".into()); }
    let host = ip.to_string();
    if hosts.contains(&host) { return Err("That IP is already in the list".into()); }
    hosts.push(host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn defaults_and_missing_adapter_are_preserved() {
        let (labels, values, at) = choices(&["Ethernet".into()], "");
        assert_eq!(at, 0); assert!(values[at].is_empty()); assert_eq!(labels.len(), 2);
        let (labels, values, at) = choices(&["Ethernet".into()], "USB Ethernet");
        assert_eq!(values[at], "USB Ethernet"); assert!(labels[at].contains("not connected"));
        let (_, values, at) = choices(&["Ethernet".into()], "ethernet"); assert_eq!(values[at], "Ethernet");
    }
    #[test] fn exact_private_hosts_only_and_no_duplicates() {
        let mut hosts = vec![];
        add_host(&mut hosts, " 192.168.50.5 ").unwrap();
        assert!(add_host(&mut hosts, "192.168.50.5").is_err());
        for bad in ["8.8.8.8", "127.0.0.1", "169.254.1.1", "192.168.0.0/16", "::1", "example.com", "*"] {
            assert!(add_host(&mut hosts, bad).is_err(), "{bad}");
        }
        assert_eq!(hosts, ["192.168.50.5"]);
        hosts.remove(0); assert!(hosts.is_empty());
    }
}
