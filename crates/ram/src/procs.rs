//! Process snapshot via `NtQuerySystemInformation(SystemProcessInformation)`: memory, CPU time,
//! parent and session for every process without opening any of them (works unelevated).
//! Exe paths come from `QueryFullProcessImageNameW`, else `SystemProcessIdInformation` (NT path).
use std::collections::HashMap;
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::Foundation::{CloseHandle, UNICODE_STRING};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::core::{HSTRING, PWSTR};

const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const SYSTEM_PROCESS_ID_INFORMATION: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(88);

/// Leading part of the native SYSTEM_PROCESS_INFORMATION (x64 layout); the windows crate only
/// exposes the reserved-field version.
#[repr(C)]
struct RawProc {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UNICODE_STRING,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub working_set: u64,
    pub private_ws: u64,
    pub private_bytes: u64,
    pub cpu_100ns: u64,
    pub create_time: i64,
    pub threads: u32,
    pub handles: u32,
    pub session: u32,
}

fn ustr(u: &UNICODE_STRING) -> String {
    if u.Buffer.is_null() || u.Length == 0 { return String::new(); }
    let s = unsafe { std::slice::from_raw_parts(u.Buffer.0, u.Length as usize / 2) };
    String::from_utf16_lossy(s)
}

/// Every process now. Pid 0 is named "Idle".
pub fn snapshot() -> anyhow::Result<Vec<Proc>> {
    let mut buf: Vec<u64> = vec![0; 256 * 1024 / 8];
    loop {
        let mut need = 0u32;
        let st = unsafe { NtQuerySystemInformation(SystemProcessInformation, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need) };
        if st.0 == STATUS_INFO_LENGTH_MISMATCH {
            buf = vec![0; (need as usize + 64 * 1024) / 8 + 1];
            continue;
        }
        if st.0 < 0 { anyhow::bail!("NtQuerySystemInformation failed: 0x{:08X}", st.0); }
        break;
    }
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    let mut out = Vec::new();
    loop {
        let p = unsafe { &*(base.add(off) as *const RawProc) };
        let pid = p.unique_process_id as u32;
        let name = if pid == 0 { "Idle".to_string() } else { ustr(&p.image_name) };
        out.push(Proc {
            pid,
            ppid: p.inherited_from_unique_process_id as u32,
            name,
            working_set: p.working_set_size as u64,
            private_ws: p.working_set_private_size.max(0) as u64,
            private_bytes: p.private_page_count as u64,
            cpu_100ns: (p.user_time + p.kernel_time).max(0) as u64,
            create_time: p.create_time,
            threads: p.number_of_threads,
            handles: p.handle_count,
            session: p.session_id,
        });
        if p.next_entry_offset == 0 { break; }
        off += p.next_entry_offset as usize;
    }
    Ok(out)
}

/// `\Device\HarddiskVolumeN` -> `C:` for every mounted drive letter.
pub fn device_map() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for l in b'A'..=b'Z' {
        let drive = format!("{}:", l as char);
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(&HSTRING::from(drive.as_str()), Some(&mut buf)) };
        if n > 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            v.push((String::from_utf16_lossy(&buf[..end]), drive));
        }
    }
    v
}

/// Turns an NT device path into a drive-letter path when a mapping matches.
pub fn nt_to_dos(nt: &str, map: &[(String, String)]) -> String {
    for (dev, drive) in map {
        if let Some(rest) = nt.strip_prefix(dev.as_str()) {
            if rest.starts_with('\\') { return format!("{drive}{rest}"); }
        }
    }
    nt.to_string()
}

fn query_full_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn nt_image_path(pid: u32) -> Option<String> {
    #[repr(C)]
    struct IdInfo { pid: usize, name: UNICODE_STRING }
    let mut buf = vec![0u16; 1024];
    let mut info = IdInfo { pid: pid as usize, name: UNICODE_STRING { Length: 0, MaximumLength: (buf.len() * 2) as u16, Buffer: PWSTR(buf.as_mut_ptr()) } };
    let st = unsafe { NtQuerySystemInformation(SYSTEM_PROCESS_ID_INFORMATION, (&mut info as *mut IdInfo).cast(), size_of::<IdInfo>() as u32, std::ptr::null_mut()) };
    if st.0 < 0 { return None; }
    let s = String::from_utf16_lossy(&buf[..info.name.Length as usize / 2]);
    if s.is_empty() { None } else { Some(s) }
}

/// Where a pid's exe lives and how we learned it: "query" (opened), "nt" (kernel name), or none.
pub fn image_path(pid: u32, map: &[(String, String)]) -> (Option<String>, &'static str) {
    if pid <= 4 { return (None, "kernel"); }
    if let Some(p) = query_full_path(pid) { return (Some(p), "query"); }
    match nt_image_path(pid) {
        Some(p) => (Some(nt_to_dos(&p, map)), "nt"),
        None => (None, "denied"),
    }
}

/// Pid -> process, for parent lookups.
pub fn by_pid(v: &[Proc]) -> HashMap<u32, &Proc> { v.iter().map(|p| (p.pid, p)).collect() }

#[cfg(test)]
mod tests {
    use super::nt_to_dos;

    #[test]
    fn maps_nt_device_paths() {
        let m = vec![(r"\Device\HarddiskVolume3".to_string(), "C:".to_string()), (r"\Device\HarddiskVolume1".to_string(), "D:".to_string())];
        assert_eq!(nt_to_dos(r"\Device\HarddiskVolume3\Windows\x.exe", &m), r"C:\Windows\x.exe");
        assert_eq!(nt_to_dos(r"\Device\HarddiskVolume1\a.exe", &m), r"D:\a.exe");
        assert_eq!(nt_to_dos(r"\Device\HarddiskVolume12\a.exe", &m), r"\Device\HarddiskVolume12\a.exe");
    }
}
