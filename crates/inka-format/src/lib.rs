//! The inka artifact format, shared by `inka build` (the writer), the launcher
//! (the reader), and `inka doctor <artifact>` (the inspector).
//!
//! An artifact is `[launcher bytes][archive][manifest][footer]`. The footer is
//! the last 24 bytes: an 8-byte magic plus two little-endian `u64` lengths
//! (archive, manifest). The archive is a sequence of
//! `{path_len u64}{data_len u64}{path}{data}` entries.
//!
//! Keeping encode and decode in one place removes the build/launcher copies
//! that could drift; the launcher and the CLI both depend on this crate.

use std::fmt;
use std::path::Path;

pub mod platform;

pub const FOOTER_LEN: usize = 24;
pub const MAGIC: &[u8; 8] = b"INKFOOT5";

// ---- version ---------------------------------------------------------------

/// A prerelease channel identifier. Ordering is prerelease < release, and
/// `beta` precedes `rc`; the numeric counter orders successive prereleases, so
/// `0.267.2-beta.9 < 0.267.2-beta.10 < 0.267.2-rc.1 < 0.267.2`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub enum Pre {
    Beta(u64),
    Rc(u64),
    #[default]
    Release,
}

impl fmt::Display for Pre {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pre::Beta(n) => write!(f, "-beta.{n}"),
            Pre::Rc(n) => write!(f, "-rc.{n}"),
            Pre::Release => Ok(()),
        }
    }
}

/// A `major.minor.patch` version with an optional prerelease (`-beta.N`/`-rc.N`).
/// Ordering is semver-like: a prerelease sorts below its release and above any
/// earlier patch, and the counter compares numerically.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Pre,
}

impl Version {
    /// A release version (`pre` = `Release`).
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Pre::Release,
        }
    }

    /// True for a prerelease (`-beta.N`/`-rc.N`).
    pub fn is_prerelease(&self) -> bool {
        !matches!(self.pre, Pre::Release)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}{}",
            self.major, self.minor, self.patch, self.pre
        )
    }
}

/// Parse `x`, `x.y`, or `x.y.z`, optionally suffixed `-beta.N` or `-rc.N`
/// (whitespace tolerated). Rejects a fourth numeric component and any other
/// suffix, so a value like `0.0.0-stub` still fails.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let (numeric, pre) = match s.split_once('-') {
        Some((num, tag)) => (num.trim(), parse_pre(tag.trim())?),
        None => (s, Pre::Release),
    };
    let mut parts = numeric.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next().unwrap_or("0").trim().parse().ok()?;
    let patch = parts.next().unwrap_or("0").trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
        pre,
    })
}

/// Parse a `beta.N`/`rc.N` prerelease tag (the text after the `-`).
fn parse_pre(tag: &str) -> Option<Pre> {
    let (label, num) = tag.split_once('.')?;
    let n: u64 = num.trim().parse().ok()?;
    match label {
        "beta" => Some(Pre::Beta(n)),
        "rc" => Some(Pre::Rc(n)),
        _ => None,
    }
}

// ---- archive + footer ------------------------------------------------------

/// Validate one archive path: relative, no NUL, no `..` or root/prefix
/// component. Component-based so a rooted path (`/abs`, `\abs`, `C:\abs`) is
/// rejected on every platform (Windows `Path::is_absolute` returns false for
/// `/abs`).
pub fn validate_rel_path(path: &str) -> Result<(), String> {
    use std::path::Component;

    if path.is_empty() {
        return Err(format!("invalid archive path '{path}' (must be relative)"));
    }
    if path.contains('\0') {
        return Err("archive path contains a NUL byte".into());
    }
    for comp in std::path::Path::new(path).components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("invalid archive path '{path}' (must be relative)"));
            }
            Component::ParentDir => {
                return Err(format!("archive path '{path}' escapes the artifact tree"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Walk the archive entries, calling `f(path, data)` for each. Both slices borrow
/// from `blob`, so the metadata-only caller never copies payload bytes.
fn walk_archive<'a>(blob: &'a [u8], mut f: impl FnMut(&'a str, &'a [u8])) -> Result<(), String> {
    let mut rest: &'a [u8] = blob;
    while !rest.is_empty() {
        if rest.len() < 16 {
            return Err("malformed archive entry header".into());
        }
        let path_len = usize::try_from(u64::from_le_bytes(rest[0..8].try_into().unwrap()))
            .map_err(|_| "archive entry path length out of range".to_string())?;
        let data_len = usize::try_from(u64::from_le_bytes(rest[8..16].try_into().unwrap()))
            .map_err(|_| "archive entry data length out of range".to_string())?;
        rest = &rest[16..];
        let end = path_len
            .checked_add(data_len)
            .ok_or_else(|| "archive entry lengths overflow".to_string())?;
        if path_len == 0 || end > rest.len() {
            return Err("malformed archive entry lengths".into());
        }
        let path = std::str::from_utf8(&rest[..path_len])
            .map_err(|_| "archive entry path is not valid UTF-8".to_string())?;
        validate_rel_path(path)?;
        f(path, &rest[path_len..end]);
        rest = &rest[end..];
    }
    Ok(())
}

/// Decode the full archive, copying every payload.
pub fn parse_archive(blob: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    walk_archive(blob, |path, data| {
        out.push((path.to_string(), data.to_vec()))
    })?;
    Ok(out)
}

/// Decode only entry names and payload lengths (no copies), for inspection.
pub fn archive_index(blob: &[u8]) -> Result<Vec<(String, usize)>, String> {
    let mut out = Vec::new();
    walk_archive(blob, |path, data| out.push((path.to_string(), data.len())))?;
    Ok(out)
}

/// Encode files as `{path_len u64}{data_len u64}{path}{data}` entries.
pub fn encode_archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data) in files {
        out.extend_from_slice(&(path.len() as u64).to_le_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(data);
    }
    out
}

// ---- embedded section payload ----------------------------------------------

/// Name of the binary section carrying a desktop app's payload. The `inka
/// desktop` writer embeds it and the per-app shim reads it (via `libsui`), so
/// both must agree. Kept <= 16 bytes for the Mach-O section-name field.
pub const SECTION_NAME: &str = "inka";

/// The decoded embedded payload: the manifest bytes plus the archive blob.
pub struct SectionPayload<'a> {
    pub manifest: &'a [u8],
    pub archive: &'a [u8],
}

