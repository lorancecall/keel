//! Process start / stop events from WMI (`Win32_ProcessStartTrace` / `Win32_ProcessStopTrace`),
//! each on its own thread blocked in `IEnumWbemClassObject::Next`: event-driven, no polling.
//! Needs admin (the service runs as LocalSystem).
use std::sync::mpsc::Sender;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Wmi::*;
use windows::core::{w, BSTR};

#[derive(Debug, Clone)]
pub enum ProcEvent { Start(u32), Stop(u32) }

fn u32_of(v: &VARIANT) -> Option<u32> {
    unsafe {
        let a = &v.Anonymous.Anonymous;
        match a.vt.0 { 3 => Some(a.Anonymous.lVal as u32), 19 => Some(a.Anonymous.ulVal), _ => None } // VT_I4, VT_UI4
    }
}

fn listen(class: &str, start: bool, tx: Sender<crate::daemon::Msg>) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let svc = loc.ConnectServer(&BSTR::from(r"ROOT\CIMV2"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
        CoSetProxyBlanket(&svc, 10, 0, None, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?; // 10 = RPC_C_AUTHN_WINNT
        let q = BSTR::from(format!("SELECT ProcessID FROM {class}"));
        let en = svc.ExecNotificationQuery(&BSTR::from("WQL"), &q, WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY, None)?;
        loop {
            let mut objs = [None];
            let mut n = 0u32;
            let hr = en.Next(WBEM_INFINITE, &mut objs, &mut n);
            if hr.is_err() { return Err(hr.into()); }
            let Some(o) = objs[0].take() else { continue };
            let mut v = VARIANT::default();
            if o.Get(w!("ProcessID"), 0, &mut v, None, None).is_ok() {
                if let Some(pid) = u32_of(&v) {
                    let ev = if start { ProcEvent::Start(pid) } else { ProcEvent::Stop(pid) };
                    if tx.send(crate::daemon::Msg::Proc(ev)).is_err() { return Ok(()); }
                }
            }
        }
    }
}

/// Starts both listener threads; failures (e.g. not elevated) are logged and following is off.
pub fn spawn(tx: Sender<crate::daemon::Msg>) {
    for (class, start) in [("Win32_ProcessStartTrace", true), ("Win32_ProcessStopTrace", false)] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = listen(class, start, tx) { crate::daemon::log(format!("process events ({class}) unavailable: {e}")); }
        });
    }
}
