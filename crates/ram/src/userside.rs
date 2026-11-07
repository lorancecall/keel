//! The user-side half of on-demand families: HKCU startup items, switched without admin by the
//! CLI and the main window (the service, as LocalSystem, only handles machine-wide items).
/// HKCU startup items of on-demand families: switched here, as the user (no admin needed).
pub fn user_state_path() -> std::path::PathBuf {
    crate::paths::user_data_dir().join("ondemand-user.json")
}
type UserState = std::collections::BTreeMap<String, Vec<crate::ondemand::Startup>>;

pub fn user_off(key: &str, items: &[(String, String)]) -> anyhow::Result<()> {
    let p = user_state_path();
    let mut st: UserState = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let rec = st.entry(key.to_string()).or_default();
    for (k, v) in items {
        if !rec.iter().any(|s| &s.key == k && &s.value == v) {
            rec.push(crate::ondemand::Startup { key: k.clone(), value: v.clone(), bytes: crate::switch::startup_get(k, v) });
        }
    }
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_string_pretty(&st)?)?; // originals first
    for (k, v) in items { crate::switch::startup_set(k, v, Some(&crate::switch::STARTUP_OFF))?; }
    Ok(())
}

pub fn user_restore(key: &str) -> anyhow::Result<()> {
    let p = user_state_path();
    let mut st: UserState = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for s in st.remove(key).unwrap_or_default() { crate::switch::startup_set(&s.key, &s.value, s.bytes.as_deref())?; }
    std::fs::write(&p, serde_json::to_string_pretty(&st)?)?;
    Ok(())
}