/// Encode a desktop payload as `[manifest_len u64 LE][manifest][archive]`.
/// Unlike the launcher's trailing artifact footer, this lives inside a binary
/// section, so the image bytes before it are never modified or re-scanned.
pub fn encode_section_payload(files: &[(String, Vec<u8>)], manifest: &[u8]) -> Vec<u8> {
    let archive = encode_archive(files);
    let mut out = Vec::with_capacity(8 + manifest.len() + archive.len());
    out.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    out.extend_from_slice(manifest);
    out.extend_from_slice(&archive);
    out
}

/// Decode a payload written by [`encode_section_payload`].
pub fn read_section_payload(bytes: &[u8]) -> Result<SectionPayload<'_>, String> {
    if bytes.len() < 8 {
        return Err("embedded payload is smaller than its header".into());
    }
    let mlen = usize::try_from(u64::from_le_bytes(bytes[0..8].try_into().unwrap()))
        .map_err(|_| "embedded payload manifest length out of range".to_string())?;
    let manifest_end = 8usize
        .checked_add(mlen)
        .ok_or_else(|| "embedded payload manifest length overflows".to_string())?;
    if manifest_end > bytes.len() {
        return Err("embedded payload manifest length out of range".into());
    }
    Ok(SectionPayload {
        manifest: &bytes[8..manifest_end],
        archive: &bytes[manifest_end..],
    })
}

// ---- locating an embedded section in a host image --------------------------

/// Locate the raw `inka` payload that was embedded by libsui in a host image,
/// working on arbitrary file bytes (not just the live process image). This is
/// the read side of the launcher/`inka build` section format and of the desktop
/// shim; `inka doctor <artifact>` also uses it. The writer lives in the `inka`
/// CLI, which depends on libsui; keeping the reader here keeps the launcher and
/// the shim's format crate dependency-free.
///
/// Supports the three libsui layouts: an ELF `PT_NOTE` (name `SUI`, type
/// `0x53554901`), a PE `RCDATA` resource named `inka`, and a Mach-O `__SUI`
/// segment (or the Intel sentinel trailer). Returns the encoded section payload
/// for [`read_section_payload`].
pub fn locate_section(image: &[u8]) -> Result<&[u8], String> {
    if image.len() >= 4 && image[..4] == *b"\x7fELF" {
        return locate_elf_note(image).ok_or_else(|| "no inka section in ELF image".to_string());
    }
    if image.len() >= 2 && image[..2] == *b"MZ" {
        return locate_pe_resource(image).ok_or_else(|| "no inka section in PE image".to_string());
    }
    if image.len() >= 4 {
        let magic = u32::from_le_bytes(image[..4].try_into().unwrap());
        // MH_MAGIC_64 / MH_CIGAM_64.
        if magic == 0xFEED_FACF || magic == 0xCFFA_EDFE {
            return locate_macho_section(image)
                .ok_or_else(|| "no inka section in Mach-O image".to_string());
        }
        // MH_MAGIC / MH_CIGAM (32-bit) or FAT magic: unsupported.
        if magic == 0xFEED_FACE
            || magic == 0xCEFA_EDFE
            || magic == 0xCAFE_BABE
            || magic == 0xBEBA_FECA
        {
            return Err("unsupported Mach-O image (expected a 64-bit thin binary)".to_string());
        }
    }
    Err("unrecognized host image format".to_string())
}

/// Decode the section payload from a host image into a [`Trailer`], mirroring
/// [`parse_trailer`] for the embedded-section format.
pub fn parse_section_trailer(image: &[u8]) -> Result<Trailer<'_>, String> {
    let section = locate_section(image)?;
    let payload = read_section_payload(section)?;
    let files = parse_archive(payload.archive)?;
    Ok(Trailer {
        files,
        manifest: payload.manifest,
    })
}

fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        value
    } else {
        (value + (align - 1)) & !(align - 1)
    }
}

fn read_u16(b: &[u8], le: bool) -> u16 {
    let a = [b[0], b[1]];
    if le {
        u16::from_le_bytes(a)
    } else {
        u16::from_be_bytes(a)
    }
}

fn read_u32(b: &[u8], le: bool) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if le {
        u32::from_le_bytes(a)
    } else {
        u32::from_be_bytes(a)
    }
}

fn read_u64(b: &[u8], le: bool) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    if le {
        u64::from_le_bytes(a)
    } else {
        u64::from_be_bytes(a)
    }
}

