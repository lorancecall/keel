//! The service's control pipe `\\.\pipe\keel`: one JSON request line, one JSON reply line.
//! A request is `{"feature": "<name>", "body": <feature request>}`; the server hands the body to
//! the handler registered for that feature. SYSTEM and administrators get full access, interactive
//! users read / write (the tray, the main window and the CLI).
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::*;
use windows::core::{HSTRING, w};

pub const PATH: &str = r"\\.\pipe\keel";
const SDDL: windows::core::PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

/// A feature's handler: request body in, reply out (runs on the pipe thread).
pub type Handler = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// The request line for `feature`.
pub fn request_line(feature: &str, body: &Value) -> String {
    serde_json::json!({ "feature": feature, "body": body }).to_string()
}

/// The reply for one request line (pure routing; tested).
pub fn route(handlers: &HashMap<String, Handler>, line: &str) -> Value {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "ok": false, "message": format!("bad request: {e}") }),
    };
    let feature = v.get("feature").and_then(Value::as_str).unwrap_or("");
    match handlers.get(feature) {
        Some(h) => h(v.get("body").cloned().unwrap_or(Value::Null)),
        None => serde_json::json!({ "ok": false, "message": format!("no feature {feature:?} in this service") }),
    }
}

/// Serves requests forever on its own thread. One request at a time: features are quick to
/// answer or hand slow work to their own threads.
pub fn serve(handlers: HashMap<String, Handler>, log: fn(String)) {
    std::thread::spawn(move || loop {
        if let Err(e) = serve_one(&handlers) {
            log(format!("pipe: {e:#}"));
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

fn serve_one(handlers: &HashMap<String, Handler>) -> Result<()> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None)?; }
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
    let h = unsafe {
        CreateNamedPipeW(&HSTRING::from(PATH), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES, 64 * 1024, 64 * 1024, 0, Some(&sa))
    };
    unsafe { let _ = LocalFree(Some(HLOCAL(sd.0))); }
    if h.is_invalid() { anyhow::bail!("CreateNamedPipeW failed"); }
    // ERROR_PIPE_CONNECTED (client came first) is fine too.
    if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
        if e.code().0 as u32 != 0x8007_0217 { unsafe { let _ = CloseHandle(h); } return Err(e.into()); }
    }
    let file = unsafe { std::fs::File::from_raw_handle(h.0) };
    let mut line = String::new();
    BufReader::new(&file).read_line(&mut line)?;
    let reply = route(handlers, &line);
    let mut w = &file;
    writeln!(w, "{reply}")?;
    w.flush()?;
    unsafe { let _ = FlushFileBuffers(HANDLE(h.0)); let _ = DisconnectNamedPipe(h); }
    Ok(())
}

/// Client side: sends one request to the service and returns its reply.
/// The service serves one request at a time; while it answers another client (the tray asks every
/// few seconds) or sets up the next pipe instance, opening fails with "busy" (231) or "not found"
/// (2). Those are retried for up to 3 s, so the main window does not report a running service as
/// stopped.
pub fn call(feature: &str, body: &Value) -> Result<Value> { call_wait(feature, body, std::time::Duration::from_secs(3)) }

/// `call` with a longer wait for a busy service (background work that queues behind slow requests).
pub fn call_wait(feature: &str, body: &Value, wait: std::time::Duration) -> Result<Value> {
    let start = std::time::Instant::now();
    let f = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(PATH) {
            Ok(f) => break f,
            Err(e) if matches!(e.raw_os_error(), Some(2 | 231)) && start.elapsed() < wait => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::Error::new(e).context("the Keel service is not running")),
        }
    };
    let mut w = &f;
    writeln!(w, "{}", request_line(feature, body))?;
    let mut line = String::new();
    BufReader::new(&f).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_feature() {
        let mut h: HashMap<String, Handler> = HashMap::new();
        h.insert("ram".into(), Arc::new(|b| serde_json::json!({ "echo": b })));
        let r = route(&h, &request_line("ram", &serde_json::json!({ "x": 1 })));
        assert_eq!(r["echo"]["x"], 1);
        assert_eq!(route(&h, &request_line("net", &Value::Null))["ok"], false);
        assert_eq!(route(&h, "not json")["ok"], false);
    }
}
