use std::env;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};

const FOOTER_LEN: usize = 24;
const MAGIC_V1: &[u8] = b"INKFOOT2"; // single embedded source
const MAGIC_V2: &[u8] = b"INKFOOT3"; // multi-file archive

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("INKA_DEBUG").is_some() {
            eprintln!($($arg)*);
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let mut parts = s.split('.');
    let a = parts.next()?.trim().parse().ok()?;
    let b = parts.next().unwrap_or("0").trim().parse().ok()?;
    let c = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some(Version(a, b, c))
}

enum Trailer<'a> {
    /// v1: a single embedded source file + manifest.
    Single {
        source: &'a [u8],
        manifest: &'a [u8],
    },
    /// v2: an archive of relative-path files + manifest.
    Archive {
        files: Vec<(String, Vec<u8>)>,
        manifest: &'a [u8],
    },
}

fn parse_trailer(bytes: &[u8]) -> Result<Trailer<'_>, String> {
    if bytes.len() < FOOTER_LEN {
        return Err("file smaller than footer".into());
    }
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    let magic = &footer[0..8];
    let alen = u64::from_le_bytes(footer[8..16].try_into().unwrap()) as usize;
    let mlen = u64::from_le_bytes(footer[16..24].try_into().unwrap()) as usize;
    if alen.saturating_add(mlen).saturating_add(FOOTER_LEN) > bytes.len() {
        return Err("trailer lengths out of range".into());
    }
    let mstart = bytes.len() - FOOTER_LEN - mlen;
    let pstart = mstart - alen;
    let manifest = &bytes[mstart..mstart + mlen];
    match magic {
        MAGIC_V1 => Ok(Trailer::Single {
            source: &bytes[pstart..mstart],
            manifest,
        }),
        MAGIC_V2 => {
            let files = parse_archive(&bytes[pstart..mstart])?;
            Ok(Trailer::Archive { files, manifest })
        }
        _ => Err("trailer magic not found (not an inka artifact?)".into()),
    }
}

/// Archive layout: repeated `{path_len u64}{data_len u64}{path}{data}`.
fn parse_archive(blob: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut rest = blob;
    while !rest.is_empty() {
        if rest.len() < 16 {
            return Err("malformed archive entry header".into());
        }
        let path_len = u64::from_le_bytes(rest[0..8].try_into().unwrap()) as usize;
        let data_len = u64::from_le_bytes(rest[8..16].try_into().unwrap()) as usize;
        rest = &rest[16..];
        if path_len == 0 || path_len + data_len > rest.len() {
            return Err("malformed archive entry lengths".into());
        }
        let path_bytes = &rest[..path_len];
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| "archive entry path is not valid UTF-8".to_string())?;
        validate_rel_path(path)?;
        let data = rest[path_len..path_len + data_len].to_vec();
        rest = &rest[path_len + data_len..];
        files.push((path.to_string(), data));
    }
    Ok(files)
}

fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || Path::new(path).is_absolute() {
        return Err(format!("invalid archive path '{path}' (must be relative)"));
    }
    if path.contains('\0') {
        return Err("archive path contains a NUL byte".into());
    }
    for comp in path.split('/') {
        if comp == ".." {
            return Err(format!("archive path '{path}' escapes the artifact tree"));
        }
    }
    Ok(())
}

/// Materialize the embedded archive under a fresh temp dir, mirroring paths.
fn extract_tree(files: &[(String, Vec<u8>)]) -> Result<PathBuf, String> {
    let nonce = format!("{}-{}", std::process::id(), files.len());
    let root = std::env::temp_dir().join(format!("inka-{nonce}"));
    let _ = fs::remove_dir_all(&root);
    for (path, data) in files {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::write(&target, data)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    Ok(root)
}

#[derive(Default)]
struct Manifest {
    min: Option<Version>,
    exact: Option<Version>,
    tested: Option<Version>,
    module: String,
    /// Canonical permission lines (`permissions=…`, `allow-*=…`, `deny-*=…`)
    /// forwarded verbatim to the runtime.
    perms: String,
}

fn parse_manifest(bytes: &[u8]) -> Manifest {
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
                let rest = val
                    .strip_prefix("inka_runtime")
                    .unwrap_or(val)
                    .trim_start();
                if let Some(x) = rest.strip_prefix(">=") {
                    m.min = parse_version(x);
                } else if let Some(x) = rest.strip_prefix(">") {
                    m.min = parse_version(x);
                } else if let Some(x) = rest.strip_prefix("==") {
                    m.exact = parse_version(x);
                } else {
                    m.exact = parse_version(rest);
                }
            }
            "tested-against" => m.tested = parse_version(val),
            "module" => m.module = val.to_string(),
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

fn runtime_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = env::var_os("INKA_RUNTIME_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(h) = env::var_os("HOME") {
        out.push(PathBuf::from(h).join(".inka-runtime"));
    }
    out.push(PathBuf::from("/usr/local/lib/inka-runtime"));
    out
}

