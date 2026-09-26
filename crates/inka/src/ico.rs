//! Build a Windows `.ico` from an icon set.
//!
//! Vendored and adapted from Deno's `convert_icon_set_to_ico` in
//! `cli/tools/desktop.rs` (MIT, Copyright (c) the Deno authors). Each source
//! image is stored verbatim as an ICO directory entry (Vista+ supports
//! PNG-compressed entries), which is what `libsui` / the `image` crate expect.

use std::fs;
use std::path::{Path, PathBuf};

/// Write an `.ico` at `ico_path` from `(path, size)` entries. Entries whose file
/// cannot be read are skipped; `Err` if no image remains.
// Only the Windows packager builds a `.ico`; tests exercise it elsewhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn convert_icon_set_to_ico(
    entries: &[(PathBuf, u32)],
    ico_path: &Path,
) -> Result<(), String> {
    let mut images: Vec<(u32, Vec<u8>)> = Vec::new();
    for (path, size) in entries {
        match fs::read(path) {
            Ok(bytes) => images.push((*size, bytes)),
            Err(_) => continue,
        }
    }
    if images.is_empty() {
        return Err("no readable icon images for the .ico".to_string());
    }
    if images.len() > u16::MAX as usize {
        return Err("too many icon images for a single .ico".to_string());
    }

    let count = images.len() as u16;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6u32 + 16 * u32::from(count);
    for (size, bytes) in &images {
        // 256 is encoded as 0 in the ICO directory.
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // palette count
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset = offset.saturating_add(bytes.len() as u32);
    }
    for (_, bytes) in &images {
        out.extend_from_slice(bytes);
    }
    fs::write(ico_path, out).map_err(|e| format!("cannot write {}: {e}", ico_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("inka-ico-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ico_has_header_directory_and_data() {
        let dir = scratch("set");
        let a = dir.join("a.png");
        let b = dir.join("b.png");
        fs::write(&a, b"aaaa").unwrap();
        fs::write(&b, b"bbbbbb").unwrap();
        let ico = dir.join("out.ico");
        convert_icon_set_to_ico(&[(a, 16), (b, 256)], &ico).unwrap();

        let bytes = fs::read(&ico).unwrap();
        assert_eq!(&bytes[0..2], &[0, 0], "reserved");
        assert_eq!(&bytes[2..4], &[1, 0], "type icon");
        assert_eq!(&bytes[4..6], &[2, 0], "two images");
        // Entry 0: 16x16, length 4, offset 6 + 2*16 = 38.
        assert_eq!(bytes[6], 16);
        assert_eq!(bytes[7], 16);
        assert_eq!(&bytes[14..18], &4u32.to_le_bytes());
        assert_eq!(&bytes[18..22], &38u32.to_le_bytes());
        // Entry 1: 256 -> 0 dim, length 6, offset 42.
        assert_eq!(bytes[22], 0);
        assert_eq!(bytes[23], 0);
        assert_eq!(&bytes[30..34], &6u32.to_le_bytes());
        assert_eq!(&bytes[34..38], &42u32.to_le_bytes());
        // Data.
        assert_eq!(&bytes[38..42], b"aaaa");
        assert_eq!(&bytes[42..48], b"bbbbbb");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ico_errors_without_readable_images() {
        let dir = scratch("empty");
        let ico = dir.join("out.ico");
        assert!(convert_icon_set_to_ico(&[], &ico).is_err());
        assert!(convert_icon_set_to_ico(&[(dir.join("missing.png"), 16)], &ico).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ico_skips_missing_but_uses_present() {
        let dir = scratch("skip");
        let a = dir.join("a.png");
        fs::write(&a, b"x").unwrap();
        let ico = dir.join("out.ico");
        convert_icon_set_to_ico(&[(dir.join("missing.png"), 32), (a, 48)], &ico).unwrap();
        let bytes = fs::read(&ico).unwrap();
        assert_eq!(&bytes[4..6], &[1, 0], "one surviving image");
        let _ = fs::remove_dir_all(&dir);
    }
}
