//! Kill-switch firewall rules (Windows Firewall COM, INetFwPolicy2). Keel's rules are in the group
//! "Keel net"; each rule's description carries its interface list (`aliases: a;b`) so the planner
//! can compare it without reading the VARIANT back. (`|` in a description makes `Add` fail with
//! E_INVALIDARG.)
use crate::model::Rule;
use anyhow::{Context, Result, anyhow};
use windows::Win32::NetworkManagement::WindowsFirewall::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_ARRAY, VT_VARIANT};
use windows::core::{BSTR, Interface};

pub const GROUP: &str = "Keel net";
const ALIASES: &str = "aliases: ";

fn policy() -> Result<INetFwPolicy2> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER).map_err(|e| anyhow!("firewall: {e}"))
    }
}

/// Keel's rules (group "Keel net"). Empty when the firewall cannot be read.
pub fn rules() -> Vec<Rule> {
    let Ok(p) = policy() else { return Vec::new() };
    let mut out = Vec::new();
    unsafe {
        let Ok(rules) = p.Rules() else { return out };
        let Ok(e) = rules._NewEnum() else { return out };
        let Ok(e) = e.cast::<windows::Win32::System::Ole::IEnumVARIANT>() else { return out };
        loop {
            let mut v = [VARIANT::default()];
            let mut got = 0u32;
            if e.Next(&mut v, &mut got).is_err() || got == 0 { break; }
            // The enumerator hands out VT_DISPATCH; IUnknown::try_from only takes VT_UNKNOWN.
            let r = match windows::Win32::System::Com::IDispatch::try_from(&v[0]) {
                Ok(d) => d.cast::<INetFwRule>(),
                Err(_) => windows::core::IUnknown::try_from(&v[0]).and_then(|u| u.cast::<INetFwRule>()),
            };
            let Ok(r) = r else { continue };
            if r.Grouping().map(|g| g.to_string()).unwrap_or_default() != GROUP { continue; }
            let desc = r.Description().map(|d| d.to_string()).unwrap_or_default();
            out.push(Rule {
                name: r.Name().map(|n| n.to_string()).unwrap_or_default(),
                enabled: r.Enabled().map(|b| b.as_bool()).unwrap_or(false),
                program: r.ApplicationName().map(|n| n.to_string()).unwrap_or_default(),
                aliases: desc.split_once(ALIASES).map(|(_, a)| a.split(';').map(String::from).collect()).unwrap_or_default(),
            });
        }
    }
    out
}

fn alias_list(aliases: &[String]) -> Result<VARIANT> {
    unsafe {
        let sa = SafeArrayCreateVector(VT_VARIANT, 0, aliases.len() as u32);
        if sa.is_null() { return Err(anyhow!("SafeArrayCreateVector failed")); }
        for (i, a) in aliases.iter().enumerate() {
            let v = VARIANT::from(BSTR::from(a.as_str()));
            SafeArrayPutElement(sa, &(i as i32), &v as *const VARIANT as *const _)?;
        }
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VARENUM(VT_ARRAY.0 | VT_VARIANT.0);
        (*var.Anonymous.Anonymous).Anonymous.parray = sa;
        Ok(var)
    }
}

/// Replaces rule `name` with an enabled block rule for `program` on `aliases`.
pub fn block(name: &str, program: &str, aliases: &[String], outbound: bool) -> Result<()> {
    anyhow::ensure!(!aliases.is_empty(), "no interfaces: a rule without interfaces would block everywhere");
    let p = policy()?;
    unsafe {
        let rules = p.Rules()?;
        let _ = rules.Remove(&BSTR::from(name));
        let r: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
        r.SetName(&BSTR::from(name)).context("SetName")?;
        r.SetDescription(&BSTR::from(format!("Keel kill switch. {ALIASES}{}", aliases.join(";")))).context("SetDescription")?;
        r.SetGrouping(&BSTR::from(GROUP)).context("SetGrouping")?;
        r.SetApplicationName(&BSTR::from(program)).context("SetApplicationName")?;
        r.SetDirection(if outbound { NET_FW_RULE_DIR_OUT } else { NET_FW_RULE_DIR_IN }).context("SetDirection")?;
        r.SetAction(NET_FW_ACTION_BLOCK).context("SetAction")?;
        r.SetProfiles(NET_FW_PROFILE2_ALL.0).context("SetProfiles")?;
        r.SetInterfaces(&alias_list(aliases)?).context("SetInterfaces")?;
        r.SetEnabled(true.into()).context("SetEnabled")?;
        rules.Add(&r).context("add")?;
    }
    Ok(())
}

pub fn disable(name: &str) -> Result<()> {
    let p = policy()?;
    unsafe { p.Rules()?.Item(&BSTR::from(name))?.SetEnabled(false.into())?; }
    Ok(())
}

/// Removes every rule in Keel's group (undo).
pub fn remove_all() -> Result<usize> {
    let names: Vec<String> = rules().into_iter().map(|r| r.name).collect();
    let p = policy()?;
    unsafe { let rules = p.Rules()?; for n in &names { rules.Remove(&BSTR::from(n.as_str()))?; } }
    Ok(names.len())
}
