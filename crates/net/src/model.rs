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
//! - With `tailscale_wifi_only`, Tailscale never uses the wired LAN (e.g. a company network):
//!   forced to Wi-Fi with the kill switch, whatever its own entry says.
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
    /// Connects to fixed servers (`servers`); they get host routes via `via`.
    Server,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vpn {
    pub name: String,
    pub kind: VpnKind,
    /// The program whose traffic the kill switch blocks off `via`.
    pub exe: String,
    /// Its tunnel adapter, by description (`*` wildcards); never blocked.
    pub adapter: String,
    pub via: Side,
    pub kill_switch: bool,
    /// `Server` VPNs: server addresses (`a.b.c.d` or `a.b.c.d/n`).
    pub servers: Vec<String>,
}

impl Default for Vpn {
    fn default() -> Self {
        Self { name: String::new(), kind: VpnKind::DefaultRoute, exe: String::new(), adapter: String::new(),
               via: Side::Wifi, kill_switch: false, servers: Vec::new() }
    }
}

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
    pub wifi_alias: String,
    /// Wired adapters, by description (`*` wildcards).
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
    fn default() -> Self { Self { enabled: false, vpns: Vec::new(), ..Self::example() } }
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

/// The VPN `tailscale_wifi_only` applies to.
pub fn is_tailscale(v: &Vpn) -> bool {
    v.exe.to_lowercase().ends_with("tailscaled.exe") || v.name.eq_ignore_ascii_case("tailscale")
}

// ------------------------------------------------------------------ snapshot

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adapter {
    pub index: u32,
    pub alias: String,
    pub description: String,
    pub up: bool,
    pub metric: u32,
    pub auto_metric: bool,
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
    pub program: String,
    pub aliases: Vec<String>,
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
    /// Create or replace a block rule for `program` on `aliases`, enabled.
    BlockRule { name: String, program: String, aliases: Vec<String>, outbound: bool },
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
        self.adapters.iter().filter(|a| a.alias.eq_ignore_ascii_case(&cfg.wifi_alias)).collect()
    }
    pub fn lans(&self, cfg: &Config) -> Vec<&Adapter> {
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
        let vpn = |a: &Adapter| cfg.vpns.iter().any(|v| !v.adapter.is_empty() && glob(&v.adapter, &a.description))
            || a.description.to_lowercase().contains("wintun");
        self.routes.iter().filter(|r| r.len == 0)
            .filter_map(|r| self.adapters.iter().find(|a| a.index == r.index && a.up && !vpn(a)).map(|a| (r.metric + a.metric, a)))
            .min_by_key(|(m, _)| *m).map(|(_, a)| a)
    }
}

/// The loopback pseudo-interface: never blocked (local IPC of the VPN clients).
fn is_loopback(a: &Adapter) -> bool { a.description.to_lowercase().contains("loopback") }

fn rule_name(v: &Vpn, outbound: bool) -> String {
    format!("Keel net: {} {}", v.name, if outbound { "out" } else { "in" })
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
        if v.via == Side::Lan { p.problems.push(format!("{}: Tailscale never uses the wired LAN; kept on Wi-Fi", v.name)); }
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
        for v in vpns.iter().filter(|v| v.kind == VpnKind::Server) { for s in &v.servers { targets.push((s.clone(), v.via)); } }
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

    // Kill switches.
    for v in &vpns {
        let names = [(rule_name(v, true), true), (rule_name(v, false), false)];
        if cfg.enabled && v.kill_switch {
            let keep: Vec<u32> = snap.side(cfg, v.via).iter().map(|a| a.index).collect();
            if keep.is_empty() {
                p.problems.push(format!("{}: no {} adapter found; kill switch left as it is", v.name, v.via));
                continue;
            }
            let mut block: Vec<String> = snap.adapters.iter()
                .filter(|a| !keep.contains(&a.index) && !is_loopback(a) && !(!v.adapter.is_empty() && glob(&v.adapter, &a.description)))
                .map(|a| a.alias.clone()).collect();
            block.sort();
            if block.is_empty() { continue; } // an empty list would mean "every interface"
            for (name, outbound) in names {
                let ok = snap.rules.iter().any(|r| r.name == name && r.enabled && r.aliases == block && r.program.eq_ignore_ascii_case(&v.exe));
                if !ok { p.actions.push(Action::BlockRule { name, program: v.exe.clone(), aliases: block.clone(), outbound }); }
            }
        } else {
            for (name, _) in names {
                if snap.rules.iter().any(|r| r.name == name && r.enabled) { p.actions.push(Action::DisableRule { name }); }
            }
        }
    }
    p
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
        Adapter { index, alias: alias.into(), description: desc.into(), up, metric, auto_metric: true }
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
        assert!(p.problems[0].contains("never uses the wired LAN"));
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
        snap.rules = p.actions.iter().filter_map(|a| if let Action::BlockRule { name, program, aliases, .. } = a {
            Some(Rule { name: name.clone(), enabled: true, program: program.clone(), aliases: aliases.clone() }) } else { None }).collect();
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
    fn lan_down_means_no_splits_and_a_note() {
        let mut snap = both_up();
        snap.adapters[1].up = false;
        let p = plan(&Config::example(), &snap, &State::default());
        assert!(adds(&p).is_empty());
        assert!(p.notes[0].contains("LAN is not connected"));
    }
}