fn resolve_runtime(m: &Manifest, dirs: &[PathBuf]) -> Option<(Version, PathBuf)> {
    if let Some(p) = env::var_os("INKA_RUNTIME") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some((Version(0, 0, 0), p));
        }
    }
    let mut best: Option<(Version, PathBuf)> = None;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let Some(stripped) = name.strip_prefix("libinka_runtime-") else {
                continue;
            };
            let Some(vstr) = stripped.strip_suffix(".so") else {
                continue;
            };
            let Some(v) = parse_version(vstr) else { continue };
            if let Some(exact) = m.exact {
                if v != exact {
                    continue;
                }
            }
            if let Some(min) = m.min {
                if v < min {
                    continue;
                }
            }
            if let Some(t) = m.tested {
                if v > t {
                    continue;
                }
            }
            if best.as_ref().map_or(true, |(bv, _)| v > *bv) {
                best = Some((v, ent.path()));
            }
        }
    }
    best
}

fn required_string(m: &Manifest) -> String {
    if let Some(e) = m.exact {
        format!("inka_runtime == {e}")
    } else if let Some(x) = m.min {
        format!("inka_runtime >= {x}")
    } else {
        "inka_runtime any".into()
    }
}

fn load_and_run(
    lib: &Path,
    module: &str,
    payload: &[u8],
    args: &[String],
    perms: &str,
) -> i32 {
    let library = match unsafe { libloading::Library::new(lib) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[inka] failed to load {}: {e}", lib.display());
            std::process::exit(1);
        }
    };

    type FnVersion = unsafe extern "C" fn() -> *const c_char;
    type FnCreate = unsafe extern "C" fn() -> *mut c_void;
    type FnRun = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        usize,
        c_int,
        *const *const c_char,
        *mut c_int,
        *mut *mut c_char,
    ) -> c_int;
    type FnRunPerm = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        usize,
        c_int,
        *const *const c_char,
        *mut c_int,
        *mut *mut c_char,
        *const c_char,
    ) -> c_int;
    type FnDestroy = unsafe extern "C" fn(*mut c_void);

    unsafe {
        let ver: libloading::Symbol<FnVersion> = library
            .get(b"inka_runtime_version")
            .expect("missing inka_runtime_version");
        let reported = CStr::from_ptr(ver()).to_string_lossy().into_owned();

        let create: libloading::Symbol<FnCreate> =
            library.get(b"inka_runtime_create").expect("missing inka_runtime_create");
        let destroy: libloading::Symbol<FnDestroy> = library
            .get(b"inka_runtime_destroy")
            .expect("missing inka_runtime_destroy");

        debug_log!("[inka] runtime {} reports: {reported}", lib.display());

        let rt = create();
        let spec = CString::new(module).unwrap_or_else(|_| CString::new("main.js").unwrap());
        let perms_c = CString::new(perms).unwrap_or_else(|_| CString::new("").unwrap());
        let argv: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).expect("nul byte in arg"))
            .collect();
        let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let mut exit_code: c_int = 0;
        let mut err_msg: *mut c_char = std::ptr::null_mut();

        let call = |rt, spec, exit_code, err_msg| {
            if let Ok(perm_sym) = library.get::<FnRunPerm>(b"inka_runtime_run_module_perm") {
                perm_sym(
                    rt,
                    spec,
                    payload.as_ptr() as *const c_char,
                    payload.len(),
                    argv.len() as c_int,
                    argv_ptrs.as_ptr(),
                    exit_code,
                    err_msg,
                    perms_c.as_ptr(),
                )
            } else {
                if !perms.is_empty() {
                    eprintln!(
                        "[inka] artifact declares permissions but runtime {} lacks support \
                         (inka_runtime_run_module_perm); refusing to run allow-all",
                        lib.display()
                    );
                    destroy(rt);
                    std::process::exit(4);
                }
                let legacy: libloading::Symbol<FnRun> = library
                    .get(b"inka_runtime_run_module")
                    .expect("missing inka_runtime_run_module");
                legacy(
                    rt,
                    spec,
                    payload.as_ptr() as *const c_char,
                    payload.len(),
                    argv.len() as c_int,
                    argv_ptrs.as_ptr(),
                    exit_code,
                    err_msg,
                )
            }
        };

        let rc = call(rt, spec.as_ptr(), &mut exit_code, &mut err_msg);

        if !err_msg.is_null() {
            eprintln!("[inka] runtime error message: {}", CStr::from_ptr(err_msg).to_string_lossy());
        }
        destroy(rt);

        if rc != 0 {
            eprintln!("[inka] runtime call failed (rc={rc})");
            std::process::exit(rc);
        }
        exit_code
    }
}

