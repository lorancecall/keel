//! Network routing, pure part: the config, a snapshot of adapters / routes / rules, and the plan
//! that brings the snapshot to what the config asks for:
//!
//! - Wi-Fi stays connected while the wired LAN is up (policy `fMinimizeConnections = 0`).
//! - VPNs like Tailscale and WireGuard bind their outer sockets to the interface with the best
//!   `0.0.0.0/0` route (route + interface metric). The side those VPNs should use gets the lowest
//!   interface metric, so they follow it ("follow side").
//! - General internet goes to the side the user picks: when that is not the follow side, its
//!   adapters get `0.0.0.0/1` + `128.0.0.0/1` (more specific than `/0`, so they win).
//! - Destination rules and VPN servers get their own routes via a side's gateway.
//! - Kill switch per VPN: firewall rules block its program on every adapter except its side and its
//!   own tunnel, so it goes offline instead of falling back to the other side.
//! - With `tailscale_wifi_only`, external Tailscale transport stays on Wi-Fi with the kill switch.
//!   Exact private LAN relay hosts are opt-in exceptions on one named Ethernet adapter.
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side { Wifi, Lan }

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str(match self { Side::Wifi => "Wi-Fi", Side::Lan => "LAN" }) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VpnKind {
    /// Binds to the interface with the best `0.0.0.0/0` route (Tailscale, WireGuard, NordLynx).
    DefaultRoute,
    /// Routes its own servers (NordVPN, OpenVPN): the listed `servers`, plus the host routes it
    /// adds itself while its tunnel is up, get a route via `via`.
    Server,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vpn {
    pub name: String,
    pub kind: VpnKind,
    /// The program whose traffic the kill switch blocks off `via` (first of `all_programs()`).
    pub exe: String,
    /// More programs for the kill switch; `*` matches one folder level (a version folder).
    pub programs: Vec<String>,
    /// Its tunnel adapters, by description (`*` wildcards, comma-separated); never blocked.
    pub adapter: String,
    pub via: Side,
    pub kill_switch: bool,
    /// `Server` VPNs: server addresses (`a.b.c.d` or `a.b.c.d/n`).
    pub servers: Vec<String>,
    /// Remote ports the kill switch blocks for every program (`udp 51820`): tunnels in the kernel
    /// (NordLynx, WireGuard) send from Windows itself, not from a program.
    pub ports: Vec<String>,
    /// Exact physical Ethernet alias for opt-in private IPv4 relay exceptions.
    pub relay_interface: String,
    /// Exact RFC1918 hosts; empty preserves full isolation.
    pub relay_hosts: Vec<String>,
}

impl Default for Vpn {
    fn default() -> Self {
        Self { name: String::new(), kind: VpnKind::DefaultRoute, exe: String::new(), programs: Vec::new(), adapter: String::new(),
               via: Side::Wifi, kill_switch: false, servers: Vec::new(), ports: Vec::new(), relay_interface: String::new(), relay_hosts: Vec::new() }
    }
}

impl Vpn {
    /// `exe` and `programs`, without blanks or repeats.
    pub fn all_programs(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for p in std::iter::once(&self.exe).chain(&self.programs) {
            let p = p.trim().trim_matches('"').to_string();
            if !p.is_empty() && !v.iter().any(|x| x.eq_ignore_ascii_case(&p)) { v.push(p); }
        }
        v
    }
    /// Sets `exe` and `programs` from one list.
    pub fn set_programs(&mut self, list: Vec<String>) {
        let mut it = list.into_iter();
        self.exe = it.next().unwrap_or_default();
        self.programs = it.collect();
    }
    pub fn adapter_patterns(&self) -> Vec<String> {
        self.adapter.split([',', ';']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }
    /// Whether `a` is one of its tunnel adapters.
    pub fn is_tunnel(&self, a: &Adapter) -> bool { self.adapter_patterns().iter().any(|p| glob(p, &a.description)) }

    /// Ready-made entries for the Network page.
    pub fn presets() -> Vec<Vpn> {
        vec![
            Vpn { name: "Tailscale".into(), exe: TAILSCALED.into(), adapter: "Tailscale*".into(), via: Side::Wifi, kill_switch: true, ..Default::default() },
            Vpn { name: "NordVPN".into(), kind: VpnKind::Server, exe: r"C:\Program Files\NordVPN\nordvpn-service.exe".into(),
                  programs: vec![r"C:\Program Files\NordVPN\*\nordvpn-service-app.exe".into(),
                                 r"C:\Program Files\NordVPN\*\Resources\Binaries\64bit\openvpn-nordvpn.exe".into()],
                  adapter: "NordLynx*, OpenVPN Data Channel Offload*".into(), via: Side::Wifi, kill_switch: true,
                  ports: vec!["udp 51820".into()], ..Default::default() },
            Vpn { name: "WireGuard".into(), exe: r"C:\Program Files\WireGuard\wireguard.exe".into(), adapter: "WireGuard Tunnel*".into(),
                  via: Side::Wifi, kill_switch: true, ports: vec!["udp 51820".into()], ..Default::default() },
        ]
    }
}

/// `udp 51820`, `tcp 1194,1195`, `udp:51820` -> (protocol number, ports). Pure; tested.
pub fn parse_ports(s: &str) -> Option<(u8, String)> {
    let s = s.trim().to_lowercase();
    let (proto, rest) = s.split_once([' ', ':', '/'])?;
    let proto = match proto.trim() { "udp" => 17, "tcp" => 6, _ => return None };
    let ports: Vec<u16> = rest.split([',', ' ']).filter(|p| !p.is_empty()).map(|p| p.parse().ok()).collect::<Option<_>>()?;
    if ports.is_empty() || ports.contains(&0) { return None; }
    Some((proto, ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",")))
}

/// Ports nobody should block for every program on a side (web, DNS, DHCP, time).
const COMMON_PORTS: [&str; 6] = ["53", "67", "68", "80", "123", "443"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRule {
    /// `a.b.c.d/n`
    pub prefix: String,
    pub via: Side,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Off: Keel's routes removed, kill switches disabled, metrics restored (the policy stays).
    pub enabled: bool,
    pub keep_wifi_with_lan: bool,
    /// Where general internet traffic goes while both sides are up.
    pub internet: Side,
    /// The Wi-Fi adapter's name; empty (or no adapter of that name) = every Wi-Fi adapter.
    pub wifi_alias: String,
    /// Wired adapters, by description (`*` wildcards); empty = every physical wired adapter.
    pub lan_descriptions: Vec<String>,
    /// Interface metric of the follow side; 1-4, below the 5 Windows gives a gigabit LAN.
    pub top_metric: u32,
    pub routes: Vec<RouteRule>,
    pub vpns: Vec<Vpn>,
    /// Keep Tailscale on Wi-Fi with its kill switch on, whatever its `via` says. Off for a new
    /// config; a `net.toml` without the field keeps it on, as files written before it existed expect.
    #[serde(default = "yes")]
    pub tailscale_wifi_only: bool,
}

fn yes() -> bool { true }

pub const TAILSCALED: &str = r"C:\Program Files\Tailscale\tailscaled.exe";

impl Default for Config {
    /// A new install changes nothing: off and no VPNs until the user sets them up on the Network
    /// page (a `net.toml` written earlier keeps its values).
    fn default() -> Self {
        Self { enabled: false, vpns: Vec::new(), wifi_alias: String::new(), lan_descriptions: Vec::new(), ..Self::example() }
    }
}

impl Config {
    /// The setup the Network page starts from when switched on: general traffic over the LAN,
    /// Tailscale on Wi-Fi with its kill switch (also the tests' configuration).
    pub fn example() -> Self {
        Self {
            enabled: true,
            keep_wifi_with_lan: true,
            internet: Side::Lan,
            wifi_alias: "Wi-Fi".into(),
            lan_descriptions: vec!["Realtek USB GbE*".into()],
            top_metric: 1,
            routes: Vec::new(),
            vpns: vec![Vpn { name: "Tailscale".into(), exe: TAILSCALED.into(), adapter: "Tailscale*".into(),
                             via: Side::Wifi, kill_switch: true, ..Default::default() }],
            tailscale_wifi_only: false,
        }
    }
}

/// The Network page's LAN choices: labels, the `lan_descriptions` each one saves, and the current
/// one. "Any wired adapter" first, then the attached wired adapters, then a saved pattern that
/// is none of them (kept as it is). Pure; tested.
pub fn lan_choices(wired: &[String], picked: &[String]) -> (Vec<String>, Vec<Vec<String>>, usize) {
    let mut labels = vec!["Any wired adapter".to_string()];
    let mut values = vec![Vec::new()];
    for d in wired {
        if !labels.contains(d) { labels.push(d.clone()); values.push(vec![d.clone()]); }
    }
    let at = values.iter().position(|v| v.len() == picked.len() && v.iter().zip(picked).all(|(a, b)| a.eq_ignore_ascii_case(b)));
    let at = at.unwrap_or_else(|| {
        labels.push(format!("{} (saved)", picked.join(", ")));
        values.push(picked.to_vec());
        values.len() - 1
    });
    (labels, values, at)
}

/// The VPN `tailscale_wifi_only` applies to.
pub fn is_tailscale(v: &Vpn) -> bool {
    v.all_programs().iter().any(|p| p.to_lowercase().ends_with("tailscaled.exe")) || v.name.eq_ignore_ascii_case("tailscale")
}

fn all_addresses() -> String { "*".into() }

// ------------------------------------------------------------------ snapshot

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adapter {
    pub index: u32,
    pub alias: String,
    pub description: String,
    pub up: bool,
    pub metric: u32,
    pub auto_metric: bool,
    #[serde(default)]
    pub kind: Kind,
}

/// What an adapter is, from its interface type (not its name, which depends on the language).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind { Wifi, Wired, #[default] Other }

/// Interface type (IANA ifType) and description -> kind. Wired virtual adapters (Hyper-V, VPN
/// taps, Bluetooth PAN) are not a LAN. Pure; tested.
pub fn kind_of(if_type: u32, description: &str) -> Kind {
    const VIRTUAL: [&str; 14] = ["virtual", "hyper-v", "vmware", "virtualbox", "tap-", "tap ", "wintun", "wireguard",
                                 "tailscale", "bluetooth", "loopback", "vpn", "miniport", "kernel debug"];
    let d = description.to_lowercase();
    if VIRTUAL.iter().any(|v| d.contains(v)) { return Kind::Other; }
    match if_type { 71 => Kind::Wifi, 6 => Kind::Wired, _ => Kind::Other }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub prefix: Ipv4Addr,
    pub len: u8,
    pub index: u32,
    pub next_hop: Ipv4Addr,
    pub metric: u32,
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}/{} via {} (if {})", self.prefix, self.len, self.next_hop, self.index)
    }
}

