// inka run: execute a .ts/.js file directly via the installed runtime tuple,
// without building an artifact. Mirrors the launcher's multi-file execution:
// dlopen the chosen runtime and call inka_runtime_run_module_dir with
// dir = the current working directory and entry = the file's cwd-relative path,
// so relative imports, vendored packages, the default store, and node built-ins
// all resolve the way a built artifact would.
//
// Permissions mirror `deno run`: deny-by-default, with explicit grants only:
//   -A / --allow-all                      everything (trimmed by any --deny-*)
//   -P [<name>] / --permission-set[=<n>]  a named config permission set
//                                         (bare -P = the config `default` set)
//   --allow-<cat>[=list] / --deny-<cat>[=list]   granular per-category grants
// Only -P/config/flag-grants apply — compile.permissions/auto-defaults never do.

use std::env;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::embed;
use crate::Version;

const CATEGORIES: [&str; 7] = ["read", "write", "net", "env", "run", "sys", "ffi"];

fn usage() -> ! {
    eprintln!(
        "usage: inka run [-A] [-P[=name]] [--allow-<cat>[=list]|--deny-<cat>[=list]]... <file> [args...]\n\
         \n\
         executes <file> (.ts/.js/...) via the installed runtime. Options must precede the\n\
         file; anything after <file> is passed to the program as its arguments.\n\
         \n\
         permissions (deny by default):\n\
         \x20 -A, --allow-all            allow everything (trimmed by any --deny-*)\n\
         \x20 -P[=<name>], --permission-set[=<name>]\n\
         \x20                             apply a named permission set from the config\n\
         \x20                             (bare -P uses the `default` set)\n\
         \x20     --allow-<cat>[=list]    grant category read|write|net|env|run|sys|ffi\n\
         \x20     --deny-<cat>[=list]     deny within an allowed category\n\
         \x20 -h, --help                  show this help"
    );
    exit(0);
}

struct Flags {
    allow_all: bool,
    permset: Option<String>,
    allow: Vec<(String, String)>, // (cat, list or "*")
    deny: Vec<(String, String)>,
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(2);
}

fn parse_flags(args: &[String]) -> (Flags, PathBuf, Vec<String>) {
    let mut f = Flags {
        allow_all: false,
        permset: None,
        allow: Vec::new(),
        deny: Vec::new(),
    };
    let mut file: Option<PathBuf> = None;
    let mut prog: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if file.is_some() {
            prog.push(a.clone());
            i += 1;
            continue;
        }
        match a.as_str() {
            "-h" | "--help" => usage(),
            "-A" | "--allow-all" => f.allow_all = true,
            "-P" => f.permset = Some("default".to_string()),
            "--runtime" => {
                // consume <ver> (validated by choose_runtime); skip it here
                i += 1;
                if i >= args.len() {
                    fail("--runtime needs a version like 0.266.0");
                }
            }
            _ if a.starts_with("-P=") => f.permset = Some(a["-P=".len()..].to_string()),
            "--permission-set" => {
                i += 1;
                if i >= args.len() {
                    fail("--permission-set needs a name (or use bare -P for the `default` set)");
                }
                f.permset = Some(args[i].clone());
            }
            _ if a.starts_with("--permission-set=") => {
                f.permset = Some(a["--permission-set=".len()..].to_string());
            }
            _ if a.starts_with("--allow-") || a.starts_with("--deny-") => {
                let deny = a.starts_with("--deny-");
                let prefix = if deny { "--deny-" } else { "--allow-" };
                let body = &a[prefix.len()..];
                let (cat, list) = match body.split_once('=') {
                    Some((c, v)) => (c.to_string(), v.to_string()),
                    None => (body.to_string(), "*".to_string()),
                };
                if !CATEGORIES.contains(&cat.as_str()) {
                    fail(&format!(
                        "unknown permission category '{cat}' (expected one of {})",
                        CATEGORIES.join(", ")
                    ));
                }
                let list = if list.is_empty() { "*".to_string() } else { list };
                let slot = if deny { &mut f.deny } else { &mut f.allow };
                slot.push((cat, list));
            }
            _ if a.starts_with('-') => {
                eprintln!("error: unknown option '{a}'");
                usage();
            }
            _ => {
                file = Some(PathBuf::from(a));
                prog = args[i + 1..].to_vec();
                break;
            }
        }
        i += 1;
    }
    let Some(file) = file else { usage() };
    (f, file, prog)
}

