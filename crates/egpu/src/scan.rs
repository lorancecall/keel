//! Installed games for the Games page, from the launchers' own records: Steam libraries (every
//! drive, `libraryfolders.vdf` and the app manifests), Epic (launcher manifests) and GOG (registry).
//! Other games the owner adds as programs. Steam's text records name the folder but not the
//! program, so the program that draws is picked from the folder (`pick_exe`); Epic names its
//! program, which wins unless it only starts an Unreal `*-Shipping.exe`. Read-only; worker thread.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub exe: PathBuf,
    /// Steam, Epic or GOG.
    pub source: &'static str,
}

/// Values of `"key" "value"` pairs in a Valve KeyValues text (`libraryfolders.vdf`, `*.acf`).
pub fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // `\t"key"\t\t"value"` splits into ["\t", key, "\t\t", value, ""].
        if parts.len() >= 5 && parts[1].eq_ignore_ascii_case(key) { out.push(parts[3].replace("\\\\", "\\")); }
    }
    out
}

/// Programs that are never the game itself.
const NOT_GAME: &[&str] = &[
    "unins", "setup", "redist", "vcredist", "vc_redist", "dxsetup", "dotnet", "crash", "report", "easyanticheat",
    "battleye", "beservice", "eac", "prereq", "installer", "helper", "cefprocess", "webhelper", "updater", "touchup",
    "uploader", "benchmark", "dxwebsetup", "physx", "oalinst",
];

/// Picks the program that draws from a game folder's programs (path relative to the folder, size):
/// Unreal's `*-Win64-Shipping.exe` first, then a name close to the game's, then the largest.
pub fn pick_exe(cands: &[(PathBuf, u64)], game: &str) -> Option<usize> {
    let squash = |s: &str| s.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
    let g = squash(game);
    let score = |(p, size): &(PathBuf, u64)| {
        let name = p.file_stem().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let shipping = name.ends_with("-win64-shipping") || name.ends_with("-wingdk-shipping");
        let n = squash(&name);
        // Close: one name inside the other, or the same first five letters ("Hades II" / Hades2.exe).
        let prefix = n.chars().zip(g.chars()).take_while(|(a, b)| a == b).count();
        let close = !n.is_empty() && !g.is_empty() && (g.contains(&n) || n.contains(&g) || prefix >= 5.min(n.len()).min(g.len()).max(4));
        let launcher = name.contains("launcher");
        (shipping as u8, close as u8, !launcher as u8, *size)
    };
    cands.iter().enumerate()
        .filter(|(_, (p, _))| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            !NOT_GAME.iter().any(|x| n.contains(x))
        })
        .max_by_key(|(_, c)| score(c)).map(|(i, _)| i)
}

/// Programs under `dir`, at most four folders deep and 400 of them.
fn programs(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() { if depth < 4 { stack.push((p, depth + 1)); } }
            else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) {
                out.push((p.strip_prefix(dir).map(Path::to_path_buf).unwrap_or(p.clone()), m.len()));
                if out.len() >= 400 { return out; }
            }
        }
    }
    out
}

fn game_in(dir: &Path, name: &str, source: &'static str) -> Option<Found> {
    let cands = programs(dir);
    let i = pick_exe(&cands, name)?;
    Some(Found { name: name.to_string(), exe: dir.join(&cands[i].0), source })
}

fn steam() -> Vec<Found> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Valve\Steam").and_then(|k| k.get_value::<String, _>("SteamPath")) else { return Vec::new() };
    let vdf = std::fs::read_to_string(Path::new(&root).join("steamapps").join("libraryfolders.vdf")).unwrap_or_default();
    let mut libs = vdf_values(&vdf, "path");
    if libs.is_empty() { libs.push(root.clone()); }
    let mut out = Vec::new();
    for lib in libs {
        let apps = Path::new(&lib).join("steamapps");
        for e in std::fs::read_dir(&apps).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if !(n.starts_with("appmanifest_") && n.ends_with(".acf")) { continue; }
            let acf = std::fs::read_to_string(e.path()).unwrap_or_default();
            let (Some(name), Some(dir)) = (vdf_values(&acf, "name").into_iter().next(), vdf_values(&acf, "installdir").into_iter().next()) else { continue };
            let low = name.to_lowercase();
            if ["redistributable", "proton", "steam linux runtime", "steamvr"].iter().any(|x| low.contains(x)) { continue; }
            if let Some(f) = game_in(&apps.join("common").join(&dir), &name, "Steam") { out.push(f); }
        }
    }
    out
}