fn load_and_run_dir(
    lib: &Path,
    dir: &str,
    entry: &str,
    args: &[String],
    perms: &str,
) -> i32 {
    let library = match unsafe { libloading::Library::new(lib) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[inka] failed to load {}: {e}", lib.display());
            std::process::exit(1);
        }
    };

    type FnRunDir = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        c_int,
        *const *const c_char,
        *mut c_int,
        *mut *mut c_char,
        *const c_char,
    ) -> c_int;

    unsafe {
        let dir_c = CString::new(dir).expect("nul in dir");
        let entry_c = CString::new(entry).expect("nul in entry");
        let perms_c = CString::new(perms).unwrap_or_else(|_| CString::new("").unwrap());
        let argv: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).expect("nul byte in arg"))
            .collect();
        let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let create: libloading::Symbol<unsafe extern "C" fn() -> *mut c_void> =
            library.get(b"inka_runtime_create").expect("missing inka_runtime_create");
        let destroy: libloading::Symbol<unsafe extern "C" fn(*mut c_void)> = library
            .get(b"inka_runtime_destroy")
            .expect("missing inka_runtime_destroy");
        let run_dir: libloading::Symbol<FnRunDir> = match library
            .get(b"inka_runtime_run_module_dir")
        {
            Ok(s) => s,
            Err(_) => {
                eprintln!(
                    "[inka] this artifact is multi-file but runtime {} does not support it \
                     (missing inka_runtime_run_module_dir); install a newer runtime",
                    lib.display()
                );
                std::process::exit(4);
            }
        };

        let rt = create();
        let mut exit_code: c_int = 0;
        let mut err_msg: *mut c_char = std::ptr::null_mut();
        let rc = run_dir(
            rt,
            dir_c.as_ptr(),
            entry_c.as_ptr(),
            argv.len() as c_int,
            argv_ptrs.as_ptr(),
            &mut exit_code,
            &mut err_msg,
            perms_c.as_ptr(),
        );

        if !err_msg.is_null() {
            eprintln!("[inka] runtime error message: {}", CStr::from_ptr(err_msg).to_string_lossy());
        }
        destroy(rt);

        if rc != 0 {
            eprintln!("[inka] runtime call failed (rc={rc})");
            std::process::exit(rc);
        }
        exit_code
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let me = fs::read_link("/proc/self/exe").expect("read /proc/self/exe");
    let bytes = fs::read(&me).expect("read own executable");

    let trailer = match parse_trailer(&bytes) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[inka] {e}");
            std::process::exit(2);
        }
    };

    let manifest_bytes = match &trailer {
        Trailer::Single { manifest, .. } | Trailer::Archive { manifest, .. } => *manifest,
    };
    let m = parse_manifest(manifest_bytes);
    let dirs = runtime_dirs();

    let Some((v, path)) = resolve_runtime(&m, &dirs) else {
        eprintln!("[inka] no compatible runtime found");
        eprintln!("[inka] required: {}", required_string(&m));
        if let Some(t) = m.tested {
            eprintln!("[inka] capped at tested-against {t}");
        }
        for d in &dirs {
            eprintln!("[inka]   searched: {}", d.display());
        }
        std::process::exit(3);
    };

    let code = match trailer {
        Trailer::Single { source, manifest: _ } => {
            debug_log!("[inka] resolved inka_runtime {v} at {}", path.display());
            debug_log!("[inka] module '{}' payload {} bytes", m.module, source.len());
            load_and_run(&path, &m.module, source, &args, &m.perms)
        }
        Trailer::Archive { files, manifest: _ } => {
            debug_log!("[inka] resolved inka_runtime {v} at {}", path.display());
            debug_log!("[inka] module '{}' archive {} files", m.module, files.len());
            let root = match extract_tree(&files) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[inka] failed to extract artifact tree: {e}");
                    std::process::exit(1);
                }
            };
            let code = load_and_run_dir(
                &path,
                &root.to_string_lossy(),
                &m.module,
                &args,
                &m.perms,
            );
            let _ = fs::remove_dir_all(&root);
            code
        }
    };
    std::process::exit(code);
}
