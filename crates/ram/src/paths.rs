//! Pure path / command-line normalization used to match a running exe against its launchers
//! (Run values, service ImagePaths, task actions, shortcut targets). No Win32 calls here.

/// Expands `%NAME%` with `lookup`; unknown names and lone `%` are kept as written.
pub fn expand_env(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => match lookup(&after[..j]) {
                Some(v) => { out.push_str(&v); rest = &after[j + 1..]; }
                None => { out.push('%'); out.push_str(&after[..j]); rest = &after[j..]; }
            },
            _ => { out.push('%'); rest = after; }
        }
    }
    out.push_str(rest);
    out
}

/// Comparable form of a file path: `\` separators, no `\??\` / `\\?\` prefix, `\SystemRoot\` and
/// bare `system32\...` resolved against `system_root`, lowercase.
pub fn normalize_path(p: &str, system_root: &str) -> String {
    let mut s = p.trim().trim_matches('"').replace('/', "\\");
    for pre in [r"\??\", r"\\?\"] {
        if let Some(r) = s.strip_prefix(pre) { s = r.to_string(); }
    }
    let lower = s.to_ascii_lowercase();
    let root = system_root.trim_end_matches('\\');
    if let Some(r) = lower.strip_prefix(r"\systemroot\") {
        s = format!(r"{root}\{}", &s[s.len() - r.len()..]);
    } else if lower.starts_with(r"system32\") || lower.starts_with(r"syswow64\") {
        s = format!(r"{root}\{s}");
    }
    while s.contains(r"\\") && !s.starts_with(r"\\") { s = s.replace(r"\\", "\\"); }
    s.to_lowercase()
}

/// The program a command line starts (quoted or not, with or without arguments), normalized.
/// Unquoted paths with spaces are cut after the first `.exe` that ends a token.
pub fn command_exe(cmd: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let sysroot = lookup("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let s = expand_env(cmd.trim(), lookup);
    let s = s.trim();
    let exe = if let Some(q) = s.strip_prefix('"') {
        q.split('"').next().unwrap_or(q).to_string()
    } else {
        let lower = s.to_ascii_lowercase();
        let mut cut = None;
        let mut from = 0;
        while let Some(i) = lower[from..].find(".exe") {
            let end = from + i + 4;
            if end == s.len() || s[end..].starts_with([' ', '\t', '"', ',']) { cut = Some(end); break; }
            from = end;
        }
        match cut {
            Some(end) => s[..end].to_string(),
            None => s.split_whitespace().next().unwrap_or("").to_string(),
        }
    };
    let exe = exe.trim();
    if exe.is_empty() { None } else { Some(normalize_path(exe, &sysroot)) }
}

/// `%LOCALAPPDATA%\keel` (known-folder API; see `keel_core::paths`).
pub use keel_core::paths::user_data_dir;

/// The lowercase file name of a normalized path.
pub fn file_name(norm: &str) -> &str { norm.rsplit('\\').next().unwrap_or(norm) }

/// `StartupApproved` binary value: first byte even = enabled (02/06), odd = disabled (03/07).
pub fn startup_approved_enabled(bytes: &[u8]) -> Option<bool> { bytes.first().map(|b| b & 1 == 0) }

/// One `Exec` action of a scheduled task, from `schtasks /query /xml ONE`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskAction {
    pub task: String,
    pub command: String,
    pub arguments: String,
    pub enabled: bool,
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn tag<'a>(s: &'a str, name: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut v = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        v.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    v
}