/// One of Keel's firewall rules. `aliases` comes from the rule's description, which Keel writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    /// Empty: every program.
    pub program: String,
    pub aliases: Vec<String>,
    /// Protocol number (6 TCP, 17 UDP) and remote ports, for port rules.
    #[serde(default)]
    pub ports: Option<(u8, String)>,
    #[serde(default = "all_addresses")]
    pub remote_addresses: String,
    /// Actual firewall direction and scope validity; unknown old snapshots never prove protection.
    #[serde(default)]
    pub outbound: bool,
    #[serde(default)]
    pub scope_valid: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub adapters: Vec<Adapter>,
    /// IPv4 routes in the active store.
    pub routes: Vec<Route>,
    pub policy: Option<u32>,
    pub rules: Vec<Rule>,
}

/// Values recorded before Keel first changed them (`%ProgramData%\keel\net-state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub policy_recorded: bool,
    pub policy_key_existed: bool,
    pub policy_old: Option<u32>,
    /// alias -> (automatic metric, metric)
    pub metrics: std::collections::BTreeMap<String, (bool, u32)>,
    /// Routes Keel added (and still owns).
    pub routes: Vec<Route>,
}

// ------------------------------------------------------------------ plan

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Action {
    SetPolicy,
    SetMetric { index: u32, alias: String, metric: u32 },
    RestoreMetric { index: u32, alias: String, auto: bool, metric: u32 },
    AddRoute(Route),
    RemoveRoute(Route),
    /// Create or replace a block rule for `program` (empty: every program) on `aliases`, enabled;
    /// with `ports`, only that protocol and those remote ports.
    BlockRule { name: String, program: String, aliases: Vec<String>, outbound: bool, ports: Option<(u8, String)>, remote_addresses: String },
    DisableRule { name: String },
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Action::SetPolicy => write!(f, "set fMinimizeConnections = 0 (keep Wi-Fi alongside the LAN; after a reboot)"),
            Action::SetMetric { alias, metric, .. } => write!(f, "set {alias} interface metric {metric}"),
            Action::RestoreMetric { alias, auto, metric, .. } =>
                if *auto { write!(f, "restore {alias} automatic metric") } else { write!(f, "restore {alias} metric {metric}") },
            Action::AddRoute(r) => write!(f, "add route {r}"),
            Action::RemoveRoute(r) => write!(f, "remove route {r}"),
            Action::BlockRule { name, aliases, .. } => write!(f, "firewall {name}: block on {}", aliases.join(", ")),
            Action::DisableRule { name } => write!(f, "disable firewall {name}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
    /// Side the default-route VPNs follow (lowest metric), and where general internet goes.
    pub follow: Option<Side>,
    pub internet: Option<Side>,
}

pub const SPLITS: [(Ipv4Addr, u8); 2] = [(Ipv4Addr::new(0, 0, 0, 0), 1), (Ipv4Addr::new(128, 0, 0, 0), 1)];
const ANY: Ipv4Addr = Ipv4Addr::UNSPECIFIED;

/// Case-insensitive match with `*` wildcards.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.to_lowercase(), s.to_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 { return p == s; }
    let mut rest = s.as_str();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 { if !rest.starts_with(part) { return false; } rest = &rest[part.len()..]; continue; }
        if i == parts.len() - 1 { return rest.ends_with(part); }
        match rest.find(part) { Some(at) => rest = &rest[at + part.len()..], None => return false }
    }
    true
}

/// `a.b.c.d/n` or `a.b.c.d` (= /32), normalized to the network address.
pub fn parse_prefix(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (a, n) = s.trim().split_once('/').map_or((s.trim(), 32), |(a, n)| (a, n.parse().unwrap_or(99)));
    let ip: Ipv4Addr = a.parse().ok()?;
    if n > 32 { return None; }
    let mask = if n == 0 { 0 } else { u32::MAX << (32 - n) };
    Some((Ipv4Addr::from(u32::from(ip) & mask), n))
}

impl Snapshot {
    pub fn wifi(&self, cfg: &Config) -> Vec<&Adapter> {
        let named: Vec<&Adapter> = self.adapters.iter()
            .filter(|a| !cfg.wifi_alias.is_empty() && a.alias.eq_ignore_ascii_case(&cfg.wifi_alias)).collect();
        if !named.is_empty() { return named; }
        self.adapters.iter().filter(|a| a.kind == Kind::Wifi).collect()
    }
    pub fn lans(&self, cfg: &Config) -> Vec<&Adapter> {
        if cfg.lan_descriptions.is_empty() { return self.adapters.iter().filter(|a| a.kind == Kind::Wired).collect(); }
        self.adapters.iter().filter(|a| cfg.lan_descriptions.iter().any(|p| glob(p, &a.description))).collect()
    }
    pub fn side(&self, cfg: &Config, s: Side) -> Vec<&Adapter> { match s { Side::Wifi => self.wifi(cfg), Side::Lan => self.lans(cfg) } }
    /// The adapter's default gateway (next hop of its `/0` route), when it has one.
    pub fn gateway(&self, index: u32) -> Option<Ipv4Addr> {
        self.routes.iter().filter(|r| r.index == index && r.len == 0 && r.next_hop != ANY)
            .min_by_key(|r| r.metric).map(|r| r.next_hop)
    }
    /// Up adapters of a side with a gateway, first one first.
    fn exits(&self, cfg: &Config, s: Side) -> Vec<(u32, Ipv4Addr)> {
        self.side(cfg, s).into_iter().filter(|a| a.up).filter_map(|a| self.gateway(a.index).map(|g| (a.index, g))).collect()
    }
    /// Where a default-route VPN goes now: the up, non-VPN adapter with the lowest route +
    /// interface metric on `/0` (tailscaled's rule).
    pub fn default_egress(&self, cfg: &Config) -> Option<&Adapter> {
        let vpn = |a: &Adapter| cfg.vpns.iter().any(|v| v.is_tunnel(a))
            || a.description.to_lowercase().contains("wintun");
        self.routes.iter().filter(|r| r.len == 0)
            .filter_map(|r| self.adapters.iter().find(|a| a.index == r.index && a.up && !vpn(a)).map(|a| (r.metric + a.metric, a)))
            .min_by_key(|(m, _)| *m).map(|(_, a)| a)
    }
}

