//! The user's app icon: Keel's badge recoloured at run time (Settings > App icon). Three styles
//! from four 128 px alpha masks made by `assets/make_icon.py` (`icon-masks.a8`):
//! filled (coloured badge, white boat) and boat only (the fourth, a ring mask, is unused). The
//! main window sets it as its window icon; the tray can use the boat in the same colour. Pure.

const MASKS: &[u8] = include_bytes!("../../../assets/icon-masks.a8");
const N: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style { Filled, Boat }

impl Style {
    /// Settings index: 0 filled, 1 boat only (2, the old "boat only", still reads as boat).
    pub fn from_index(i: u8) -> Style { if i == 0 { Style::Filled } else { Style::Boat } }
}

/// Colour choices offered in Settings (name, hex); the first is the built-in dark badge.
pub const COLORS: &[(&str, &str)] = &[
    ("Graphite", "#1f1f1f"), ("Blue", "#0f6cbd"), ("Teal", "#0e7c86"), ("Green", "#2e7d32"),
    ("Orange", "#c2410c"), ("Red", "#c42b1c"), ("Purple", "#6b4fbb"),
];

/// `#rrggbb` -> (r, g, b).
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) { return None; }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((v(0)?, v(2)?, v(4)?))
}

fn mask(i: usize) -> &'static [u8] { &MASKS[i * N * N..(i + 1) * N * N] }

/// Area-averaged sample of a 128 px mask for pixel (x, y) of a `size` px image.
fn sample(m: &[u8], size: usize, x: usize, y: usize) -> f32 {
    let (x0, x1) = (x * N / size, ((x + 1) * N).div_ceil(size).min(N));
    let (y0, y1) = (y * N / size, ((y + 1) * N).div_ceil(size).min(N));
    let mut sum = 0u32;
    for yy in y0..y1 { for xx in x0..x1 { sum += m[yy * N + xx] as u32; } }
    sum as f32 / (((x1 - x0) * (y1 - y0)).max(1) as f32 * 255.0)
}

/// Straight (not premultiplied) RGBA pixels, `size` x `size`.
pub fn render(style: Style, rgb: (u8, u8, u8), size: usize) -> Vec<u8> {
    let (badge, holes, boat) = (mask(0), mask(1), mask(3));
    let mut out = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            // Two layers: the colour, then white on top (filled style's boat).
            let (c, w) = match style {
                Style::Filled => (sample(badge, size, x, y), sample(holes, size, x, y)),
                Style::Boat => (sample(boat, size, x, y), 0.0),
            };
            let a = (c + w).min(1.0);
            let px = &mut out[(y * size + x) * 4..][..4];
            if a < 0.03 { continue; } // mask noise outside the badge
            let mix = |ch: u8| ((ch as f32 * c + 255.0 * w) / a).round().min(255.0) as u8;
            px.copy_from_slice(&[mix(rgb.0), mix(rgb.1), mix(rgb.2), (a * 255.0).round() as u8]);
        }
    }
    out
}

/// Frame sizes of the .ico for shortcuts (Start menu, desktop, taskbar pins).
const ICO_SIZES: [usize; 6] = [16, 24, 32, 48, 64, 256];