fn locate_elf_note(image: &[u8]) -> Option<&[u8]> {
    if image.len() < 64 || image[4] != 2 {
        // Only 64-bit ELF is supported (matching the libsui writer).
        return None;
    }
    let le = match image[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let e_phoff = read_u64(&image[0x20..0x28], le) as usize;
    let e_phentsize = read_u16(&image[0x36..0x38], le) as usize;
    let e_phnum = read_u16(&image[0x38..0x3a], le) as usize;
    if e_phoff == 0 || e_phentsize < 56 {
        return None;
    }
    for i in 0..e_phnum {
        let off = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        if off.checked_add(56)? > image.len() {
            return None;
        }
        let p = &image[off..off + 56];
        if read_u32(&p[0..4], le) != 4 {
            continue; // not PT_NOTE
        }
        let p_offset = read_u64(&p[8..16], le) as usize;
        let p_filesz = read_u64(&p[32..40], le) as usize;
        let p_align = read_u64(&p[48..56], le) as usize;
        let end = p_offset.checked_add(p_filesz)?;
        if end > image.len() {
            continue;
        }
        if let Some(data) = find_in_elf_note_segment(&image[p_offset..end], p_align.max(4)) {
            return Some(data);
        }
    }
    None
}

/// Mirrors libsui's `find_in_note_segment`: note header fields are always
/// little-endian, note/desc are padded to `align`.
fn find_in_elf_note_segment(segment: &[u8], align: usize) -> Option<&[u8]> {
    let mut pos = 0usize;
    while pos + 12 <= segment.len() {
        let namesz = u32::from_le_bytes(segment[pos..pos + 4].try_into().ok()?) as usize;
        let descsz = u32::from_le_bytes(segment[pos + 4..pos + 8].try_into().ok()?) as usize;
        let note_type = u32::from_le_bytes(segment[pos + 8..pos + 12].try_into().ok()?);
        pos += 12;
        if pos.checked_add(namesz)? > segment.len() {
            break;
        }
        let mut note_name = &segment[pos..pos + namesz];
        while let [rest @ .., 0] = note_name {
            note_name = rest;
        }
        pos = align_up(pos + namesz, align);
        if pos.checked_add(descsz)? > segment.len() {
            break;
        }
        let desc = &segment[pos..pos + descsz];
        pos = align_up(pos + descsz, align);
        if note_name == b"SUI" && note_type == 0x5355_4901 {
            if let Some(data) = parse_elf_note_desc(desc) {
                return Some(data);
            }
        }
    }
    None
}

/// The SUI note description is `[name_len u16 LE][section name][data]`.
fn parse_elf_note_desc(desc: &[u8]) -> Option<&[u8]> {
    if desc.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(desc[0..2].try_into().ok()?) as usize;
    if desc.len() < 2 + name_len {
        return None;
    }
    if &desc[2..2 + name_len] != SECTION_NAME.as_bytes() {
        return None;
    }
    Some(&desc[2 + name_len..])
}

// PE constants. `RT_RCDATA` is resource type 10; the resource is written with
// the section name uppercased and language ID 0 by libsui.
const PE_RT_RCDATA: u32 = 10;

fn locate_pe_resource(image: &[u8]) -> Option<&[u8]> {
    if image.len() < 0x40 {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(image[0x3c..0x40].try_into().ok()?) as usize;
    if image.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = e_lfanew + 4;
    if coff + 20 > image.len() {
        return None;
    }
    let num_sections = u16::from_le_bytes(image[coff + 2..coff + 4].try_into().ok()?) as usize;
    let size_opt = u16::from_le_bytes(image[coff + 16..coff + 18].try_into().ok()?) as usize;
    let opt = coff + 20;
    let magic = u16::from_le_bytes(image.get(opt..opt + 2)?.try_into().ok()?);
    let dd_off = match magic {
        0x20b => opt + 112, // PE32+
        0x10b => opt + 96,  // PE32
        _ => return None,
    };
    // Data directory entry 2 (resource table): VirtualAddress + Size.
    let res_rva =
        u32::from_le_bytes(image.get(dd_off + 16..dd_off + 20)?.try_into().ok()?) as usize;
    if res_rva == 0 {
        return None;
    }

    // Section table for RVA -> file-offset translation.
    let sec_off = opt.checked_add(size_opt)?;
    let mut sections: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(num_sections);
    for i in 0..num_sections {
        let o = sec_off.checked_add(i.checked_mul(40)?)?;
        if o + 40 > image.len() {
            return None;
        }
        let vsize = u32::from_le_bytes(image[o + 8..o + 12].try_into().ok()?) as usize;
        let va = u32::from_le_bytes(image[o + 12..o + 16].try_into().ok()?) as usize;
        let raw_size = u32::from_le_bytes(image[o + 16..o + 20].try_into().ok()?) as usize;
        let raw = u32::from_le_bytes(image[o + 20..o + 24].try_into().ok()?) as usize;
        sections.push((va, vsize, raw_size, raw));
    }
    let rva_to_off = |rva: usize| -> Option<usize> {
        sections.iter().find_map(|&(va, vsize, raw_size, raw)| {
            if rva >= va && rva - va < vsize.max(raw_size) {
                Some(raw + (rva - va))
            } else {
                None
            }
        })
    };

    let res_base = rva_to_off(res_rva)?;
    let type_off = pe_res_entry(image, res_base, 0, Some(PE_RT_RCDATA), None)?.0;
    let name_off = pe_res_entry(image, res_base, type_off, None, Some(SECTION_NAME))?.0;
    let (data_off, is_dir) = pe_res_entry(image, res_base, name_off, Some(0), None)?;
    if is_dir {
        return None;
    }
    let de = res_base.checked_add(data_off)?;
    let data_rva = u32::from_le_bytes(image.get(de..de + 4)?.try_into().ok()?) as usize;
    let data_size = u32::from_le_bytes(image.get(de + 4..de + 8)?.try_into().ok()?) as usize;
    let file_off = rva_to_off(data_rva)?;
    image.get(file_off..file_off.checked_add(data_size)?)
}

/// Find a child entry of the resource directory at `dir_off` (relative to the
/// resource base) by numeric ID or (case-insensitive) string name. Returns the
/// entry's offset field and whether it points at a subdirectory.
fn pe_res_entry(
    image: &[u8],
    res_base: usize,
    dir_off: usize,
    want_id: Option<u32>,
    want_name: Option<&str>,
) -> Option<(usize, bool)> {
    let d = res_base.checked_add(dir_off)?;
    let named = u16::from_le_bytes(image.get(d + 12..d + 14)?.try_into().ok()?) as usize;
    let ids = u16::from_le_bytes(image.get(d + 14..d + 16)?.try_into().ok()?) as usize;
    let entries = d + 16;
    for i in 0..(named + ids) {
        let e = entries.checked_add(i.checked_mul(8)?)?;
        let name_field = u32::from_le_bytes(image.get(e..e + 4)?.try_into().ok()?);
        let off_field = u32::from_le_bytes(image.get(e + 4..e + 8)?.try_into().ok()?);
        let is_dir = off_field & 0x8000_0000 != 0;
        let off = (off_field & 0x7fff_ffff) as usize;
        let matched = if name_field & 0x8000_0000 != 0 {
            match want_name {
                Some(w) => pe_res_name_eq(image, res_base + (name_field & 0x7fff_ffff) as usize, w),
                None => false,
            }
        } else {
            want_id == Some(name_field)
        };
        if matched {
            return Some((off, is_dir));
        }
    }
    None
}

/// Compare a UTF-16 `IMAGE_RESOURCE_DIR_STRING_U` against an ASCII name.
fn pe_res_name_eq(image: &[u8], so: usize, want: &str) -> bool {
    let Some(len_b) = image.get(so..so + 2) else {
        return false;
    };
    let len = u16::from_le_bytes(len_b.try_into().unwrap()) as usize;
    let mut s = String::with_capacity(len);
    for i in 0..len {
        let Some(cb) = image.get(so + 2 + i * 2..so + 4 + i * 2) else {
            return false;
        };
        let c = u16::from_le_bytes(cb.try_into().unwrap());
        if c > 0x7f {
            return false;
        }
        s.push(c as u8 as char);
    }
    s.eq_ignore_ascii_case(want)
}

// Mach-O: libsui writes a `__SUI` segment holding the payload as a section for
// arm64, and an in-file sentinel trailer for Intel.
const MACHO_SEGNAME: &[u8] = b"__SUI";
const SUI_SENTINEL: &[u8] = b"<~sui-data~>";
const SUI_SENTINEL_MAGIC: [u8; 4] = [0xEF, 0xBE, 0xAD, 0xDE];

fn locate_macho_section(image: &[u8]) -> Option<&[u8]> {
    if image.len() < 32 {
        return None;
    }
    let magic = u32::from_le_bytes(image[..4].try_into().ok()?);
    let cputype = u32::from_le_bytes(image[4..8].try_into().ok()?);
    const CPU_TYPE_ARM_64: u32 = 0x0100_000C;
    if magic != 0xFEED_FACF || cputype != CPU_TYPE_ARM_64 {
        // Intel (or big-endian) Mach-O: locate the appended sentinel.
        return locate_macho_sentinel(image);
    }
    let ncmds = u32::from_le_bytes(image[16..20].try_into().ok()?) as usize;
    let sizeofcmds = u32::from_le_bytes(image[20..24].try_into().ok()?) as usize;
    let end = 32usize.checked_add(sizeofcmds)?;
    if end > image.len() {
        return None;
    }
    let mut off = 32usize;
    for _ in 0..ncmds {
        if off + 8 > end {
            break;
        }
        let cmd = u32::from_le_bytes(image[off..off + 4].try_into().ok()?);
        let cmdsize = u32::from_le_bytes(image[off + 4..off + 8].try_into().ok()?) as usize;
        if cmdsize < 8 || off.checked_add(cmdsize)? > end {
            break;
        }
        if cmd == 0x19 && cmdsize >= 72 {
            // LC_SEGMENT_64.
            let segname = &image[off + 8..off + 24];
            let nsects = u32::from_le_bytes(image[off + 64..off + 68].try_into().ok()?) as usize;
            let mut so = off + 72;
            for _ in 0..nsects {
                if so + 80 > off + cmdsize {
                    break;
                }
                let sectname = &image[so..so + 16];
                if segname.starts_with(MACHO_SEGNAME)
                    && sectname.starts_with(SECTION_NAME.as_bytes())
                {
                    let size =
                        u64::from_le_bytes(image[so + 40..so + 48].try_into().ok()?) as usize;
                    let fileoff =
                        u32::from_le_bytes(image[so + 48..so + 52].try_into().ok()?) as usize;
                    return image.get(fileoff..fileoff.checked_add(size)?);
                }
                so += 80;
            }
        }
        off += cmdsize;
    }
    // No `__SUI` segment; fall back to the Intel sentinel trailer.
    locate_macho_sentinel(image)
}

/// The Intel Mach-O writer appends `<~sui-data~>` + magic + `len u64 LE` + data.
fn locate_macho_sentinel(image: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    while i + SUI_SENTINEL.len() + 4 + 8 <= image.len() {
        if &image[i..i + SUI_SENTINEL.len()] == SUI_SENTINEL
            && image[i + SUI_SENTINEL.len()..i + SUI_SENTINEL.len() + 4] == SUI_SENTINEL_MAGIC
        {
            let len_off = i + SUI_SENTINEL.len() + 4;
            let len = u64::from_le_bytes(image[len_off..len_off + 8].try_into().ok()?) as usize;
            let start = len_off + 8;
            return image.get(start..start.checked_add(len)?);
        }
        i += 1;
    }
    None
}

/// The 24-byte footer: `MAGIC` + archive length + manifest length (LE `u64`).
pub fn encode_footer(archive_len: u64, manifest_len: u64) -> [u8; FOOTER_LEN] {
    let mut footer = [0u8; FOOTER_LEN];
    footer[0..8].copy_from_slice(MAGIC);
    footer[8..16].copy_from_slice(&archive_len.to_le_bytes());
    footer[16..24].copy_from_slice(&manifest_len.to_le_bytes());
    footer
}

/// Byte ranges of the two payload sections inside an artifact.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub archive_off: usize,
    pub archive_len: usize,
    pub manifest_off: usize,
    pub manifest_len: usize,
}

/// Read the footer and locate the archive/manifest ranges. Does not copy.
pub fn read_layout(bytes: &[u8]) -> Result<Layout, String> {
    if bytes.len() < FOOTER_LEN {
        return Err("file smaller than footer".into());
    }
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    if &footer[0..8] != MAGIC {
        return Err("not an inka artifact (missing INKFOOT5 trailer)".into());
    }
    let alen = usize::try_from(u64::from_le_bytes(footer[8..16].try_into().unwrap()))
        .map_err(|_| "artifact archive length out of range".to_string())?;
    let mlen = usize::try_from(u64::from_le_bytes(footer[16..24].try_into().unwrap()))
        .map_err(|_| "artifact manifest length out of range".to_string())?;
    if alen.saturating_add(mlen).saturating_add(FOOTER_LEN) > bytes.len() {
        return Err("trailer lengths out of range".into());
    }
    let manifest_off = bytes.len() - FOOTER_LEN - mlen;
    let archive_off = manifest_off - alen;
    Ok(Layout {
        archive_off,
        archive_len: alen,
        manifest_off,
        manifest_len: mlen,
    })
}

/// The decoded artifact: extracted files plus the raw manifest bytes.
pub struct Trailer<'a> {
    pub files: Vec<(String, Vec<u8>)>,
    pub manifest: &'a [u8],
}