fn epic() -> Vec<Found> {
    let dir = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(r"Epic\EpicGamesLauncher\Data\Manifests");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if !e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")) { continue; }
        let Some(v) = std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { continue };
        let (Some(name), Some(loc)) = (v["DisplayName"].as_str(), v["InstallLocation"].as_str()) else { continue };
        let named = v["LaunchExecutable"].as_str().map(|e| Path::new(loc).join(e)).filter(|p| p.exists());
        // The Unreal stub starts the real game: Windows' GPU choice must be on the one that draws.
        let shipping = game_in(Path::new(loc), name, "Epic").filter(|f| f.exe.to_string_lossy().to_lowercase().ends_with("-shipping.exe"));
        match (shipping, named) {
            (Some(f), _) => out.push(f),
            (None, Some(exe)) => out.push(Found { name: name.to_string(), exe, source: "Epic" }),
            (None, None) => if let Some(f) = game_in(Path::new(loc), name, "Epic") { out.push(f); },
        }
    }
    out
}

fn gog() -> Vec<Found> {
    use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};
    let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\WOW6432Node\GOG.com\Games") else { return Vec::new() };
    let mut out = Vec::new();
    for id in k.enum_keys().flatten() {
        let Ok(g) = k.open_subkey(&id) else { continue };
        let (Ok(name), Ok(path)) = (g.get_value::<String, _>("gameName"), g.get_value::<String, _>("path")) else { continue };
        if let Some(f) = game_in(Path::new(&path), &name, "GOG") { out.push(f); }
    }
    out
}

/// Every installed game the launchers know, by name; one entry per program.
pub fn installed() -> Vec<Found> {
    let mut v: Vec<Found> = steam().into_iter().chain(epic()).chain(gog()).collect();
    v.sort_by_key(|f| f.name.to_lowercase());
    v.dedup_by(|a, b| a.exe.to_string_lossy().eq_ignore_ascii_case(&b.exe.to_string_lossy()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_valve_key_values() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"E:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(vdf_values(vdf, "path"), [r"C:\Program Files (x86)\Steam", r"E:\SteamLibrary"]);
    }

    #[test]
    fn picks_the_program_that_draws() {
        let c = |v: &[(&str, u64)]| v.iter().map(|(p, s)| (PathBuf::from(p), *s)).collect::<Vec<_>>();
        let ue = c(&[("Palworld.exe", 300_000), (r"Pal\Binaries\Win64\Palworld-Win64-Shipping.exe", 150_000_000),
                     (r"Engine\Binaries\Win64\CrashReportClient.exe", 30_000_000)]);
        assert_eq!(pick_exe(&ue, "Palworld"), Some(1));
        let mh = c(&[("MonsterHunterWorld.exe", 400_000_000), ("unins000.exe", 1_000_000), (r"_CommonRedist\vcredist_x64.exe", 9_000_000)]);
        assert_eq!(pick_exe(&mh, "Monster Hunter: World"), Some(0));
        let launcher = c(&[("GameLauncher.exe", 90_000_000), ("Game.exe", 80_000_000)]);
        assert_eq!(pick_exe(&launcher, "Some Game"), Some(1), "a launcher loses to the game");
        assert_eq!(pick_exe(&c(&[("unins000.exe", 1)]), "x"), None);
        let hades = c(&[(r"Release\F10.exe", 90_000_000), (r"Release\Hades2.exe", 60_000_000)]);
        assert_eq!(pick_exe(&hades, "Hades II"), Some(1), "same first letters beat size");
    }
}