/// A Windows .ico file of the icon (32-bit frames with alpha), for shortcuts.
pub fn ico_bytes(style: Style, rgb: (u8, u8, u8)) -> Vec<u8> {
    let frames: Vec<Vec<u8>> = ICO_SIZES.iter().map(|&n| {
        let px = render(style, rgb, n);
        let mask_row = n.div_ceil(32) * 4;
        let mut b = Vec::with_capacity(40 + n * n * 4 + mask_row * n);
        // BITMAPINFOHEADER: height counts the colour and the mask halves.
        for v in [40u32, n as u32, 2 * n as u32] { b.extend(v.to_le_bytes()); }
        b.extend(1u16.to_le_bytes());
        b.extend(32u16.to_le_bytes());
        for v in [0u32, (n * n * 4 + mask_row * n) as u32, 0, 0, 0, 0] { b.extend(v.to_le_bytes()); }
        // Bottom-up BGRA rows, then an all-zero mask (alpha decides).
        for y in (0..n).rev() {
            for x in 0..n { let p = &px[(y * n + x) * 4..][..4]; b.extend([p[2], p[1], p[0], p[3]]); }
        }
        b.extend(std::iter::repeat_n(0u8, mask_row * n));
        b
    }).collect();
    let mut out = Vec::new();
    out.extend([0u8, 0, 1, 0]);
    out.extend((ICO_SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * ICO_SIZES.len();
    for (n, f) in ICO_SIZES.iter().zip(&frames) {
        let d = if *n >= 256 { 0 } else { *n as u8 };
        out.extend([d, d, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((f.len() as u32).to_le_bytes());
        out.extend((offset as u32).to_le_bytes());
        offset += f.len();
    }
    for f in frames { out.extend(f); }
    out
}

/// Points every shortcut (.lnk) in `dirs` that starts `target` at `icon` (index 0), or back at the
/// program's own icon when `icon` is None. Returns the shortcuts changed. Needs rights to write
/// those shortcuts (the all-users Start menu and desktop: the service).
pub fn retarget_shortcuts(dirs: &[std::path::PathBuf], target: &std::path::Path, icon: Option<&std::path::Path>) -> anyhow::Result<Vec<String>> {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, IPersistFile, STGM_READWRITE};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    use windows::core::{Interface, HSTRING, PCWSTR};
    let mut done = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        for d in dirs {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) { continue; }
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                let file: IPersistFile = link.cast()?;
                if file.Load(&HSTRING::from(p.as_os_str()), STGM_READWRITE).is_err() { continue; }
                let mut buf = [0u16; 520];
                if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_err() { continue; }
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                let path = String::from_utf16_lossy(&buf[..end]);
                if !std::path::Path::new(&path).eq(target) && !path.eq_ignore_ascii_case(&target.to_string_lossy()) { continue; }
                let loc = icon.unwrap_or(target);
                link.SetIconLocation(&HSTRING::from(loc.as_os_str()), 0)?;
                file.Save(PCWSTR::null(), true)?;
                done.push(p.display().to_string());
            }
        }
        if !done.is_empty() { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None); }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a shortcut made by hand: KEEL_TEST_LNK_DIR with a .lnk to KEEL_TEST_LNK_TARGET.
    #[test]
    #[ignore]
    fn retargets_a_shortcut() {
        let (Some(d), Some(t)) = (std::env::var_os("KEEL_TEST_LNK_DIR"), std::env::var_os("KEEL_TEST_LNK_TARGET")) else { return };
        let ico = std::path::PathBuf::from(&d).join("test.ico");
        std::fs::write(&ico, ico_bytes(Style::Boat, (200, 0, 0))).unwrap();
        let done = retarget_shortcuts(&[d.into()], std::path::Path::new(&t), Some(&ico)).unwrap();
        assert_eq!(done.len(), 1, "{done:?}");
    }

    #[test]
    fn ico_has_a_directory_and_its_frames() {
        let ico = ico_bytes(Style::Filled, (15, 108, 189));
        assert_eq!(&ico[..6], &[0, 0, 1, 0, ICO_SIZES.len() as u8, 0]);
        // Last entry: 256 px written as 0, and its frame ends the file.
        let e = 6 + 16 * (ICO_SIZES.len() - 1);
        assert_eq!(ico[e], 0);
        let size = u32::from_le_bytes(ico[e + 8..e + 12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().unwrap()) as usize;
        assert_eq!(off + size, ico.len());
        assert_eq!(&ico[off..off + 4], &40u32.to_le_bytes(), "BITMAPINFOHEADER");
        let _ = std::fs::write(std::env::temp_dir().join("keel-test-icon.ico"), &ico); // to look at by hand
    }

    fn alpha_sum(v: &[u8]) -> u32 { v.chunks(4).map(|p| p[3] as u32).sum() }

    #[test]
    fn styles_cover_different_areas() {
        let red = (200, 0, 0);
        let (f, b) = (render(Style::Filled, red, 32), render(Style::Boat, red, 32));
        assert_eq!(f.len(), 32 * 32 * 4);
        assert!(alpha_sum(&f) > alpha_sum(&b) && alpha_sum(&b) > 0);
        // Filled: the corner is outside the rounded badge, the middle row has colour and white.
        assert_eq!(f[3], 0);
        assert!(f.chunks(4).any(|p| p[3] > 200 && p[0] > 240 && p[1] > 240), "white boat");
        assert!(f.chunks(4).any(|p| p[3] > 200 && p[0] == 200 && p[1] == 0), "red badge");
    }

    #[test]
    fn hex_colours() {
        assert_eq!(parse_hex("#0f6cbd"), Some((15, 108, 189)));
        assert_eq!(parse_hex("0F6CBD"), Some((15, 108, 189)));
        assert_eq!(parse_hex("#0f6cb"), None);
        assert!(COLORS.iter().all(|(_, h)| parse_hex(h).is_some()));
    }
}