/// Decode the trailer (archive + manifest) from a full artifact image.
pub fn parse_trailer(bytes: &[u8]) -> Result<Trailer<'_>, String> {
    let layout = read_layout(bytes)?;
    let manifest = &bytes[layout.manifest_off..layout.manifest_off + layout.manifest_len];
    let files = parse_archive(&bytes[layout.archive_off..layout.archive_off + layout.archive_len])?;
    Ok(Trailer { files, manifest })
}

// ---- manifest --------------------------------------------------------------

/// Recognized manifest keys, parsed from the line-oriented payload. Unknown
/// keys are ignored so a newer writer stays readable by an older reader.
#[derive(Default)]
pub struct Manifest {
    pub min: Option<Version>,
    pub gt: Option<Version>,
    pub exact: Option<Version>,
    pub tested: Option<Version>,
    /// A present-but-unparseable `runtime=`/`tested-against=` value. Must not
    /// silently drop to "accept anything".
    pub malformed: Option<String>,
    pub module: String,
    /// Canonical permission lines (`permissions=…`, `allow-*=…`, `deny-*=…`),
    /// joined with `\n`, forwarded verbatim to the runtime.
    pub perms: String,
    /// Comma-separated runtime capability names the artifact requires.
    pub requires: String,
    /// `exe` or `cwd`; how relative permission paths are anchored.
    pub path_base: Option<String>,
    /// `beta` when the artifact was built with `inka build --beta`, so the
    /// launcher may select a prerelease runtime tuple.
    pub channel: Option<String>,
}

