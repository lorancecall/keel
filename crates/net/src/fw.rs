//! Kill-switch firewall rules (Windows Firewall COM, INetFwPolicy2). Keel's rules are in the group
//! "Keel net". Read actual filters rather than trusting description metadata when retiring guards.
use crate::model::Rule;
use anyhow::{Context, Result, anyhow};
use windows::Win32::NetworkManagement::WindowsFirewall::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement, SafeArrayGetDim, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayGetElement};
use windows::Win32::System::Variant::{VARIANT, VT_ARRAY, VT_VARIANT, VT_BSTR};
use windows::core::{BSTR, Interface};

pub const GROUP: &str = "Keel net";
const ALIASES: &str = "aliases: ";
const PORTS: &str = "ports: ";

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
            let aliases = r.Interfaces().ok().and_then(|v| read_aliases(&v).ok());
            let proto = r.Protocol().ok();
            let ports = match proto {
                Some(6 | 17) => r.RemotePorts().ok().map(|p| (proto.unwrap() as u8, p.to_string())),
                _ => None,
            };
            let scope_valid = aliases.is_some() && r.Action().ok() == Some(NET_FW_ACTION_BLOCK)
                && r.Profiles().ok() == Some(NET_FW_PROFILE2_ALL.0)
                && matches!(proto, Some(256 | 6 | 17))
                && (proto == Some(256) || ports.is_some())
                && r.LocalAddresses().is_ok_and(|a| a.to_string() == "*")
                && r.InterfaceTypes().is_ok_and(|t| t.to_string().eq_ignore_ascii_case("All"))
                && (proto == Some(256) || r.LocalPorts().is_ok_and(|p| p.to_string() == "*"));
            out.push(Rule {
                name: r.Name().map(|n| n.to_string()).unwrap_or_default(),
                enabled: r.Enabled().map(|b| b.as_bool()).unwrap_or(false),
                program: r.ApplicationName().map(|n| n.to_string()).unwrap_or_default(),
                aliases: aliases.unwrap_or_default(),
                ports,
                outbound: r.Direction().ok() == Some(NET_FW_RULE_DIR_OUT),
                scope_valid: scope_valid && matches!(r.Direction().ok(), Some(NET_FW_RULE_DIR_OUT | NET_FW_RULE_DIR_IN)),
                remote_addresses: r.RemoteAddresses().map(|a| a.to_string()).unwrap_or_default(),
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

/// INetFwRule::Interfaces is a SAFEARRAY; malformed/unknown values never prove protection.
fn read_aliases(v: &VARIANT) -> Result<Vec<String>> {
    unsafe {
        let raw = &*v.Anonymous.Anonymous;
        let vt = raw.vt.0;
        anyhow::ensure!(vt == VT_ARRAY.0 | VT_VARIANT.0 || vt == VT_ARRAY.0 | VT_BSTR.0, "unexpected interface array");
        let sa = raw.Anonymous.parray;
        anyhow::ensure!(!sa.is_null() && SafeArrayGetDim(sa) == 1, "invalid interface array");
        let lo = SafeArrayGetLBound(sa, 1)?;
        let hi = SafeArrayGetUBound(sa, 1)?;
        anyhow::ensure!(hi >= lo && (hi as i64 - lo as i64) < 4096, "invalid interface count");
        let mut out = Vec::new();
        for i in lo..=hi {
            let name = if vt == VT_ARRAY.0 | VT_VARIANT.0 {
                let mut x = VARIANT::default();
                SafeArrayGetElement(sa, &i, (&mut x as *mut VARIANT).cast())?;
                anyhow::ensure!(x.Anonymous.Anonymous.vt == VT_BSTR, "non-string interface alias");
                x.Anonymous.Anonymous.Anonymous.bstrVal.to_string()
            } else {
                let mut x = BSTR::default();
                SafeArrayGetElement(sa, &i, (&mut x as *mut BSTR).cast())?;
                x.to_string()
            };
            anyhow::ensure!(!name.is_empty(), "empty interface alias");
            out.push(name);
        }
        out.sort(); out.dedup();
        Ok(out)
    }
}

/// Replaces rule `name` with an enabled block rule for `program` (empty: every program) on
/// `aliases`; with `ports`, only that protocol (6 / 17) and those remote ports.
pub fn block(name: &str, program: &str, aliases: &[String], outbound: bool, ports: Option<&(u8, String)>, remote_addresses: &str) -> Result<()> {
    anyhow::ensure!(!aliases.is_empty(), "no interfaces: a rule without interfaces would block everywhere");
    let p = policy()?;
    unsafe {
        let rules = p.Rules()?;
        // Add replaces by identifier; retain existing protection if the replacement fails.
        let r: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
        r.SetName(&BSTR::from(name)).context("SetName")?;
        let pdesc = ports.map_or(String::new(), |(proto, list)| format!("{PORTS}{proto}/{list} "));
        r.SetDescription(&BSTR::from(format!("Keel kill switch. {pdesc}{ALIASES}{}", aliases.join(";")))).context("SetDescription")?;
        r.SetGrouping(&BSTR::from(GROUP)).context("SetGrouping")?;
        if !program.is_empty() { r.SetApplicationName(&BSTR::from(program)).context("SetApplicationName")?; }
        if let Some((proto, list)) = ports {
            r.SetProtocol(*proto as i32).context("SetProtocol")?;
            r.SetRemotePorts(&BSTR::from(list.as_str())).context("SetRemotePorts")?;
        }
        r.SetRemoteAddresses(&BSTR::from(remote_addresses)).context("SetRemoteAddresses")?;
        r.SetDirection(if outbound { NET_FW_RULE_DIR_OUT } else { NET_FW_RULE_DIR_IN }).context("SetDirection")?;
        r.SetAction(NET_FW_ACTION_BLOCK).context("SetAction")?;
        r.SetProfiles(NET_FW_PROFILE2_ALL.0).context("SetProfiles")?;
        r.SetInterfaces(&alias_list(aliases)?).context("SetInterfaces")?;
        r.SetEnabled(true.into()).context("SetEnabled")?;
        rules.Add(&r).with_context(|| format!("add (remote addresses: {remote_addresses})"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_interface_array_round_trips() {
        let aliases = vec!["Ethernet".into(), "USB LAN".into()];
        let value = alias_list(&aliases).unwrap();
        assert_eq!(read_aliases(&value).unwrap(), aliases);
        assert!(read_aliases(&VARIANT::from(BSTR::from("Ethernet"))).is_err());
    }
}
