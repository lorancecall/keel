//! The memory feature on the service's control pipe (`keel_core::pipe`, feature "ram"): on-demand
//! requests go to the running daemon loop, which answers with its state.
use crate::ondemand::{Reply, Request};
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

pub const FEATURE: &str = "ram";

/// The daemon loop's sender; the service replaces it each time the loop (re)starts.
pub type Slot = Arc<Mutex<Option<Sender<crate::daemon::Msg>>>>;

fn answer(slot: &Slot, body: Value) -> Result<Reply> {
    let req: Request = serde_json::from_value(body).context("bad request")?;
    let tx = slot.lock().unwrap().clone().context("the memory feature is not running")?;
    let (rtx, rrx) = channel();
    tx.send(crate::daemon::Msg::Req(req, rtx)).ok().context("the memory feature is not running")?;
    rrx.recv().context("no reply")
}

/// The pipe handler for this feature.
pub fn handler(slot: Slot) -> keel_core::pipe::Handler {
    Arc::new(move |body| {
        let r = answer(&slot, body).unwrap_or_else(|e| Reply { ok: false, message: format!("{e:#}"), state: Default::default(), failed: vec![] });
        serde_json::to_value(r).unwrap_or(Value::Null)
    })
}

/// Serves the pipe with only this feature (`keel ram daemon`, run without the service).
pub fn serve(tx: Sender<crate::daemon::Msg>) {
    let mut h = std::collections::HashMap::new();
    h.insert(FEATURE.to_string(), handler(Arc::new(Mutex::new(Some(tx)))));
    keel_core::pipe::serve(h, |m| crate::daemon::log(m));
}

/// Client side: sends one request to the service.
pub fn call(req: &Request) -> Result<Reply> {
    let v = keel_core::pipe::call(FEATURE, &serde_json::to_value(req)?)?;
    Ok(serde_json::from_value(v)?)
}

/// `call` that waits up to `wait` for a busy service (each new family takes the service a scan).
pub fn call_wait(req: &Request, wait: std::time::Duration) -> Result<Reply> {
    let v = keel_core::pipe::call_wait(FEATURE, &serde_json::to_value(req)?, wait)?;
    Ok(serde_json::from_value(v)?)
}