pub fn parse_manifest(bytes: &[u8]) -> Manifest {
    let mut m = Manifest {
        module: "main.js".into(),
        ..Default::default()
    };
    for raw in String::from_utf8_lossy(bytes).lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        match key {
            "runtime" => {
                let rest = val.strip_prefix("inka_runtime").unwrap_or(val).trim_start();
                let (slot, text) = if let Some(x) = rest.strip_prefix(">=") {
                    (Slot::Min, x)
                } else if let Some(x) = rest.strip_prefix(">") {
                    (Slot::Gt, x)
                } else if let Some(x) = rest.strip_prefix("==") {
                    (Slot::Exact, x)
                } else {
                    (Slot::Exact, rest)
                };
                match parse_version(text) {
                    Some(v) => match slot {
                        Slot::Min => m.min = Some(v),
                        Slot::Gt => m.gt = Some(v),
                        Slot::Exact => m.exact = Some(v),
                    },
                    None => m.malformed = Some(format!("runtime={val}")),
                }
            }
            "tested-against" => match parse_version(val) {
                Some(v) => m.tested = Some(v),
                None => m.malformed = Some(format!("tested-against={val}")),
            },
            "module" => m.module = val.to_string(),
            "requires" => m.requires = val.to_string(),
            "path-base" => m.path_base = Some(val.to_string()),
            "channel" => m.channel = Some(val.to_string()),
            "permissions" => {
                if !m.perms.is_empty() {
                    m.perms.push('\n');
                }
                m.perms.push_str(&format!("permissions={val}"));
            }
            _ if key.starts_with("allow-") || key.starts_with("deny-") => {
                if !m.perms.is_empty() {
                    m.perms.push('\n');
                }
                m.perms.push_str(&format!("{key}={val}"));
            }
            _ => {}
        }
    }
    m
}

enum Slot {
    Min,
    Gt,
    Exact,
}

impl Manifest {
    /// True when the artifact opts into prerelease runtime tuples: `channel=beta`
    /// or any version constraint (`min`/`gt`/`exact`/`tested`) names a
    /// prerelease. A stable artifact never does.
    pub fn wants_prerelease(&self) -> bool {
        self.channel.as_deref() == Some("beta")
            || [self.min, self.gt, self.exact, self.tested]
                .into_iter()
                .flatten()
                .any(|v| v.is_prerelease())
    }
}

/// Does `v` satisfy every constraint in the manifest (floor, strict `>`, exact,
/// and the `tested-against` cap)?
pub fn constraint_allows(m: &Manifest, v: Version) -> bool {
    if let Some(e) = m.exact {
        if v != e {
            return false;
        }
    }
    if let Some(g) = m.gt {
        if v <= g {
            return false;
        }
    }
    if let Some(mn) = m.min {
        if v < mn {
            return false;
        }
    }
    if let Some(t) = m.tested {
        if v > t {
            return false;
        }
    }
    true
}

// ---- permission path tokens ------------------------------------------------

/// Token naming the executable's directory; expanded host-side at launch/run.
pub const EXE_DIR_TOKEN: &str = "${EXE_DIR}";
/// Token naming the project/execution root.
pub const PROJECT_DIR_TOKEN: &str = "${PROJECT_DIR}";

/// Expand portable path tokens and (when `path_base == "exe"`) anchor relative
/// read/write grants to `exe_dir`, in a permission DSL.
///
/// This runs in the host (launcher / `inka run`), never in the engine: the DSL
/// forwarded to the runtime carries Deno-normalized absolute paths, so Deno's
/// own relative-to-cwd semantics are preserved by default (`path_base` unset)
/// while an artifact can opt into portability. Non read/write lines pass
/// through unchanged.
pub fn expand_permissions(
    dsl: &str,
    path_base: Option<&str>,
    exe_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in dsl.split('\n') {
        let line = raw.trim();
        let Some((key, value)) = line.split_once('=') else {
            out.push(line.to_string());
            continue;
        };
        let (kind, cat) = if let Some(c) = key.strip_prefix("allow-") {
            ("allow", c)
        } else if let Some(c) = key.strip_prefix("deny-") {
            ("deny", c)
        } else {
            out.push(line.to_string());
            continue;
        };
        if cat != "read" && cat != "write" {
            out.push(line.to_string());
            continue;
        }
        let items: Vec<String> = value
            .split(',')
            .map(|item| anchor_item(item.trim(), path_base, exe_dir, project_dir))
            .collect();
        out.push(format!("{kind}-{cat}={}", items.join(",")));
    }
    out.join("\n")
}

