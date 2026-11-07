//! `keel net ...`: network routing. The service applies the config on every network change;
//! `apply` / `undo` do it by hand.
use anyhow::{Result, bail};
use keel_net::model::{self, Side};

pub const HELP: &str = "\
keel net - Wi-Fi beside the LAN, internet side, VPN pinning, kill switches

USAGE (commands marked * need an administrator prompt)
  keel net status         adapters, default routes, Keel's routes and rules, what apply would change
  keel net plan           only what apply would change
  keel net apply        * record the original values, then bring routes / metrics / rules / policy
                            to the config (the service does this on every network change)
  keel net undo         * put everything back as recorded and remove Keel's routes and rules
  keel net init         * write the default config to edit
  keel net help

CONFIG (%ProgramData%\\keel\\net.toml)
  enabled = true
  keep_wifi_with_lan = true         Wi-Fi stays connected while Ethernet is up (after a reboot)
  internet = \"lan\" | \"wifi\"         where general internet goes while both are up
  wifi_alias = \"Wi-Fi\"   lan_descriptions = [\"Realtek USB GbE*\"]   top_metric = 1
  [[routes]] prefix = \"10.0.0.0/8\"  via = \"lan\"
  [[vpns]]   name, kind = \"default-route\" | \"server\", exe, adapter (description, * wildcards),
             via = \"wifi\" | \"lan\", kill_switch = true | false, servers = [\"a.b.c.d\"] (server kind)
  tailscale_wifi_only = true | false  Tailscale never uses the wired LAN (kept on Wi-Fi, kill
                                      switch on); default false, true when net.toml lacks it
  Default-route VPNs (Tailscale, WireGuard, NordLynx) bind to the side with the best 0.0.0.0/0
  route, so they share one side.
";

fn print_status(full: bool) -> Result<()> {
    let r = keel_net::report()?;
    let (cfg, snap, plan) = (&r.cfg, &r.snap, &r.plan);
    if full {
        let src = if keel_net::config_path().exists() { keel_net::config_path().display().to_string() } else { "defaults (no net.toml)".into() };
        println!("config: {src}");
        let pol = match snap.policy { Some(0) => "0 (Wi-Fi stays beside Ethernet)".to_string(), Some(v) => format!("{v} (Wi-Fi may drop)"), None => "not set (Wi-Fi may drop when Ethernet is up)".into() };
        println!("policy fMinimizeConnections: {pol}");
        println!("\nadapters:");
        let role = |a: &model::Adapter| {
            if snap.wifi(cfg).iter().any(|x| x.index == a.index) { "wifi" }
            else if snap.lans(cfg).iter().any(|x| x.index == a.index) { "lan" }
            else if cfg.vpns.iter().any(|v| !v.adapter.is_empty() && model::glob(&v.adapter, &a.description)) { "vpn" } else { "" }
        };
        for a in &snap.adapters {
            let gw = snap.gateway(a.index).map(|g| g.to_string()).unwrap_or("-".into());
            println!("  {:<5} {:<30} {:<5} metric {:<3}{:<5} gw {:<15} {}", role(a), a.alias, if a.up { "up" } else { "down" },
                a.metric, if a.auto_metric { " auto" } else { "" }, gw, a.description);
        }
        println!("\ndefault routes (route + interface metric):");
        let mut defs: Vec<(u32, &model::Route, &model::Adapter)> = snap.routes.iter().filter(|x| x.len == 0)
            .filter_map(|x| snap.adapters.iter().find(|a| a.index == x.index).map(|a| (x.metric + a.metric, x, a))).collect();
        defs.sort_by_key(|d| d.0);
        for (m, x, a) in defs { println!("  {m:<4} {:<30} via {}", a.alias, x.next_hop); }
        let splits: Vec<String> = snap.routes.iter().filter(|x| model::SPLITS.contains(&(x.prefix, x.len)) || r.state.routes.contains(x))
            .map(|x| x.to_string()).collect();
        println!("Keel / split routes: {}", if splits.is_empty() { "none".into() } else { splits.join("; ") });
        for rule in &snap.rules {
            println!("firewall {}: {} on {}", rule.name, if rule.enabled { "block" } else { "disabled" }, rule.aliases.join(", "));
        }
        let side = |s: Option<Side>| s.map_or("-".to_string(), |s| s.to_string());
        println!("\nVPN side (lowest metric): {}; general internet: {}", side(plan.follow), side(plan.internet));
        println!("=> {}", model::summary(cfg, snap, plan));
    }
    for p in &plan.problems { println!("! {p}"); }
    for n in &plan.notes { println!("  note: {n}"); }
    if plan.actions.is_empty() { println!("apply would change nothing"); }
    else { println!("apply would:"); for a in &plan.actions { println!("  {a}"); } }
    Ok(())
}

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("status") | None => print_status(true),
        Some("plan") => print_status(false),
        Some("apply") => {
            let (ok, bad) = keel_net::reconcile()?;
            println!("{ok} change(s) made, {bad} failed (log: {})", keel_core::log::service_log_path().display());
            print_status(false)
        }
        Some("undo") => { for l in keel_net::undo()? { println!("{l}"); } Ok(()) }
        Some("init") => { println!("{}", keel_net::write_default_config()?.display()); Ok(()) }
        Some("help" | "-h" | "--help") => { print!("{HELP}"); Ok(()) }
        Some(x) => bail!("unknown command net {x}; see `keel net help`"),
    }
}