/// The loopback pseudo-interface: never blocked (local IPC of the VPN clients).
fn is_loopback(a: &Adapter) -> bool { a.description.to_lowercase().contains("loopback") }

/// Validate relay input without changing configuration or firewall state.
/// A missing adapter is a runtime fail-closed condition, not permission to use another adapter.
pub fn validate_relay(v: &Vpn, snap: Option<&Snapshot>) -> Result<Vec<Ipv4Addr>, String> {
    if v.relay_hosts.is_empty() { return Ok(Vec::new()); }
    if v.via != Side::Wifi || !v.kill_switch { return Err("LAN relay requires Wi-Fi and the kill switch".into()); }
    if v.all_programs().is_empty() { return Err("LAN relay requires a VPN program".into()); }
    if v.relay_interface.is_empty() || v.relay_interface.contains(['*', '?', ';', '|']) {
        return Err("select an exact Ethernet adapter".into());
    }
    let mut hosts = Vec::new();
    for host in &v.relay_hosts {
        let ip = host.parse::<Ipv4Addr>().map_err(|_| format!("invalid private IPv4 host: {host}"))?;
        if !ip.is_private() { return Err(format!("not an RFC1918 private IPv4 host: {host}")); }
        hosts.push(ip);
    }
    hosts.sort_unstable(); hosts.dedup();
    if let Some(snap) = snap {
        if !snap.adapters.iter().any(|a| a.alias == v.relay_interface && a.kind == Kind::Wired && !v.is_tunnel(a)) {
            return Err("the selected Ethernet adapter is unavailable".into());
        }
    }
    Ok(hosts)
}

/// Syntax validation for persisted settings; Tailscale's enforced Wi-Fi policy remains authoritative.
pub fn validate_relays(cfg: &Config) -> Result<(), String> {
    for v in &cfg.vpns {
        let mut v = v.clone();
        if cfg.tailscale_wifi_only && is_tailscale(&v) { v.via = Side::Wifi; v.kill_switch = true; }
        validate_relay(&v, None).map_err(|e| format!("{}: {e}", v.name))?;
    }
    Ok(())
}

/// One kill-switch rule: name, program (empty: every program), outbound, ports.
struct RuleSpec {
    name: String, program: String, outbound: bool, ports: Option<(u8, String)>,
    aliases: Vec<String>, remote_addresses: String,
}

/// Emit an inclusive IPv4 interval as aligned CIDRs, rather than one very broad
/// address range. Use explicit /1 IPv6 blocks below as well: avoid /0 special
/// handling in the Windows Firewall address parser. Coverage is unchanged.
fn ipv4_interval_cidrs(mut start: u64, end: u64, out: &mut Vec<String>) {
    while start <= end {
        let aligned = if start == 0 { 32 } else { (start as u32).trailing_zeros() };
        let remaining = 63 - (end - start + 1).leading_zeros();
        let bits = aligned.min(remaining);
        out.push(format!("{}/{}", Ipv4Addr::from(start as u32), 32 - bits));
        start += 1u64 << bits;
    }
}

/// Complement of exact private IPv4 hosts, including all IPv6. Invalid input fails closed.
fn relay_complement(hosts: &[String]) -> Option<String> {
    if hosts.is_empty() { return None; }
    let mut values = Vec::new();
    for host in hosts {
        let ip = host.parse::<Ipv4Addr>().ok()?;
        if !ip.is_private() { return None; }
        values.push(u32::from(ip));
    }
    values.sort_unstable();
    values.dedup();
    let mut ranges = Vec::new();
    let mut start = 0u64;
    for value in values {
        if start < value as u64 {
            ipv4_interval_cidrs(start, value as u64 - 1, &mut ranges);
        }
        start = value as u64 + 1;
    }
    if start <= u32::MAX as u64 {
        ipv4_interval_cidrs(start, u32::MAX as u64, &mut ranges);
    }
    ranges.extend(["::/1".into(), "8000::/1".into()]);
    Some(ranges.join(","))
}

/// A VPN's kill-switch rules: `Keel net: <vpn> out|in` for its first program, `... out #2` for the
/// next ones, `Keel net: <vpn> udp 51820 out` per port entry. Bad or common port entries are left
/// out and reported in `problems`.
fn rule_specs(v: &Vpn, snap: &Snapshot, keep: &[u32], problems: &mut Vec<String>) -> Vec<RuleSpec> {
    let dir = |o: bool| if o { "out" } else { "in" };
    let mut out = Vec::new();
    for (i, prog) in v.all_programs().iter().enumerate() {
        for o in [true, false] {
            let name = if i == 0 { format!("Keel net: {} {}", v.name, dir(o)) } else { format!("Keel net: {} {} #{}", v.name, dir(o), i + 1) };
            out.push((name, prog.clone(), o, None));
        }
    }
    for spec in &v.ports {
        match parse_ports(spec) {
            None => problems.push(format!("{}: bad port entry {spec:?} (e.g. udp 51820)", v.name)),
            Some((_, ports)) if ports.split(',').any(|x| COMMON_PORTS.contains(&x)) =>
                problems.push(format!("{}: {spec:?} would block common traffic for every program; left out", v.name)),
            Some(pp) => out.push((format!("Keel net: {} {} out", v.name, spec.trim().to_lowercase().replace(':', " ")), String::new(), true, Some(pp))),
        }
    }
    let mut aliases: Vec<String> = snap.adapters.iter()
        .filter(|a| !keep.contains(&a.index) && !is_loopback(a) && !v.is_tunnel(a))
        .map(|a| a.alias.clone()).collect();
    aliases.sort();
    let relay = match validate_relay(v, Some(snap)) {
        Ok(_) => relay_complement(&v.relay_hosts).filter(|_| !keep.iter().any(|i| snap.adapters.iter()
            .any(|a| a.index == *i && a.alias == v.relay_interface))),
        Err(e) => { problems.push(format!("{}: {e}; full block retained", v.name)); None }
    };
    let mut scoped = Vec::new();
    for (name, program, outbound, ports) in out {
        // Global/kernel-port rules never receive relay exceptions.
        if let Some(addresses) = relay.as_ref().filter(|_| !program.is_empty() && ports.is_none()) {
            use sha2::{Digest, Sha256};
            let fingerprint = serde_json::to_vec(&(&program, outbound, &aliases, &v.relay_interface, addresses)).unwrap();
            let digest = Sha256::digest(&fingerprint).iter().map(|b| format!("{b:02x}")).collect::<String>();
            let name = format!("{name} {digest}");
            scoped.push(RuleSpec { name: format!("{name} relay external"), program: program.clone(), outbound,
                ports: None, aliases: vec![v.relay_interface.clone()], remote_addresses: addresses.clone() });
            let others: Vec<String> = aliases.iter().filter(|a| **a != v.relay_interface).cloned().collect();
            if !others.is_empty() {
                scoped.push(RuleSpec { name: format!("{name} relay other interfaces"), program, outbound,
                    ports: None, aliases: others, remote_addresses: "*".into() });
            }
        } else if !aliases.is_empty() {
            scoped.push(RuleSpec { name, program, outbound, ports, aliases: aliases.clone(), remote_addresses: "*".into() });
        }
    }
    scoped
}