/// Parses the concatenated task XML: each `<Task>` is preceded by a `<!-- \path\name -->` comment.
/// A task is disabled when its `<Settings>` has `<Enabled>false</Enabled>`.
pub fn parse_tasks_xml(xml: &str) -> Vec<TaskAction> {
    let mut out = Vec::new();
    for chunk in xml.split("<!-- ").skip(1) {
        let Some(end) = chunk.find("-->") else { continue };
        let task = chunk[..end].trim().to_string();
        let body = &chunk[end..];
        let enabled = !tag(body, "Settings").iter().any(|s| s.contains("<Enabled>false</Enabled>"));
        for exec in tag(body, "Exec") {
            let command = tag(exec, "Command").first().map(|c| xml_unescape(c.trim())).unwrap_or_default();
            let arguments = tag(exec, "Arguments").first().map(|c| xml_unescape(c.trim())).unwrap_or_default();
            if !command.is_empty() { out.push(TaskAction { task: task.clone(), command, arguments, enabled }); }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(n: &str) -> Option<String> {
        match n.to_ascii_lowercase().as_str() {
            "systemroot" | "windir" => Some(r"C:\WINDOWS".into()),
            "programfiles" => Some(r"C:\Program Files".into()),
            "localappdata" => Some(r"C:\Users\me\AppData\Local".into()),
            _ => None,
        }
    }

    #[test]
    fn expands_known_and_keeps_unknown() {
        assert_eq!(expand_env(r"%SystemRoot%\system32\x.exe", &env), r"C:\WINDOWS\system32\x.exe");
        assert_eq!(expand_env("%NOPE%\\a %ProgramFiles%", &env), r"%NOPE%\a C:\Program Files");
        assert_eq!(expand_env("100% sure", &env), "100% sure");
        assert_eq!(expand_env("%%", &env), "%%");
    }

    #[test]
    fn normalizes_nt_and_relative_paths() {
        assert_eq!(normalize_path(r"\SystemRoot\System32\drivers\x.sys", r"C:\Windows"), r"c:\windows\system32\drivers\x.sys");
        assert_eq!(normalize_path(r"\??\C:\Windows\foo.sys", r"C:\Windows"), r"c:\windows\foo.sys");
        assert_eq!(normalize_path(r"system32\svchost.exe", r"C:\Windows\"), r"c:\windows\system32\svchost.exe");
        assert_eq!(normalize_path("C:/Program Files//A/b.exe", r"C:\Windows"), r"c:\program files\a\b.exe");
    }

    #[test]
    fn command_exe_handles_quotes_args_and_spaces() {
        let c = |s| command_exe(s, &env);
        assert_eq!(c(r#""C:\Program Files\Foo\foo.exe" --minimized"#).unwrap(), r"c:\program files\foo\foo.exe");
        assert_eq!(c(r"C:\Program Files (x86)\Bar\bar.exe /background").unwrap(), r"c:\program files (x86)\bar\bar.exe");
        assert_eq!(c(r"%ProgramFiles%\Baz\baz.exe").unwrap(), r"c:\program files\baz\baz.exe");
        assert_eq!(c(r"%SystemRoot%\system32\svchost.exe -k netsvcs -p").unwrap(), r"c:\windows\system32\svchost.exe");
        assert_eq!(c(r"system32\svchost.exe -k LocalServiceNoNetwork").unwrap(), r"c:\windows\system32\svchost.exe");
        assert_eq!(c(r"C:\Apps\my.exec\run.exe -x").unwrap(), r"c:\apps\my.exec\run.exe");
        assert_eq!(c(r#"C:\Tools\t.exe,"arg""#).unwrap(), r"c:\tools\t.exe");
        assert_eq!(c("rundll32 shell32.dll,Foo").unwrap(), "rundll32");
        assert_eq!(c("   "), None);
    }

    #[test]
    fn file_name_and_startup_approved() {
        assert_eq!(file_name(r"c:\a\b.exe"), "b.exe");
        assert_eq!(file_name("b.exe"), "b.exe");
        assert_eq!(startup_approved_enabled(&[2, 0, 0]), Some(true));
        assert_eq!(startup_approved_enabled(&[3, 0, 0]), Some(false));
        assert_eq!(startup_approved_enabled(&[]), None);
    }

    #[test]
    fn parses_task_xml() {
        let xml = r#"
<Tasks>
<!-- \Vendor\Updater -->
<Task version="1.2">
  <Settings><Enabled>false</Enabled><IdleSettings><StopOnIdleEnd>true</StopOnIdleEnd></IdleSettings></Settings>
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Actions Context="Author">
    <Exec><Command>"C:\Program Files\V\up.exe"</Command><Arguments>/silent &amp; /q</Arguments></Exec>
    <ComHandler><ClassId>{X}</ClassId></ComHandler>
  </Actions>
</Task>
<!-- \Other -->
<Task><Actions><Exec><Command>%windir%\system32\o.exe</Command></Exec></Actions></Task>
</Tasks>"#;
        let t = parse_tasks_xml(xml);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], TaskAction { task: r"\Vendor\Updater".into(), command: r#""C:\Program Files\V\up.exe""#.into(), arguments: "/silent & /q".into(), enabled: false });
        assert!(t[1].enabled);
        assert_eq!(command_exe(&t[1].command, &env).unwrap(), r"c:\windows\system32\o.exe");
    }
}
