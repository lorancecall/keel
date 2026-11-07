//! Kernel pool usage by tag (`NtQuerySystemInformation(SystemPoolTagInformation)`, no admin).
//! Driver memory belongs to no process, so a driver leak never shows in process samples; the
//! tags do (e.g. `DAL3` = AMD display driver, amdkmdag.sys).
use serde::{Deserialize, Serialize};
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};

const SYSTEM_POOL_TAG_INFORMATION: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(22);
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
/// x64 SYSTEM_POOLTAG: Tag[4], PagedAllocs, PagedFrees, PagedUsed (8), NonPagedAllocs,
/// NonPagedFrees, NonPagedUsed (8) = 40 bytes; the array starts after Count (+4 padding).
const ROW: usize = 40;

/// One tag's bytes in use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    pub tag: String,
    pub paged: u64,
    pub nonpaged: u64,
}

/// Tags whose owner is known on this machine (the rest show as their tag).
pub fn owner(tag: &str) -> Option<&'static str> {
    match tag {
        t if t.starts_with("DAL") => Some("AMD display driver (amdkmdag.sys)"),
        t if t.starts_with("Amd") => Some("AMD driver"),
        "MmSt" | "MmRe" | "MmCa" => Some("memory manager"),
        "FMfn" | "FMsc" => Some("filter manager (file system minifilters)"),
        "File" | "Ntff" => Some("NTFS / file objects"),
        "EtwB" => Some("ETW trace buffers"),
        "CM25" | "CM31" | "CM16" => Some("registry"),
        _ => None,
    }
}

/// Parses the raw SystemPoolTagInformation buffer (pure).
pub fn parse(buf: &[u8]) -> Vec<Tag> {
    if buf.len() < 8 { return vec![]; }
    let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let u64at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    (0..count).filter_map(|i| {
        let o = 8 + i * ROW;
        if o + ROW > buf.len() { return None; }
        let tag: String = buf[o..o + 4].iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect();
        Some(Tag { tag: tag.trim_end().to_string(), paged: u64at(o + 16), nonpaged: u64at(o + 32) })
    }).collect()
}

/// All tags now, or empty when the call is refused.
pub fn tags() -> Vec<Tag> {
    let mut buf: Vec<u64> = vec![0; 256 * 1024 / 8];
    loop {
        let mut need = 0u32;
        let st = unsafe { NtQuerySystemInformation(SYSTEM_POOL_TAG_INFORMATION, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need) };
        if st.0 == STATUS_INFO_LENGTH_MISMATCH && (need as usize) > buf.len() * 8 { buf = vec![0; need as usize / 8 + 1024]; continue; }
        if st.0 < 0 { return vec![]; }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        return parse(bytes);
    }
}

/// Tags using at least `min` bytes (paged or nonpaged), largest first, plus totals as `*NP` / `*P`.
pub fn top(min: u64) -> Vec<Tag> {
    let all = tags();
    if all.is_empty() { return all; }
    let (p, np) = all.iter().fold((0, 0), |a, t| (a.0 + t.paged, a.1 + t.nonpaged));
    let mut v: Vec<Tag> = all.into_iter().filter(|t| t.paged >= min || t.nonpaged >= min).collect();
    v.sort_by(|a, b| (b.paged + b.nonpaged).cmp(&(a.paged + a.nonpaged)));
    v.insert(0, Tag { tag: "*ALL".into(), paged: p, nonpaged: np });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pool_tag_rows() {
        let mut b = vec![0u8; 8 + 2 * ROW];
        b[0] = 2;
        b[8..12].copy_from_slice(b"DAL3");
        b[8 + 32..8 + 40].copy_from_slice(&(420u64 << 20).to_le_bytes());
        let o = 8 + ROW;
        b[o..o + 4].copy_from_slice(b"FMfn");
        b[o + 16..o + 24].copy_from_slice(&(150u64 << 20).to_le_bytes());
        let v = parse(&b);
        assert_eq!(v, vec![Tag { tag: "DAL3".into(), paged: 0, nonpaged: 420 << 20 }, Tag { tag: "FMfn".into(), paged: 150 << 20, nonpaged: 0 }]);
        assert_eq!(owner("DAL3"), Some("AMD display driver (amdkmdag.sys)"));
        assert!(parse(&[1, 0, 0, 0, 0, 0, 0, 0]).is_empty(), "truncated buffer");
    }
}