/// Host routes a `Server` VPN added itself on the wrong side while its tunnel is up: `/32`s via
/// the gateway of an adapter that is not on `via` (its server, routed by the best route, which
/// Keel's `/1` splits may have put on the other side). Keel adds the same host route via `via`.
fn learned_servers(cfg: &Config, snap: &Snapshot, state: &State, v: &Vpn) -> Vec<String> {
    if v.kind != VpnKind::Server || !snap.adapters.iter().any(|a| a.up && v.is_tunnel(a)) { return Vec::new(); }
    let other: Vec<u32> = snap.side(cfg, if v.via == Side::Wifi { Side::Lan } else { Side::Wifi }).iter().map(|a| a.index).collect();
    let mut out: Vec<String> = snap.routes.iter()
        .filter(|r| r.len == 32 && other.contains(&r.index) && r.next_hop != ANY && snap.gateway(r.index) == Some(r.next_hop))
        .filter(|r| !state.routes.iter().any(|o| o.prefix == r.prefix && o.len == 32 && o.index == r.index))
        .map(|r| format!("{}/32", r.prefix)).collect();
    out.sort();
    out.dedup();
    out
}

/// The routes Keel owns in `snap`: the ones it recorded, plus `/1` splits on Wi-Fi / LAN adapters
/// (adopted: they can only be ours).
fn owned(cfg: &Config, snap: &Snapshot, state: &State) -> Vec<Route> {
    let sides: Vec<u32> = snap.wifi(cfg).iter().chain(snap.lans(cfg).iter()).map(|a| a.index).collect();
    snap.routes.iter().filter(|r| {
        state.routes.iter().any(|o| o.prefix == r.prefix && o.len == r.len && o.index == r.index && o.next_hop == r.next_hop)
            || (sides.contains(&r.index) && SPLITS.contains(&(r.prefix, r.len)))
    }).copied().collect()
}

pub fn plan(cfg: &Config, snap: &Snapshot, state: &State) -> Plan {
    let mut p = Plan::default();
    let mut vpns = cfg.vpns.clone();
    for v in vpns.iter_mut().filter(|v| cfg.tailscale_wifi_only && is_tailscale(v)) {
        if v.via == Side::Lan { p.problems.push(format!("{}: external Tailscale transport must use Wi-Fi; kept on Wi-Fi", v.name)); }
        v.via = Side::Wifi;
        v.kill_switch = true;
    }
    if !(1..=4).contains(&cfg.top_metric) { p.problems.push("top_metric must be 1-4".into()); }

    if cfg.keep_wifi_with_lan && snap.policy != Some(0) { p.actions.push(Action::SetPolicy); }

    // Metrics: the follow side gets top_metric; sides Keel changed before go back.
    let followers: Vec<&Vpn> = vpns.iter().filter(|v| v.kind == VpnKind::DefaultRoute).collect();
    let follow = followers.first().map_or(cfg.internet, |v| v.via);
    if let Some(v) = followers.iter().find(|v| v.via != follow) {
        p.problems.push(format!("{} wants {} but default-route VPNs share one side ({follow})", v.name, v.via));
    }
    let internet = cfg.internet;
    if cfg.enabled { (p.follow, p.internet) = (Some(follow), Some(internet)); }
    let lead: Vec<&Adapter> = if cfg.enabled { snap.side(cfg, follow) } else { Vec::new() };
    for a in &snap.adapters {
        let ours = lead.iter().any(|l| l.index == a.index);
        if ours && (a.auto_metric || a.metric != cfg.top_metric) {
            p.actions.push(Action::SetMetric { index: a.index, alias: a.alias.clone(), metric: cfg.top_metric });
        } else if !ours && let Some(&(auto, m)) = state.metrics.get(&a.alias) && (a.auto_metric != auto || (!auto && a.metric != m)) {
            p.actions.push(Action::RestoreMetric { index: a.index, alias: a.alias.clone(), auto, metric: m });
        }
    }

    // Routes: desired set vs the routes Keel owns.
    let mut want: Vec<Route> = Vec::new();
    if cfg.enabled {
        let route = |(index, gw): (u32, Ipv4Addr), (prefix, len): (Ipv4Addr, u8)| Route { prefix, len, index, next_hop: gw, metric: 0 };
        if internet != follow {
            let exits = snap.exits(cfg, internet);
            if exits.is_empty() { p.notes.push(format!("{internet} is not connected: general internet uses {follow}")); }
            for e in exits { for s in SPLITS { want.push(route(e, s)); } }
        }
        let mut targets: Vec<(String, Side)> = cfg.routes.iter().map(|r| (r.prefix.clone(), r.via)).collect();
        for v in vpns.iter().filter(|v| v.kind == VpnKind::Server) {
            for s in &v.servers { targets.push((s.clone(), v.via)); }
            for s in learned_servers(cfg, snap, state, v) {
                p.notes.push(format!("{}: server {s} moved to {}", v.name, v.via));
                targets.push((s, v.via));
            }
        }
        for (t, side) in targets {
            match (parse_prefix(&t), snap.exits(cfg, side).first()) {
                (None, _) => p.problems.push(format!("bad prefix {t}")),
                (Some(_), None) => p.notes.push(format!("{t}: {side} is not connected; no route")),
                (Some(pre), Some(&e)) => want.push(route(e, pre)),
            }
        }
    }
    let have = owned(cfg, snap, state);
    let same = |a: &Route, b: &Route| a.prefix == b.prefix && a.len == b.len && a.index == b.index && a.next_hop == b.next_hop;
    for r in &have { if !want.iter().any(|w| same(w, r)) { p.actions.push(Action::RemoveRoute(*r)); } }
    for w in &want { if !snap.routes.iter().any(|r| same(w, r)) { p.actions.push(Action::AddRoute(*w)); } }

    // Keep old protection until every replacement rule is observed in the snapshot.
    // Planning a replacement is not proof it was successfully installed.
    let mut specs: Vec<(&Vpn, Vec<RuleSpec>)> = Vec::new();
    for v in &vpns {
        if !(cfg.enabled && v.kill_switch) { continue; }
        let keep: Vec<u32> = snap.side(cfg, v.via).iter().map(|a| a.index).collect();
        // Even if Wi-Fi disappears, build full blocks for all remaining non-tunnel adapters.
        if keep.is_empty() { p.problems.push(format!("{}: no {} adapter found; blocking remaining interfaces", v.name, v.via)); }
        let rules = rule_specs(v, snap, &keep, &mut p.problems);
        if rules.is_empty() { p.problems.push(format!("{}: the kill switch has no program or port to block", v.name)); }
        block_rules(&mut p, snap, &rules);
        specs.push((v, rules));
    }
    let wanted: Vec<&String> = specs.iter().flat_map(|(_, rules)| rules.iter().map(|r| &r.name)).collect();
    for old in snap.rules.iter().filter(|r| r.enabled && !wanted.contains(&&r.name)) {
        let safe = specs.iter().filter(|(v, _)| v.all_programs().iter().any(|prog| prog.eq_ignore_ascii_case(&old.program))
            || (old.program.is_empty() && v.ports.iter().any(|port| parse_ports(port) == old.ports)))
            .all(|(v, rules)| !snap.side(cfg, v.via).is_empty() && !rules.is_empty()
                && rules.iter().all(|spec| snap.rules.iter().any(|r| rule_matches(r, spec))));
        if safe { p.actions.push(Action::DisableRule { name: old.name.clone() }); }
        else { p.notes.push(format!("old firewall rule {} kept until the new kill switch is in place", old.name)); }
    }
    p
}

/// Block rules for `v` on every adapter outside `keep`, its tunnels and loopback.
fn block_rules(p: &mut Plan, snap: &Snapshot, rules: &[RuleSpec]) {
    for spec in rules {
        let ok = snap.rules.iter().any(|r| rule_matches(r, spec));
        if !ok {
            p.actions.push(Action::BlockRule { name: spec.name.clone(), program: spec.program.clone(),
                aliases: spec.aliases.clone(), outbound: spec.outbound, ports: spec.ports.clone(),
                remote_addresses: spec.remote_addresses.clone() });
        }
    }
}

