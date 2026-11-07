//! Starting other programs. The tray and the main window have no console, so every console tool
//! they ran (`wevtutil` every 5 minutes, `schtasks` on each scan) made Windows open a terminal
//! window that flashed up and went away. Every launch in the workspace goes
//! through here: console tools never get a window; programs with their own window (explorer, the
//! main window) start normally. A test fails if any other file under `crates/` or `apps/` calls
//! `Command::new` directly.
use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A console tool whose output is read: never shows a window.
pub fn hidden(exe: impl AsRef<OsStr>) -> Command {
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// A program that shows its own window (explorer, keel-gui).
pub fn windowed(exe: impl AsRef<OsStr>) -> Command { Command::new(exe) }

#[cfg(test)]
mod tests {
    /// Lines that start a program without going through this module.
    fn direct_launches(src: &str) -> Vec<String> {
        src.lines().filter(|l| l.contains("Command::new(") && !l.trim_start().starts_with("//"))
            .map(|l| l.trim().to_string()).collect()
    }

    #[test]
    fn finds_direct_launches() {
        assert_eq!(direct_launches("let o = std::process::Command::new(\"x\").output();\n// Command::new( in a comment"),
                   vec!["let o = std::process::Command::new(\"x\").output();".to_string()]);
    }

    #[test]
    fn every_launch_goes_through_cmd() {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() { if p.file_name().is_none_or(|n| n != "target") { walk(&p, out); } continue; }
                if p.extension().is_none_or(|x| x != "rs") || p.file_name().is_some_and(|n| n == "cmd.rs") { continue; }
                let src = std::fs::read_to_string(&p).unwrap();
                out.extend(direct_launches(&src).into_iter().map(|l| format!("{}: {l}", p.display())));
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut found = Vec::new();
        for d in ["crates", "apps"] { walk(&root.join(d), &mut found); }
        assert!(found.is_empty(), "use cmd::hidden / cmd::windowed instead:\n{}", found.join("\n"));
    }
}