fn anchor_item(
    item: &str,
    path_base: Option<&str>,
    exe_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> String {
    let mut s = item.to_string();
    if let Some(exe) = exe_dir {
        s = s.replace(EXE_DIR_TOKEN, &exe.to_string_lossy());
    }
    if let Some(project) = project_dir {
        s = s.replace(PROJECT_DIR_TOKEN, &project.to_string_lossy());
    }
    if path_base == Some("exe") {
        if let Some(exe) = exe_dir {
            let plain_relative = !s.is_empty()
                && s != "*"
                && std::path::Path::new(&s).is_relative()
                && !s.contains("://")
                && !s.contains('$');
            if plain_relative {
                s = exe.join(&s).to_string_lossy().into_owned();
            }
        }
    }
    s
}

// ---- runtime capabilities --------------------------------------------------

/// The lowest runtime tuple that enforces the current security model
/// (deny-by-default, realpath confinement, `_dir`-only entry). Artifacts always
/// require at least this tuple. Verified against the installed 0.266.5/0.266.6.
pub const SECURITY_FLOOR: Version = Version::new(0, 266, 5);

/// Capability names a runtime can advertise and an artifact can require, each
/// with the first tuple that provided it. Keep in sync with the runtime's
/// `inka_runtime_features()` (a runtime test asserts the names match).
pub const FEATURE_FLOORS: &[(&str, Version)] = &[
    ("raw-cjs", Version::new(0, 266, 5)),
    ("native-addon", Version::new(0, 266, 5)),
    ("import-perm", Version::new(0, 266, 5)),
    ("tsconfig-run", Version::new(0, 266, 5)),
    ("workspace", Version::new(0, 266, 5)),
];

/// Every capability name this inka release knows how to require.
pub fn known_features() -> Vec<&'static str> {
    FEATURE_FLOORS.iter().map(|(n, _)| *n).collect()
}

/// Parse a comma-separated `requires=` value (empty items dropped, order kept).
pub fn parse_requires(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The minimum tuple that provides every requested capability, never below
/// `SECURITY_FLOOR`. Unknown names are ignored (forward compatibility).
pub fn floor_for(features: &[String]) -> Version {
    let mut floor = SECURITY_FLOOR;
    for f in features {
        if let Some((_, v)) = FEATURE_FLOORS.iter().find(|(n, _)| n == f) {
            if *v > floor {
                floor = *v;
            }
        }
    }
    floor
}

/// Does the manifest contain a `key=` line (anywhere)?
pub fn manifest_has_key(bytes: &[u8], key: &str) -> bool {
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|l| l.trim_start().starts_with(&format!("{key}=")))
}