fn rule_matches(r: &Rule, spec: &RuleSpec) -> bool {
    r.name == spec.name && r.enabled && r.scope_valid && r.outbound == spec.outbound && r.aliases == spec.aliases
        && r.program.eq_ignore_ascii_case(&spec.program) && same_ports(&r.ports, &spec.ports)
        && same_addresses(&r.remote_addresses, &spec.remote_addresses)
}

/// Windows may coalesce adjacent port entries into ranges.
fn same_ports(a: &Option<(u8, String)>, b: &Option<(u8, String)>) -> bool {
    fn ranges(s: &str) -> Option<Vec<(u16, u16)>> {
        let mut out = Vec::new();
        for item in s.split(',').map(str::trim) {
            let (lo, hi) = item.split_once('-').unwrap_or((item, item));
            let (lo, hi): (u16, u16) = (lo.trim().parse().ok()?, hi.trim().parse().ok()?);
            if lo == 0 || lo > hi { return None; }
            out.push((lo, hi));
        }
        out.sort_unstable();
        let mut merged: Vec<(u16, u16)> = Vec::new();
        for (lo, hi) in out {
            if let Some(last) = merged.last_mut() && lo <= last.1.saturating_add(1) { last.1 = last.1.max(hi); }
            else { merged.push((lo, hi)); }
        }
        Some(merged)
    }
    match (a, b) {
        (None, None) => true,
        (Some((ap, a)), Some((bp, b))) if ap == bp => match (ranges(a), ranges(b)) { (Some(a), Some(b)) => a == b, _ => false },
        _ => false,
    }
}

