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
pub fn encode_section_payload(files: &[(String, Vec<u8>)], manifest: &str) -> Vec<u8> {
    let archive = encode_archive(files);
    let mut out = Vec::with_capacity(8 + manifest.len() + archive.len());
    out.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
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
        let payload = encode_section_payload(&files, manifest);
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
}