/// Replace (or append) a `key=` line in a manifest buffer.
pub fn manifest_set_key(bytes: &mut Vec<u8>, key: &str, value: &str) {
    let text = String::from_utf8_lossy(bytes);
    let mut replaced = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&format!("{key}=")) {
            out.push(format!("{key}={value}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        if !out.is_empty() {
            out.push(String::new()); // blank line separator
        }
        out.push(format!("{key}={value}"));
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    *bytes = joined.into_bytes();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(path.len() as u64).to_le_bytes());
        v.extend_from_slice(&(data.len() as u64).to_le_bytes());
        v.extend_from_slice(path.as_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn archive_roundtrip() {
        let files = vec![
            ("main.js".to_string(), b"hi".to_vec()),
            ("node_modules/x/index.js".to_string(), b"x".to_vec()),
        ];
        let blob = encode_archive(&files);
        assert_eq!(parse_archive(&blob).unwrap(), files);
        assert_eq!(
            archive_index(&blob).unwrap(),
            vec![
                ("main.js".to_string(), 2),
                ("node_modules/x/index.js".to_string(), 1),
            ]
        );
    }

    #[test]
    fn archive_overflow_header_is_rejected() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&u64::MAX.to_le_bytes());
        blob.extend_from_slice(&1u64.to_le_bytes());
        assert!(parse_archive(&blob).is_err());
    }

    #[test]
    fn archive_rejects_parent_dir() {
        assert!(parse_archive(&entry("../evil", b"x")).is_err());
        assert!(parse_archive(&entry("/abs", b"x")).is_err());
    }

    #[test]
    fn section_payload_roundtrip() {
        let files = vec![
            ("main.js".to_string(), b"console.log(1)".to_vec()),
            ("assets/a.txt".to_string(), b"a".to_vec()),
        ];
        let manifest = "module=main.js\napp-name=Demo\n";
        let payload = encode_section_payload(&files, manifest.as_bytes());
        let got = read_section_payload(&payload).unwrap();
        assert_eq!(got.manifest, manifest.as_bytes());
        assert_eq!(parse_archive(got.archive).unwrap(), files);
    }

    #[test]
    fn section_payload_rejects_truncated_and_oversized() {
        assert!(read_section_payload(&[]).is_err());
        // Header claims a 1-byte manifest but provides none.
        assert!(read_section_payload(&1u64.to_le_bytes()).is_err());
        // Header claims a manifest longer than the payload; rejected before
        // overflowing (all-ones manifest length).
        let mut blob = u64::MAX.to_le_bytes().to_vec();
        blob.extend_from_slice(b"x");
        assert!(read_section_payload(&blob).is_err());
    }

    #[test]
    fn footer_roundtrip_and_magic() {
        let files = vec![("main.js".to_string(), b"hello".to_vec())];
        let archive = encode_archive(&files);
        let manifest = b"module=main.js\n";
        let mut image = vec![0x7f, 0x45, 0x4c, 0x46]; // launcher bytes
        image.extend_from_slice(&archive);
        image.extend_from_slice(manifest);
        image.extend_from_slice(&encode_footer(archive.len() as u64, manifest.len() as u64));

        let layout = read_layout(&image).unwrap();
        assert_eq!(layout.archive_len, archive.len());
        assert_eq!(layout.manifest_len, manifest.len());
        let t = parse_trailer(&image).unwrap();
        assert_eq!(t.files, files);
        assert_eq!(t.manifest, manifest);
        assert_eq!(manifest_index(&image), vec![("main.js".to_string(), 5)]);
        assert!(parse_manifest(t.manifest).module == "main.js");
    }

    #[test]
    fn layout_rejects_non_artifact() {
        assert!(read_layout(b"not an artifact at all").is_err());
        let mut not_magic = vec![0u8; 32];
        not_magic[24..32].copy_from_slice(b"XXXXXXXX");
        assert!(read_layout(&not_magic).is_err());
    }

    #[test]
    fn manifest_version_operators_and_constraints() {
        let m = parse_manifest(b"runtime=inka_runtime>=0.266.2\n");
        assert_eq!(m.min, Some(Version::new(0, 266, 2)));
        assert!(m.gt.is_none() && m.exact.is_none() && m.malformed.is_none());

        let m = parse_manifest(b"runtime=inka_runtime>0.266.2\n");
        assert_eq!(m.gt, Some(Version::new(0, 266, 2)));
        assert!(
            !constraint_allows(&m, Version::new(0, 266, 2)),
            "> rejects equal"
        );
        assert!(constraint_allows(&m, Version::new(0, 266, 3)));

        let m = parse_manifest(b"runtime=inka_runtime==0.266.2\n");
        assert_eq!(m.exact, Some(Version::new(0, 266, 2)));
        assert!(!constraint_allows(&m, Version::new(0, 266, 3)));

        let m = parse_manifest(b"runtime=0.266.2\n");
        assert_eq!(m.exact, Some(Version::new(0, 266, 2)));

        let m = parse_manifest(b"runtime=inka_runtime>=0.266.2\ntested-against=0.266.4\n");
        assert!(constraint_allows(&m, Version::new(0, 266, 2)));
        assert!(constraint_allows(&m, Version::new(0, 266, 4)));
        assert!(!constraint_allows(&m, Version::new(0, 266, 5)), "cap");
    }

    #[test]
    fn malformed_version_is_detected() {
        assert!(parse_manifest(b"runtime=inka_runtime>=garbage\n")
            .malformed
            .is_some());
        assert!(parse_manifest(b"tested-against=nope\n").malformed.is_some());
    }

    #[test]
    fn wants_prerelease_covers_channel_and_each_slot() {
        // A plain/stable artifact never opts in.
        assert!(!parse_manifest(b"module=main.js\n").wants_prerelease());
        assert!(!parse_manifest(b"runtime=inka_runtime>=0.266.5\n").wants_prerelease());
        assert!(!parse_manifest(b"channel=stable\n").wants_prerelease());
        // `channel=beta` opts in.
        assert!(parse_manifest(b"channel=beta\n").wants_prerelease());
        // Each version slot opts in on its own.
        assert!(parse_manifest(b"runtime=inka_runtime>=0.267.2-beta.1\n").wants_prerelease());
        assert!(parse_manifest(b"runtime=inka_runtime>0.267.2-beta.1\n").wants_prerelease());
        assert!(parse_manifest(b"runtime=inka_runtime==0.267.2-beta.1\n").wants_prerelease());
        assert!(parse_manifest(b"tested-against=0.267.2-rc.1\n").wants_prerelease());
    }

    #[test]
    fn manifest_perms_requires_and_path_base() {
        let m = parse_manifest(
            b"permissions=all\nallow-read=${EXE_DIR}/data\ndeny-read=/etc\nrequires=native-addon,raw-cjs\npath-base=exe\nchannel=beta\n",
        );
        assert_eq!(
            m.perms,
            "permissions=all\nallow-read=${EXE_DIR}/data\ndeny-read=/etc"
        );
        assert_eq!(m.requires, "native-addon,raw-cjs");
        assert_eq!(m.path_base.as_deref(), Some("exe"));
        assert_eq!(m.channel.as_deref(), Some("beta"));
    }

    #[test]
    fn manifest_set_key_appends_and_replaces() {
        let mut appended = b"runtime=x\n".to_vec();
        manifest_set_key(&mut appended, "module", "app.js");
        assert_eq!(
            String::from_utf8(appended).unwrap(),
            "runtime=x\n\nmodule=app.js\n"
        );
        let mut replaced = b"module=old.js\n".to_vec();
        manifest_set_key(&mut replaced, "module", "sub/main.ts");
        assert_eq!(String::from_utf8(replaced).unwrap(), "module=sub/main.ts\n");
        assert!(manifest_has_key(b"module=x\n", "module"));
        assert!(!manifest_has_key(b"module=x\n", "runtime"));
    }

    #[test]
    fn parse_version_strictness() {
        assert_eq!(parse_version("0.266.7"), Some(Version::new(0, 266, 7)));
        assert_eq!(parse_version("1.2"), Some(Version::new(1, 2, 0)));
        assert_eq!(parse_version(" 1 "), Some(Version::new(1, 0, 0)));
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("0.0.0-stub"), None);
        // Only `beta.N`/`rc.N` prereleases are accepted.
        assert_eq!(
            parse_version("0.267.2-beta.1"),
            Some(Version {
                major: 0,
                minor: 267,
                patch: 2,
                pre: Pre::Beta(1),
            })
        );
        assert_eq!(
            parse_version("0.8.1-rc.10"),
            Some(Version {
                major: 0,
                minor: 8,
                patch: 1,
                pre: Pre::Rc(10),
            })
        );
        assert_eq!(parse_version("0.267.2-beta"), None);
        assert_eq!(parse_version("0.267.2-alpha.1"), None);
    }

    #[test]
    fn prerelease_ordering_and_display() {
        let beta1 = parse_version("0.267.2-beta.1").unwrap();
        let beta9 = parse_version("0.267.2-beta.9").unwrap();
        let beta10 = parse_version("0.267.2-beta.10").unwrap();
        let rc1 = parse_version("0.267.2-rc.1").unwrap();
        let release = parse_version("0.267.2").unwrap();
        let prior = parse_version("0.267.1").unwrap();

        // Prereleases sort below their release and above the prior patch.
        assert!(prior < beta1 && beta1 < beta10 && beta10 < rc1 && rc1 < release);
        // Numeric counters compare numerically, not lexically.
        assert!(beta9 < beta10);
        // Round-trips through Display.
        assert_eq!(beta10.to_string(), "0.267.2-beta.10");
        assert_eq!(release.to_string(), "0.267.2");
        assert!(beta1.is_prerelease() && !release.is_prerelease());
    }

    #[test]
    fn floor_and_requires() {
        assert_eq!(floor_for(&[]), SECURITY_FLOOR);
        assert_eq!(
            parse_requires("raw-cjs, native-addon"),
            vec!["raw-cjs", "native-addon"]
        );
        assert!(parse_requires("").is_empty());
        // Unknown names are ignored and never lower the floor.
        assert_eq!(floor_for(&["nope".to_string()]), SECURITY_FLOOR);
        // Every known feature is satisfied at the security floor today.
        let all: Vec<String> = known_features().iter().map(|s| s.to_string()).collect();
        assert_eq!(floor_for(&all), SECURITY_FLOOR);
    }

    /// Helper mirroring `read_layout` + `archive_index` for the roundtrip test.
    fn manifest_index(image: &[u8]) -> Vec<(String, usize)> {
        let layout = read_layout(image).unwrap();
        archive_index(&image[layout.archive_off..layout.archive_off + layout.archive_len]).unwrap()
    }

    #[test]
    fn expand_permissions_tokens() {
        let exe = Path::new("/opt/app");
        let proj = Path::new("/work/proj");
        let dsl = "permissions=all\nallow-read=${EXE_DIR}/data,./rel\nallow-write=${PROJECT_DIR}/out\ndeny-read=/etc";
        let out = expand_permissions(dsl, None, Some(exe), Some(proj));
        assert_eq!(
            out,
            "permissions=all\nallow-read=/opt/app/data,./rel\nallow-write=/work/proj/out\ndeny-read=/etc"
        );
    }

    #[test]
    fn expand_permissions_path_base_exe_anchors_relative() {
        let exe = Path::new("/opt/app");
        let dsl = "allow-read=./data,*,/abs,${EXE_DIR}/x";
        let out = expand_permissions(dsl, Some("exe"), Some(exe), None);
        // Compare paths, not a hardcoded separator string: `join` uses the
        // platform separator (`\` on Windows).
        let joined = exe.join("./data");
        let anchored = joined.to_string_lossy();
        assert_eq!(out, format!("allow-read={anchored},*,/abs,/opt/app/x"));
    }

    #[test]
    fn expand_permissions_leaves_unknown_token_untouched() {
        // Without an exe dir the token must survive (visible, not silently "/x").
        let out = expand_permissions("allow-read=${EXE_DIR}/x", None, None, None);
        assert_eq!(out, "allow-read=${EXE_DIR}/x");
    }

    #[test]
    fn expand_permissions_ignores_non_read_write_lines() {
        let out = expand_permissions(
            "allow-net=example.com\nallow-env=*",
            Some("exe"),
            Some(Path::new("/opt/app")),
            None,
        );
        assert_eq!(out, "allow-net=example.com\nallow-env=*");
    }

    #[test]
    fn locate_section_rejects_non_artifacts() {
        assert!(locate_section(b"not an executable at all").is_err());
        // A plausible but section-less ELF.
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        assert!(locate_section(&elf).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn locate_section_roundtrips_native_embed() {
        // Embed with the same libsui writer the CLI uses, then read it back.
        let image = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let files = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let payload = encode_section_payload(&files, b"module=main.js\napp=Demo\n");

        let out: Vec<u8> = {
            #[cfg(target_os = "linux")]
            {
                let mut out = Vec::new();
                libsui::Elf::new(&image)
                    .append(SECTION_NAME, &payload, &mut out)
                    .unwrap();
                out
            }
            #[cfg(target_os = "windows")]
            {
                let mut out = Vec::new();
                libsui::PortableExecutable::from(&image)
                    .unwrap()
                    .write_resource(SECTION_NAME, payload.clone())
                    .unwrap()
                    .build(&mut out)
                    .unwrap();
                out
            }
        };

        assert_eq!(locate_section(&out).unwrap(), payload.as_slice());
        let t = parse_section_trailer(&out).unwrap();
        assert_eq!(t.files, files);
        assert_eq!(t.manifest, b"module=main.js\napp=Demo\n");
    }

    #[test]
    fn locate_section_reads_macho_intel_sentinel() {
        let payload = encode_section_payload(
            &[("main.js".to_string(), b"x".to_vec())],
            b"module=main.js\n",
        );
        // A 64-bit x86_64 Mach-O (not arm64) takes the sentinel path.
        let mut image = vec![0u8; 32];
        image[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        image[4..8].copy_from_slice(&0x0100_0007u32.to_le_bytes()); // CPU_TYPE_X86_64
        image.extend_from_slice(SUI_SENTINEL);
        image.extend_from_slice(&SUI_SENTINEL_MAGIC);
        image.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        image.extend_from_slice(&payload);
        assert_eq!(locate_section(&image).unwrap(), payload.as_slice());
    }

    #[test]
    fn locate_section_reads_macho_arm64_segment() {
        let files = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let payload = encode_section_payload(&files, b"module=main.js\n");
        let data_off = 32 + 152; // header + one LC_SEGMENT_64 (72 + 80)
        let mut image = vec![0u8; 32];
        image[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
        image[4..8].copy_from_slice(&0x0100_000Cu32.to_le_bytes()); // CPU_TYPE_ARM_64
        image[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds
        image[20..24].copy_from_slice(&152u32.to_le_bytes()); // sizeofcmds
        let mut cmd = [0u8; 152];
        cmd[0..4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
        cmd[4..8].copy_from_slice(&152u32.to_le_bytes());
        cmd[8..8 + MACHO_SEGNAME.len()].copy_from_slice(MACHO_SEGNAME);
        cmd[64..68].copy_from_slice(&1u32.to_le_bytes()); // nsects
        let s = &mut cmd[72..152];
        s[..SECTION_NAME.len()].copy_from_slice(SECTION_NAME.as_bytes());
        s[16..16 + MACHO_SEGNAME.len()].copy_from_slice(MACHO_SEGNAME);
        s[40..48].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // size
        s[48..52].copy_from_slice(&(data_off as u32).to_le_bytes()); // offset
        image.extend_from_slice(&cmd);
        image.extend_from_slice(&payload);

        assert_eq!(locate_section(&image).unwrap(), payload.as_slice());
        let t = parse_section_trailer(&image).unwrap();
        assert_eq!(t.files, files);
        assert_eq!(t.manifest, b"module=main.js\n");
    }
}