/// Normalize Windows' remote-address spelling into merged inclusive address ranges.
fn address_ranges(input: &str) -> Option<Vec<(u8, u128, u128)>> {
    use std::net::IpAddr;
    fn address(s: &str) -> Option<(u8, u128)> {
        match s.trim().parse::<IpAddr>().ok()? {
            IpAddr::V4(ip) => Some((4, u32::from(ip) as u128)),
            IpAddr::V6(ip) => Some((6, u128::from(ip))),
        }
    }
    let mut ranges = Vec::new();
    for item in input.split(',').map(str::trim) {
        if item == "*" {
            ranges.extend([(4, 0, u32::MAX as u128), (6, 0, u128::MAX)]);
        } else if let Some((left, right)) = item.split_once('-') {
            let (af, first) = address(left)?;
            let (bf, last) = address(right)?;
            if af != bf || first > last { return None; }
            ranges.push((af, first, last));
        } else if let Some((host, mask)) = item.split_once('/') {
            let (af, ip) = address(host)?;
            let bits = if af == 4 { 32 } else { 128 };
            let prefix: u32 = match mask.trim().parse::<u32>() {
                Ok(n) if n <= bits => n,
                Ok(_) => return None,
                Err(_) => {
                    let (mf, value) = address(mask)?;
                    if mf != af { return None; }
                    let m = if af == 4 { (value as u32).leading_ones() } else { value.leading_ones() };
                    let expected = if m == 0 { 0 } else { u128::MAX << (bits - m) };
                    let expected = if af == 4 { expected & u32::MAX as u128 } else { expected };
                    if value != expected { return None; }
                    m
                }
            };
            let full = if af == 4 { u32::MAX as u128 } else { u128::MAX };
            let hostmask = if prefix == 0 { full } else if prefix == bits { 0 } else { full >> prefix };
            let first = ip & (full ^ hostmask);
            ranges.push((af, first, first | hostmask));
        } else {
            let (af, ip) = address(item)?;
            ranges.push((af, ip, ip));
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u8, u128, u128)> = Vec::new();
    for (af, start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && last.0 == af && start <= last.2.saturating_add(1) {
            last.2 = last.2.max(end);
        } else {
            merged.push((af, start, end));
        }
    }
    if merged.is_empty() { None } else { Some(merged) }
}

fn same_addresses(a: &str, b: &str) -> bool {
    match (address_ranges(a), address_ranges(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// What the plan's actions change, recorded before any of them runs. Keeps the first record.
pub fn record(state: &State, snap: &Snapshot, actions: &[Action], policy_key_existed: bool) -> State {
    let mut s = state.clone();
    for a in actions {
        match a {
            Action::SetPolicy if !s.policy_recorded => {
                (s.policy_recorded, s.policy_key_existed, s.policy_old) = (true, policy_key_existed, snap.policy);
            }
            Action::SetMetric { index, alias, .. } if !s.metrics.contains_key(alias) => {
                if let Some(ad) = snap.adapters.iter().find(|x| x.index == *index) {
                    s.metrics.insert(alias.clone(), (ad.auto_metric, ad.metric));
                }
            }
            Action::AddRoute(r) => s.routes.push(*r),
            _ => {}
        }
    }
    s
}

/// One line for the tray / status: where things go now.
pub fn summary(cfg: &Config, snap: &Snapshot, p: &Plan) -> String {
    if !cfg.enabled { return "Off: Windows default routing".into(); }
    if let Some(x) = p.problems.first() { return x.clone(); }
    let egress = snap.default_egress(cfg).map_or("none".to_string(), |a| a.alias.clone());
    let lan_up = snap.lans(cfg).iter().any(|a| a.up);
    let wifi_up = snap.wifi(cfg).iter().any(|a| a.up);
    let net = match (cfg.internet, lan_up, wifi_up) {
        (Side::Lan, true, _) => "internet via LAN",
        (Side::Wifi, _, true) => "internet via Wi-Fi",
        (_, false, true) => "internet via Wi-Fi (LAN down)",
        (_, true, false) => "internet via LAN (Wi-Fi down)",
        _ => "no network",
    };
    let vpns: Vec<String> = cfg.vpns.iter().map(|v| format!("{} via {}", v.name, if cfg.tailscale_wifi_only && is_tailscale(v) { Side::Wifi } else { v.via })).collect();
    format!("{net}; {} (VPN egress now: {egress})", vpns.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(index: u32, alias: &str, desc: &str, up: bool, metric: u32) -> Adapter {
        Adapter { index, alias: alias.into(), description: desc.into(), up, metric, auto_metric: true, kind: kind_of(if alias == "Wi-Fi" { 71 } else { 6 }, desc) }
    }

    #[test]
    fn old_config_has_no_relay_exceptions() {
        let v: Vpn = toml::from_str("name = \"N\"").unwrap();
        assert!(v.relay_hosts.is_empty());
        assert!(v.relay_interface.is_empty());
        assert!(Vpn::default().relay_hosts.is_empty());
    }
    fn relay_config() -> Config {
        let mut cfg = Config::example();
        cfg.vpns[0].relay_interface = "Ethernet".into();
        cfg.vpns[0].relay_hosts = vec!["192.168.50.1".into(), "192.168.50.5".into()];
        cfg
    }

    fn planned_rules(p: &Plan) -> Vec<Rule> {
        p.actions.iter().filter_map(|a| match a {
            Action::BlockRule { name, program, aliases, ports, remote_addresses, outbound, .. } => Some(Rule {
                name: name.clone(), enabled: true, program: program.clone(), aliases: aliases.clone(),
                ports: ports.clone(), remote_addresses: remote_addresses.clone(), outbound: *outbound, scope_valid: true,
            }), _ => None
        }).collect()
    }

    #[test]
    fn relay_hosts_complement_and_input_validation() {
        assert!(same_addresses(&relay_complement(&relay_config().vpns[0].relay_hosts).unwrap(),
            "0.0.0.0-192.168.50.0,192.168.50.2-192.168.50.4,192.168.50.6-255.255.255.255,::/0"));
        for bad in ["0.0.0.0", "8.8.8.8", "::1", "192.168.50.0/24", "*", "127.0.0.1", "169.254.1.1"] {
            assert!(relay_complement(&[bad.into()]).is_none(), "{bad}");
        }
        assert!(relay_complement(&[]).is_none());
        let one = relay_complement(&["10.0.0.1".into()]).unwrap();
        assert_eq!(one, relay_complement(&["10.0.0.1".into(), "10.0.0.1".into()]).unwrap());
    }

    #[test]
    fn relay_cidrs_preserve_exact_host_exclusions() {
        for hosts in [
            vec!["192.168.50.1", "192.168.50.4", "192.168.50.5"],
            vec!["10.0.0.0", "10.0.0.1", "10.255.255.255"],
            vec!["172.16.0.0", "172.31.255.255", "192.168.255.255"],
            vec!["192.168.50.5", "192.168.50.1", "192.168.50.5"],
        ] {
            let hosts: Vec<String> = hosts.into_iter().map(String::from).collect();
            let complement = relay_complement(&hosts).unwrap();
            assert!(!complement.contains('-'));
            assert!(!complement.split(',').any(|c| c.ends_with("/0")));
            let mut expected: Vec<(u8, u128, u128)> = Vec::new();
            let mut ips: Vec<u32> = hosts.iter().map(|h| u32::from(h.parse::<Ipv4Addr>().unwrap())).collect();
            ips.sort_unstable();
            ips.dedup();
            let mut start = 0u128;
            for ip in ips {
                if start < ip as u128 { expected.push((4, start, ip as u128 - 1)); }
                start = ip as u128 + 1;
            }
            expected.push((4, start, u32::MAX as u128));
            expected.push((6, 0, u128::MAX));
            assert_eq!(address_ranges(&complement).unwrap(), expected);
            // A canonicalized mask returned by Windows must not trigger reapply.
            let windows = complement.split(',').map(|c| {
                let (addr, prefix) = c.split_once('/').unwrap();
                if addr.contains(':') { return c.to_owned(); }
                let prefix: u32 = prefix.parse().unwrap();
                format!("{addr}/{}", Ipv4Addr::from(u32::MAX << (32 - prefix)))
            }).collect::<Vec<_>>().join(",");
            assert!(same_addresses(&complement, &windows));
        }
    }

    #[test]
    fn relay_only_exact_ethernet_and_actual_addresses_are_compared() {
        let cfg = relay_config();
        let mut snap = both_up();
        let p = plan(&cfg, &snap, &State::default());
        let rules = planned_rules(&p);
        assert_eq!(rules.len(), 4);
        for rule in &rules {
            if rule.name.ends_with("relay external") {
                assert_eq!(rule.aliases, ["Ethernet"]);
                assert!(rule.remote_addresses.ends_with(",::/1,8000::/1"));
            } else {
                assert_eq!(rule.remote_addresses, "*");
                assert!(!rule.aliases.contains(&"Ethernet".into()));
                assert!(!rule.aliases.contains(&"Wi-Fi".into()));
            }
        }
        snap.rules = rules;
        assert!(planned_rules(&plan(&cfg, &snap, &State::default())).is_empty());
        snap.rules[0].remote_addresses = "10.0.0.0/8".into(); // external tampering must be repaired
        assert_eq!(planned_rules(&plan(&cfg, &snap, &State::default())).len(), 1);
    }

    #[test]
    fn relay_migration_is_fail_closed_and_clear_restores_full_block() {
        let cfg = relay_config();
        let mut snap = both_up();
        snap.rules = planned_rules(&plan(&Config::example(), &snap, &State::default()));
        let p = plan(&cfg, &snap, &State::default());
        assert!(!p.actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        snap.rules.extend(planned_rules(&p));
        let ready = plan(&cfg, &snap, &State::default());
        assert_eq!(ready.actions.iter().filter(|a| matches!(a, Action::DisableRule { .. })).count(), 2);
        snap.rules.retain(|r| r.name.contains("relay "));
        let mut cleared = cfg;
        cleared.vpns[0].relay_hosts.clear();
        let p = plan(&cleared, &snap, &State::default());
        assert!(p.problems.is_empty());
        assert_eq!(planned_rules(&p).len(), 2);
        assert!(!p.actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        assert!(planned_rules(&p).iter().all(|r| r.remote_addresses == "*"));
    }

    #[test]
    fn relay_save_validation_and_backward_compatibility() {
        let old: Vpn = toml::from_str("name = \"Old\"\n").unwrap();
        assert!(old.relay_hosts.is_empty());
        let mut cfg = relay_config();
        cfg.vpns[0].via = Side::Lan;
        assert!(validate_relays(&cfg).is_err());
        cfg.tailscale_wifi_only = true;
        assert!(validate_relays(&cfg).is_ok());
        cfg.vpns[0].relay_hosts = vec!["100.64.0.1".into()];
        assert!(validate_relays(&cfg).is_err());
        cfg.vpns[0].relay_hosts.clear();
        assert!(validate_relays(&cfg).is_ok());
        let encoded = toml::to_string(&relay_config()).unwrap();
        assert_eq!(toml::from_str::<Config>(&encoded).unwrap(), relay_config());
    }

    #[test]
    fn relay_adapter_change_keeps_old_generation_during_partial_install() {
        let a = relay_config();
        let mut snap = both_up();
        snap.adapters.push(Adapter { index: 99, alias: "USB Ethernet".into(), description: "USB Ethernet".into(),
            kind: Kind::Wired, up: true, metric: 5, auto_metric: true });
        let old = planned_rules(&plan(&a, &snap, &State::default()));
        snap.rules = old.clone();
        let mut b = a.clone(); b.vpns[0].relay_interface = "USB Ethernet".into();
        let pending = plan(&b, &snap, &State::default());
        let new = planned_rules(&pending);
        assert_eq!(new.len(), 4);
        assert!(new.iter().all(|n| old.iter().all(|o| n.name != o.name)));
        assert!(!pending.actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        // Only one Add succeeds: every old guard still stays active.
        snap.rules.push(new[0].clone());
        assert!(!plan(&b, &snap, &State::default()).actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        snap.rules.extend(new[1..].to_vec());
        assert_eq!(plan(&b, &snap, &State::default()).actions.iter().filter(|a| matches!(a, Action::DisableRule { .. })).count(), old.len());
        // A rule's metadata cannot prove an allow or wrong-direction rule protects the interface.
        snap.rules.last_mut().unwrap().scope_valid = false;
        assert!(!plan(&b, &snap, &State::default()).actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
    }

    #[test]
    fn relay_invalid_inputs_or_missing_interface_keep_full_block() {
        for interface in ["", "Missing", "Wi-Fi", "Tailscale", "Ether*"] {
            let mut cfg = relay_config();
            cfg.vpns[0].relay_interface = interface.into();
            let p = plan(&cfg, &both_up(), &State::default());
            assert!(!p.problems.is_empty(), "{interface}");
            assert_eq!(planned_rules(&p).len(), 2);
            assert!(planned_rules(&p).iter().all(|r| r.remote_addresses == "*"));
        }
        let mut cfg = relay_config();
        cfg.vpns[0].relay_hosts.push("8.8.8.8".into());
        let p = plan(&cfg, &both_up(), &State::default());
        assert!(!p.problems.is_empty());
        assert!(planned_rules(&p).iter().all(|r| r.remote_addresses == "*"));
    }

    #[test]
    fn relay_missing_wifi_or_invalid_config_does_not_retire_old_guards() {
        let mut cfg = relay_config();
        let mut snap = both_up();
        snap.rules = planned_rules(&plan(&Config::example(), &snap, &State::default()));
        snap.adapters.retain(|a| a.alias != "Wi-Fi");
        let p = plan(&cfg, &snap, &State::default());
        assert!(!p.actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        let mut snap = both_up();
        snap.rules = planned_rules(&plan(&cfg, &snap, &State::default()));
        cfg.vpns[0].relay_interface = "Missing".into();
        let p = plan(&cfg, &snap, &State::default());
        assert!(!p.actions.iter().any(|a| matches!(a, Action::DisableRule { .. })));
        assert!(planned_rules(&p).iter().all(|r| r.remote_addresses == "*"));
    }

    #[test]
    fn relay_wifi_down_keeps_external_block() {
        let cfg = relay_config();
        let mut snap = both_up();
        snap.adapters.iter_mut().find(|a| a.alias == "Wi-Fi").unwrap().up = false;
        let rules = planned_rules(&plan(&cfg, &snap, &State::default()));
        assert_eq!(rules.len(), 4);
        assert!(rules.iter().filter(|r| r.name.ends_with("external")).all(|r| r.remote_addresses.ends_with("::/1,8000::/1")));
    }
    #[test]
    fn lan_picker_choices() {
        let wired = vec!["ASIX AX88179".to_string(), "Intel I225-V".to_string()];
        let (l, v, at) = lan_choices(&wired, &[]);
        assert_eq!((l.len(), at), (3, 0));
        assert!(v[0].is_empty());
        let (_, v, at) = lan_choices(&wired, &["intel i225-v".to_string()]);
        assert_eq!(v[at], ["Intel I225-V"]);
        let (l, v, at) = lan_choices(&wired, &["Realtek USB GbE*".to_string()]);
        assert_eq!((l[at].as_str(), v[at][0].as_str()), ("Realtek USB GbE* (saved)", "Realtek USB GbE*"));
    }

    #[test]
    fn adapters_found_by_type_on_any_windows() {
        assert_eq!(kind_of(71, "MediaTek Wi-Fi 6E"), Kind::Wifi);
        assert_eq!(kind_of(6, "Intel(R) Ethernet Controller I225-V"), Kind::Wired);
        assert_eq!(kind_of(6, "Hyper-V Virtual Ethernet Adapter"), Kind::Other);
        assert_eq!(kind_of(6, "Bluetooth Device (Personal Area Network)"), Kind::Other);
        assert_eq!(kind_of(53, "Tailscale Tunnel"), Kind::Other);
        assert_eq!(kind_of(71, "Microsoft Wi-Fi Direct Virtual Adapter #2"), Kind::Other);
        assert_eq!(kind_of(6, "WAN Miniport (IP)"), Kind::Other);
        assert_eq!(kind_of(6, "Cisco AnyConnect Virtual Miniport Adapter for Windows x64"), Kind::Other);
        assert_eq!(kind_of(6, "Realtek USB GbE Family Controller"), Kind::Wired);
        // German Windows: "WLAN" and "Ethernet 2"; a new config finds both by type.
        let mut w = ad(3, "WLAN", "Intel Wi-Fi 6 AX201", true, 40); w.kind = Kind::Wifi;
        let mut e = ad(4, "Ethernet 2", "ASIX AX88179 USB 3.0 to Gigabit Ethernet", true, 5); e.kind = Kind::Wired;
        let mut v = ad(5, "vEthernet", "Hyper-V Virtual Ethernet Adapter", true, 15); v.kind = Kind::Other;
        let s = Snapshot { adapters: vec![w, e, v], routes: vec![], policy: None, rules: vec![] };
        let c = Config::default();
        assert_eq!(s.wifi(&c).iter().map(|a| a.index).collect::<Vec<_>>(), [3]);
        assert_eq!(s.lans(&c).iter().map(|a| a.index).collect::<Vec<_>>(), [4]);
        // An old "Wi-Fi" alias that does not exist here falls back to the type; a picked LAN wins.
        let c = Config { wifi_alias: "Wi-Fi".into(), lan_descriptions: vec!["ASIX*".into()], ..Config::default() };
        assert_eq!(s.wifi(&c).len(), 1);
        assert_eq!(s.lans(&c)[0].index, 4);
    }
    fn r(prefix: &str, index: u32, gw: &str, metric: u32) -> Route {
        let (p, l) = parse_prefix(prefix).unwrap();
        Route { prefix: p, len: l, index, next_hop: gw.parse().unwrap(), metric }
    }
    /// Wi-Fi (12) and LAN (7) both up with gateways, Tailscale tunnel (30), AnyConnect (40).
    fn both_up() -> Snapshot {
        Snapshot {
            adapters: vec![ad(12, "Wi-Fi", "RZ616 Wi-Fi 6E 160MHz", true, 40), ad(7, "Ethernet", "Realtek USB GbE Family Controller", true, 5),
                           ad(30, "Tailscale", "Tailscale Tunnel", true, 5), ad(40, "Ethernet 2", "Cisco AnyConnect Virtual Miniport", false, 1),
                           ad(1, "Loopback Pseudo-Interface 1", "Software Loopback Interface 1", true, 75)],
            routes: vec![r("0.0.0.0/0", 12, "192.168.1.1", 0), r("0.0.0.0/0", 7, "192.168.235.1", 0)],
            policy: None,
            rules: vec![],
        }
    }
    fn adds(p: &Plan) -> Vec<String> { p.actions.iter().filter_map(|a| if let Action::AddRoute(r) = a { Some(r.to_string()) } else { None }).collect() }

    #[test]
    fn globs_and_prefixes() {
        assert!(glob("Realtek USB GbE*", "realtek usb gbe family controller"));
        assert!(glob("*Tunnel", "Tailscale Tunnel") && glob("Wi-Fi", "WI-FI") && !glob("Tailscale*", "WireGuard Tunnel"));
        assert_eq!(parse_prefix("10.1.2.3/8"), Some(("10.0.0.0".parse().unwrap(), 8)));
        assert_eq!(parse_prefix("1.2.3.4"), Some(("1.2.3.4".parse().unwrap(), 32)));
        assert_eq!(parse_prefix("1.2.3.4/33"), None);
    }

    #[test]
    fn default_config_plan() {
        let cfg = Config::example();
        let p = plan(&cfg, &both_up(), &State::default());
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert!(p.actions.contains(&Action::SetPolicy));
        assert!(p.actions.contains(&Action::SetMetric { index: 12, alias: "Wi-Fi".into(), metric: 1 }));
        assert_eq!(adds(&p), ["0.0.0.0/1 via 192.168.235.1 (if 7)", "128.0.0.0/1 via 192.168.235.1 (if 7)"]);
        // Kill switch: tailscaled blocked on everything but Wi-Fi and its tunnel, both directions.
        let rules: Vec<_> = p.actions.iter().filter_map(|a| if let Action::BlockRule { aliases, .. } = a { Some(aliases.clone()) } else { None }).collect();
        assert_eq!(rules, vec![vec!["Ethernet".to_string(), "Ethernet 2".into()]; 2]);
    }

    #[test]
    fn internet_over_wifi_needs_no_splits() {
        let cfg = Config { internet: Side::Wifi, ..Config::example() };
        let mut snap = both_up();
        snap.routes.push(r("0.0.0.0/1", 7, "192.168.235.1", 0)); // a split adopted from before
        let p = plan(&cfg, &snap, &State::default());
        assert!(adds(&p).is_empty());
        assert!(p.actions.contains(&Action::RemoveRoute(r("0.0.0.0/1", 7, "192.168.235.1", 0))));
    }

    #[test]
    fn wireguard_on_lan_moves_the_follow_side_and_splits_wifi() {
        let wg = Vpn { name: "WireGuard".into(), adapter: "WireGuard Tunnel*".into(), via: Side::Lan, ..Default::default() };
        let cfg = Config { internet: Side::Wifi, vpns: vec![wg], ..Config::example() };
        let p = plan(&cfg, &both_up(), &State::default());
        assert_eq!(p.follow, Some(Side::Lan));
        assert!(p.actions.contains(&Action::SetMetric { index: 7, alias: "Ethernet".into(), metric: 1 }));
        assert_eq!(adds(&p), ["0.0.0.0/1 via 192.168.1.1 (if 12)", "128.0.0.0/1 via 192.168.1.1 (if 12)"]);
    }

    #[test]
    fn tailscale_wifi_only_keeps_it_off_the_lan() {
        let mut cfg = Config { tailscale_wifi_only: true, ..Config::example() };
        cfg.vpns[0].via = Side::Lan;
        cfg.vpns[0].kill_switch = false;
        let p = plan(&cfg, &both_up(), &State::default());
        assert!(p.problems[0].contains("must use Wi-Fi"));
        assert_eq!(p.follow, Some(Side::Wifi));
        assert!(p.actions.iter().any(|a| matches!(a, Action::BlockRule { aliases, .. } if aliases.contains(&"Ethernet".to_string()))));
    }

    #[test]
    fn without_wifi_only_tailscale_follows_its_via() {
        let mut cfg = Config::example();
        cfg.vpns[0].via = Side::Lan;
        cfg.vpns[0].kill_switch = false;
        let p = plan(&cfg, &both_up(), &State::default());
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.follow, Some(Side::Lan));
        assert!(!p.actions.iter().any(|a| matches!(a, Action::BlockRule { .. })));
        assert!(summary(&cfg, &both_up(), &p).contains("Tailscale via LAN"));
    }

    #[test]
    fn wifi_only_defaults_on_for_files_without_it() {
        let c: Config = toml::from_str("internet = \"wifi\"\n").unwrap();
        assert!(c.tailscale_wifi_only);
        assert!(!Config::default().tailscale_wifi_only && !Config::default().enabled && Config::default().vpns.is_empty());
        assert!(!toml::from_str::<Config>(&toml::to_string(&Config::example()).unwrap()).unwrap().tailscale_wifi_only);
    }

    #[test]
    fn server_vpn_and_rules_get_host_routes() {
        let nord = Vpn { name: "OpenVPN".into(), kind: VpnKind::Server, via: Side::Lan, servers: vec!["203.0.113.7".into()], ..Default::default() };
        let mut cfg = Config::example();
        cfg.vpns.push(nord);
        cfg.routes.push(RouteRule { prefix: "10.20.0.0/16".into(), via: Side::Lan });
        let a = adds(&plan(&cfg, &both_up(), &State::default()));
        assert!(a.contains(&"203.0.113.7/32 via 192.168.235.1 (if 7)".to_string()));
        assert!(a.contains(&"10.20.0.0/16 via 192.168.235.1 (if 7)".to_string()));
    }

    #[test]
    fn applied_state_is_stable_and_off_undoes_it() {
        let cfg = Config::example();
        let mut snap = both_up();
        let p = plan(&cfg, &snap, &State::default());
        let state = record(&State::default(), &snap, &p.actions, false);
        // Apply the plan to the snapshot by hand.
        snap.policy = Some(0);
        snap.adapters[0].metric = 1; snap.adapters[0].auto_metric = false;
        for a in &p.actions { if let Action::AddRoute(x) = a { snap.routes.push(*x); } }
        snap.rules = p.actions.iter().filter_map(|a| if let Action::BlockRule { name, program, aliases, ports, remote_addresses, outbound } = a {
            Some(Rule { name: name.clone(), enabled: true, program: program.clone(), aliases: aliases.clone(), ports: ports.clone(), remote_addresses: remote_addresses.clone(), outbound: *outbound, scope_valid: true }) } else { None }).collect();
        assert_eq!(plan(&cfg, &snap, &state).actions, vec![]);
        assert_eq!(state.metrics["Wi-Fi"], (true, 40));
        assert_eq!(state.policy_old, None);

        let off = plan(&Config { enabled: false, ..cfg }, &snap, &state);
        assert!(off.actions.contains(&Action::RestoreMetric { index: 12, alias: "Wi-Fi".into(), auto: true, metric: 40 }));
        assert_eq!(off.actions.iter().filter(|a| matches!(a, Action::RemoveRoute(_))).count(), 2);
        assert_eq!(off.actions.iter().filter(|a| matches!(a, Action::DisableRule { .. })).count(), 2);
        assert!(!off.actions.contains(&Action::SetPolicy));
    }

    #[test]
    fn windows_port_filter_normalization() {
        assert!(same_ports(&Some((17, "1194-1196".into())), &Some((17, "1196,1194,1195".into()))));
        assert!(!same_ports(&Some((17, "1194-1196".into())), &Some((17, "1194,1196".into()))));
        assert!(!same_ports(&Some((6, "1194".into())), &Some((17, "1194".into()))));
    }

    #[test]
    fn port_entries() {
        assert_eq!(parse_ports("udp 51820"), Some((17, "51820".into())));
        assert_eq!(parse_ports("TCP:1194, 1195"), Some((6, "1194,1195".into())));
        assert_eq!(parse_ports("51820"), None);
        assert_eq!(parse_ports("udp x"), None);
    }

    /// NordVPN on Wi-Fi while general internet goes over the LAN, NordLynx connected.
    fn nord_snapshot() -> (Config, Snapshot) {
        let mut nord = Vpn::presets()[1].clone();
        nord.programs = vec![r"C:\Program Files\NordVPN\8.15.2.0\nordvpn-service-app.exe".into()];
        let cfg = Config { vpns: vec![nord], ..Config::example() };
        let mut snap = both_up();
        snap.adapters.push(ad(37, "NordLynx", "NordLynx Tunnel", true, 1));
        // NordVPN routed its server by the best route: Keel's /1 split on the LAN.
        snap.routes.push(r("0.0.0.0/1", 7, "192.168.235.1", 0));
        snap.routes.push(r("128.0.0.0/1", 7, "192.168.235.1", 0));
        snap.routes.push(r("185.1.2.3/32", 7, "192.168.235.1", 0));
        snap.routes.push(r("192.168.235.20/32", 7, "0.0.0.0", 0)); // the LAN's own address: on-link
        (cfg, snap)
    }

    #[test]
    fn nordvpn_kill_switch_blocks_its_service_and_the_kernel_port() {
        let (cfg, snap) = nord_snapshot();
        let p = plan(&cfg, &snap, &State::default());
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let rules: Vec<(String, String, Option<(u8, String)>, Vec<String>)> = p.actions.iter().filter_map(|a| match a {
            Action::BlockRule { name, program, ports, aliases, .. } => Some((name.clone(), program.clone(), ports.clone(), aliases.clone())), _ => None }).collect();
        let names: Vec<&str> = rules.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, ["Keel net: NordVPN out", "Keel net: NordVPN in", "Keel net: NordVPN out #2", "Keel net: NordVPN in #2", "Keel net: NordVPN udp 51820 out"]);
        assert!(rules[0].1.ends_with("nordvpn-service.exe"));
        assert_eq!((rules[4].1.as_str(), rules[4].2.clone()), ("", Some((17, "51820".into()))));
        // Its own tunnel and the Wi-Fi are never blocked.
        assert!(rules.iter().all(|r| !r.3.contains(&"NordLynx".to_string()) && !r.3.contains(&"Wi-Fi".to_string())));
        assert!(rules[0].3.contains(&"Ethernet".to_string()));
    }

    #[test]
    fn nordvpn_server_route_moves_to_its_side() {
        let (cfg, snap) = nord_snapshot();
        let p = plan(&cfg, &snap, &State::default());
        assert_eq!(adds(&p), ["185.1.2.3/32 via 192.168.1.1 (if 12)"]);
        // Tunnel down: nothing learned (other host routes on the LAN are left alone).
        let mut down = snap.clone();
        down.adapters.last_mut().unwrap().up = false;
        assert!(adds(&plan(&cfg, &down, &State::default())).is_empty());
        // A default-route VPN never learns routes.
        let mut dr = cfg.clone();
        dr.vpns[0].kind = VpnKind::DefaultRoute;
        assert!(adds(&plan(&dr, &snap, &State::default())).is_empty());
    }

    #[test]
    fn removed_vpn_and_common_ports() {
        let (mut cfg, mut snap) = nord_snapshot();
        snap.rules.push(Rule { name: "Keel net: Old VPN out".into(), enabled: true, program: "x.exe".into(), aliases: vec!["Ethernet".into()], ports: None, remote_addresses: "*".into(), outbound: true, scope_valid: true });
        cfg.vpns[0].ports.push("tcp 443".into());
        let p = plan(&cfg, &snap, &State::default());
        assert!(p.actions.contains(&Action::DisableRule { name: "Keel net: Old VPN out".into() }));
        assert!(p.problems.iter().any(|x| x.contains("tcp 443")));
        assert!(!p.actions.iter().any(|a| matches!(a, Action::BlockRule { name, .. } if name.contains("443"))));
    }

    #[test]
    fn old_entries_keep_their_programs() {
        let v: Vpn = toml::from_str("name = \"N\"\nkind = \"server\"\nexe = 'C:\\a.exe'\nadapter = \"A*, B*\"\n").unwrap();
        assert_eq!((v.all_programs(), v.adapter_patterns()), (vec![r"C:\a.exe".to_string()], vec!["A*".to_string(), "B*".into()]));
    }

    #[test]
    fn lan_down_means_no_splits_and_a_note() {
        let mut snap = both_up();
        snap.adapters[1].up = false;
        let p = plan(&Config::example(), &snap, &State::default());
        assert!(adds(&p).is_empty());
        assert!(p.notes[0].contains("LAN is not connected"));
    }
}