/// Render the permission DSL string from the parsed flags.
fn permission_dsl(cwd: &Path, f: &Flags) -> String {
    if f.allow_all {
        let mut lines = vec!["permissions=all".to_string()];
        for (cat, list) in &f.deny {
            lines.push(format!("deny-{cat}={list}"));
        }
        return lines.join("\n");
    }
    if let Some(name) = &f.permset {
        let (dsl, notes) = crate::config::permission_set_dsl(cwd, name);
        for n in &notes {
            eprintln!("warning: {n}");
        }
        return dsl;
    }
    let mut lines = Vec::new();
    for (cat, list) in &f.allow {
        lines.push(format!("allow-{cat}={list}"));
    }
    for (cat, list) in &f.deny {
        lines.push(format!("deny-{cat}={list}"));
    }
    lines.join("\n")
}

/// Choose the runtime .so: newest installed, or an exact --runtime <ver>.
/// Only option tokens before the file are considered (program args after the
/// file are never scanned).
fn choose_runtime(args: &[String]) -> (PathBuf, Option<Version>) {
    let dir = crate::runtime_dir(None);
    let (runtimes, _) = crate::installed_parts(&dir);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') {
            break; // first positional = the file
        }
        if a == "--runtime" {
            let ver = args.get(i + 1).map(String::as_str).unwrap_or("");
            match crate::parse_version(ver) {
                Some(v) => {
                    for (rv, p) in &runtimes {
                        if *rv == v {
                            return (p.clone(), Some(v));
                        }
                    }
                    fail(&format!(
                        "no runtime {} installed in {} (have: {})",
                        v,
                        dir.display(),
                        runtimes
                            .iter()
                            .map(|(v, _)| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                None => fail(&format!("--runtime needs a version like 0.266.0, got '{ver}'")),
            }
        }
        i += 1;
    }
    match runtimes.last() {
        Some((v, p)) => (p.clone(), Some(v.clone())),
        None => fail(&format!(
            "no runtime installed in {}; run `inka install <version>` first",
            dir.display()
        )),
    }
}

fn set_default_env(lib: &Path) {
    // INKA_STORE defaults to a `store/` dir next to the runtime when present.
    if env::var_os("INKA_STORE").is_none() {
        if let Some(dir) = lib.parent() {
            let candidate = dir.join("store");
            if candidate.is_dir() {
                env::set_var("INKA_STORE", &candidate);
            }
        }
    }
    // INKA_RESOLVER defaults to the newest installed resolver.
    if env::var_os("INKA_RESOLVER").is_none() {
        let dir = crate::runtime_dir(None);
        let (_, resolvers) = crate::installed_parts(&dir);
        if let Some((_, p)) = resolvers.last() {
            env::set_var("INKA_RESOLVER", p);
        } else {
            eprintln!(
                "[inka] warning: no inka resolver installed; vendored/package resolution will fail"
            );
        }
    }
    // INKA_VENDOR: only an embedded/this-project vendored/ dir counts.
    let vroot = crate::vendor::vendor_root();
    if vroot.is_dir() {
        env::set_var("INKA_VENDOR", &vroot);
    } else {
        env::remove_var("INKA_VENDOR");
    }
}

fn validate_flags(f: &Flags) {
    if f.allow_all && (f.permset.is_some() || !f.allow.is_empty()) {
        fail("--allow-all cannot be combined with -P/--permission-set or --allow-*");
    }
    if f.permset.is_some() && (!f.allow.is_empty() || !f.deny.is_empty()) {
        fail("-P/--permission-set cannot be combined with --allow-*/--deny-*");
    }
}

pub(crate) fn cmd_run(args: &[String]) {
    let (flags, file, prog) = parse_flags(args);
    validate_flags(&flags);
    if !file.is_file() {
        fail(&format!("source file not found: {}", file.display()));
    }
    let cwd = env::current_dir().unwrap_or_else(|e| {
        eprintln!("error: cannot determine current directory: {e}");
        exit(2);
    });
    // The entry must live inside the cwd tree so relative-import and vendored
    // resolution stay coherent (same constraint as `inka build`).
    let entry = match embed::rel_from_cwd(&cwd, &file) {
        Ok(r) => r,
        Err(e) => fail(&e),
    };
    let perms = permission_dsl(&cwd, &flags);

    let (lib, chosen) = choose_runtime(args);
    if env::var_os("INKA_DEBUG").is_some() {
        eprintln!(
            "[inka] running {} in {} with runtime {}",
            entry,
            cwd.display(),
            chosen.map(|v| v.to_string()).unwrap_or_default()
        );
    }
    set_default_env(&lib);

    let library = match unsafe { libloading::Library::new(&lib) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[inka] failed to load {}: {e}", lib.display());
            exit(1);
        }
    };

    unsafe {
        let create: libloading::Symbol<unsafe extern "C" fn() -> *mut std::ffi::c_void> =
            match library.get(b"inka_runtime_create") {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "[inka] runtime {} is missing inka_runtime_create; reinstall it",
                        lib.display()
                    );
                    exit(4);
                }
            };
        let destroy: libloading::Symbol<unsafe extern "C" fn(*mut std::ffi::c_void)> = library
            .get(b"inka_runtime_destroy")
            .expect("missing inka_runtime_destroy");
        type FnRunDir = unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            std::ffi::c_int,
            *const *const std::ffi::c_char,
            *mut std::ffi::c_int,
            *mut *mut std::ffi::c_char,
            *const std::ffi::c_char,
        ) -> std::ffi::c_int;
        let run_dir: libloading::Symbol<FnRunDir> = match library
            .get(b"inka_runtime_run_module_dir")
        {
            Ok(s) => s,
            Err(_) => {
                eprintln!(
                    "[inka] runtime {} does not support `run` (missing inka_runtime_run_module_dir); install a newer runtime",
                    lib.display()
                );
                exit(4);
            }
        };

        let dir_c = match std::ffi::CString::new(cwd.to_string_lossy().into_owned()) {
            Ok(c) => c,
            Err(_) => exit(2),
        };
        let entry_c = match std::ffi::CString::new(entry.clone()) {
            Ok(c) => c,
            Err(_) => exit(2),
        };
        let perms_c = std::ffi::CString::new(perms).unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
        let argv: Vec<std::ffi::CString> = prog
            .iter()
            .map(|a| std::ffi::CString::new(a.as_str()).expect("nul byte in arg"))
            .collect();
        let mut argv_ptrs: Vec<*const std::ffi::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let rt = create();
        let mut exit_code: std::ffi::c_int = 0;
        let mut err_msg: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = run_dir(
            rt,
            dir_c.as_ptr(),
            entry_c.as_ptr(),
            argv.len() as std::ffi::c_int,
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
            exit(rc);
        }
        exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> (Flags, PathBuf, Vec<String>) {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_flags(&v)
    }

    #[test]
    fn everything_after_the_file_is_program_args() {
        let (f, file, prog) = parsed(&["hello.js", "-A", "--allow-read=x", "tail"]);
        assert_eq!(file, PathBuf::from("hello.js"));
        assert!(!f.allow_all && f.permset.is_none() && f.allow.is_empty());
        assert_eq!(prog, vec!["-A", "--allow-read=x", "tail"]);
    }

    #[test]
    fn granular_flags_render_into_dsl() {
        let (f, file, prog) = parsed(&[
            "--allow-read=data.txt",
            "--deny-net=1.2.3.4",
            "--allow-net",
            "app.js",
        ]);
        assert_eq!(file, PathBuf::from("app.js"));
        assert!(prog.is_empty());
        let dsl = permission_dsl(Path::new("."), &f);
        assert!(dsl.contains("allow-read=data.txt"), "{dsl}");
        assert!(dsl.contains("allow-net=*"), "{dsl}");
        assert!(dsl.contains("deny-net=1.2.3.4"), "{dsl}");
    }

    #[test]
    fn allow_all_trims_by_deny() {
        let (f, _, _) = parsed(&["-A", "--deny-read=x", "app.js"]);
        let dsl = permission_dsl(Path::new("."), &f);
        assert_eq!(dsl, "permissions=all\ndeny-read=x");
    }

    #[test]
    fn no_flags_is_deny_by_default() {
        let (f, _, _) = parsed(&["app.js"]);
        assert_eq!(permission_dsl(Path::new("."), &f), "");
    }
}
