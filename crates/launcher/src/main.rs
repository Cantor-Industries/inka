use std::env;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};

const FOOTER_LEN: usize = 24;
const MAGIC: &[u8] = b"DEXFOOT2";

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("DEX_DEBUG").is_some() {
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

fn parse_trailer(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if bytes.len() < FOOTER_LEN {
        return Err("file smaller than footer".into());
    }
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    if &footer[0..8] != MAGIC {
        return Err("trailer magic not found (not a dex artifact?)".into());
    }
    let plen = u64::from_le_bytes(footer[8..16].try_into().unwrap()) as usize;
    let mlen = u64::from_le_bytes(footer[16..24].try_into().unwrap()) as usize;
    if plen.saturating_add(mlen).saturating_add(FOOTER_LEN) > bytes.len() {
        return Err("trailer lengths out of range".into());
    }
    let mstart = bytes.len() - FOOTER_LEN - mlen;
    let pstart = mstart - plen;
    Ok((&bytes[pstart..mstart], &bytes[mstart..mstart + mlen]))
}

#[derive(Default)]
struct Manifest {
    min: Option<Version>,
    exact: Option<Version>,
    tested: Option<Version>,
    module: String,
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
                    .strip_prefix("deno_runtime")
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
            _ => {}
        }
    }
    m
}

fn runtime_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = env::var_os("DENO_RUNTIME_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(h) = env::var_os("HOME") {
        out.push(PathBuf::from(h).join(".deno-runtime"));
    }
    out.push(PathBuf::from("/usr/local/lib/deno-runtime"));
    out
}

fn resolve_runtime(m: &Manifest, dirs: &[PathBuf]) -> Option<(Version, PathBuf)> {
    if let Some(p) = env::var_os("DEX_RUNTIME") {
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
            let Some(stripped) = name.strip_prefix("libdeno_runtime-") else {
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
        format!("deno_runtime == {e}")
    } else if let Some(x) = m.min {
        format!("deno_runtime >= {x}")
    } else {
        "deno_runtime any".into()
    }
}

fn load_and_run(lib: &Path, module: &str, payload: &[u8], args: &[String]) -> i32 {
    let library = match unsafe { libloading::Library::new(lib) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[dex] failed to load {}: {e}", lib.display());
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
    type FnDestroy = unsafe extern "C" fn(*mut c_void);

    unsafe {
        let ver: libloading::Symbol<FnVersion> = library
            .get(b"dex_runtime_version")
            .expect("missing dex_runtime_version");
        let reported = CStr::from_ptr(ver()).to_string_lossy().into_owned();

        let create: libloading::Symbol<FnCreate> =
            library.get(b"dex_runtime_create").expect("missing dex_runtime_create");
        let run_mod: libloading::Symbol<FnRun> = library
            .get(b"dex_runtime_run_module")
            .expect("missing dex_runtime_run_module");
        let destroy: libloading::Symbol<FnDestroy> = library
            .get(b"dex_runtime_destroy")
            .expect("missing dex_runtime_destroy");

        debug_log!("[dex] runtime {} reports: {reported}", lib.display());

        let rt = create();
        let spec = CString::new(module).unwrap_or_else(|_| CString::new("main.js").unwrap());
        let argv: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).expect("nul byte in arg"))
            .collect();
        let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let mut exit_code: c_int = 0;
        let mut err_msg: *mut c_char = std::ptr::null_mut();
        let rc = run_mod(
            rt,
            spec.as_ptr(),
            payload.as_ptr() as *const c_char,
            payload.len(),
            argv.len() as c_int,
            argv_ptrs.as_ptr(),
            &mut exit_code,
            &mut err_msg,
        );

        if !err_msg.is_null() {
            eprintln!("[dex] runtime error message: {}", CStr::from_ptr(err_msg).to_string_lossy());
        }
        destroy(rt);

        if rc != 0 {
            eprintln!("[dex] runtime call failed (rc={rc})");
            std::process::exit(rc);
        }
        exit_code
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let me = fs::read_link("/proc/self/exe").expect("read /proc/self/exe");
    let bytes = fs::read(&me).expect("read own executable");

    let (payload, manifest_bytes) = match parse_trailer(&bytes) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[dex] {e}");
            std::process::exit(2);
        }
    };

    let m = parse_manifest(manifest_bytes);
    let dirs = runtime_dirs();

    let Some((v, path)) = resolve_runtime(&m, &dirs) else {
        eprintln!("[dex] no compatible runtime found");
        eprintln!("[dex] required: {}", required_string(&m));
        if let Some(t) = m.tested {
            eprintln!("[dex] capped at tested-against {t}");
        }
        for d in &dirs {
            eprintln!("[dex]   searched: {}", d.display());
        }
        std::process::exit(3);
    };

    debug_log!("[dex] resolved deno_runtime {v} at {}", path.display());
    debug_log!("[dex] module '{}' payload {} bytes", m.module, payload.len());
    let code = load_and_run(&path, &m.module, payload, &args);
    std::process::exit(code);
}
