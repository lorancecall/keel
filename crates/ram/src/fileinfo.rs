//! File facts used for grouping: the version resource's CompanyName and folder sizes (read-only).
use std::path::Path;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::core::HSTRING;

fn ver_string(block: &[u8], sub: &str) -> Option<String> {
    let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let ok = unsafe { VerQueryValueW(block.as_ptr().cast(), &HSTRING::from(sub), &mut p, &mut len) }.as_bool();
    if !ok || p.is_null() || len == 0 { return None; }
    let s = unsafe { std::slice::from_raw_parts(p as *const u16, len as usize) };
    let s = String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]);
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `CompanyName` from the file's version resource (first translation, then en-US / Unicode).
pub fn company_name(path: &str) -> Option<String> {
    let h = HSTRING::from(path);
    let size = unsafe { GetFileVersionInfoSizeW(&h, None) };
    if size == 0 { return None; }
    let mut block = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&h, None, size, block.as_mut_ptr().cast()).ok()? };
    let mut langs = Vec::new();
    let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    if unsafe { VerQueryValueW(block.as_ptr().cast(), &HSTRING::from(r"\VarFileInfo\Translation"), &mut p, &mut len) }.as_bool() && len >= 4 {
        let t = unsafe { std::slice::from_raw_parts(p as *const u16, len as usize / 2) };
        for c in t.chunks_exact(2) { langs.push(format!("{:04x}{:04x}", c[0], c[1])); }
    }
    langs.extend(["040904b0".to_string(), "040904e4".to_string()]);
    langs.iter().find_map(|l| ver_string(&block, &format!(r"\StringFileInfo\{l}\CompanyName")))
}

/// Total bytes of all files under `dir` (reparse points such as junctions not followed; unreadable
/// parts skipped), and whether the walk finished before `deadline` (else the total is a lower bound).
pub fn dir_size(dir: &Path, deadline: std::time::Instant) -> (u64, bool) {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if std::time::Instant::now() > deadline { return (total, false); }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.file_attributes() & REPARSE_POINT != 0 { continue; }
            if m.is_dir() { stack.push(e.path()); } else { total += m.len(); }
        }
    }
    (total, true)
}
