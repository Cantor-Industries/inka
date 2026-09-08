// inka run: execute a .ts/.js file directly via the installed runtime tuple,
// without building an artifact. Mirrors the launcher's multi-file execution:
// dlopen the chosen runtime and call inka_runtime_run_module_dir with dir =
// the execution root and entry = the file's path relative to it, so relative
// imports, vendored packages, the default store, and node built-ins all resolve
// the way a built artifact would.
//
// Permissions mirror `deno run --no-prompt`: deny-by-default, with explicit
// grants only:
//   -A / --allow-all                            everything (trimmed by --deny-*)
//   -R/-W/-N/-E/-S[=list]                       deno short forms (read/write/net/
//                                               env/sys) + long --allow-<cat>
//   -P [<name>] / --permission-set[=<n>]        a named config permission set
//                                               (bare -P = the config `default`)
//   --deny-<cat>[=list]                         trim an allowed category
// `--` ends option parsing. Only -P/config/flag-grants apply — compile.permissions
// and auto-defaults never do.

use std::env;
use std::ffi::CStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::Version;

const CATEGORIES: [&str; 7] = ["read", "write", "net", "env", "run", "sys", "ffi"];

fn usage() -> ! {
    eprintln!(
        "usage: inka run [options] <file> [args...]\n\
         \n\
         executes <file> (.ts/.js/...) via the installed runtime. Options must precede the\n\
         file; anything after <file> (or after `--`) is passed to the program as its\n\
         arguments.\n\
         \n\
         permissions (deny by default; no prompting):\n\
         \x20 -A, --allow-all            allow everything (trimmed by any --deny-*)\n\
         \x20 -R, -W, -N, -E, -S         allow read/write/net/env/sys (whole category)\n\
         \x20 -R=<list>, -N=<list>, ...   same, scoped to the given list\n\
         \x20     --allow-<cat>[=list]    grant category read|write|net|env|run|sys|ffi\n\
         \x20     --deny-<cat>[=list]     deny within an allowed category\n\
         \x20 -P[=<name>], --permission-set[=<name>]\n\
         \x20                             apply a named permission set from the config\n\
         \x20                             (bare -P uses the `default` set)\n\
         \x20     --runtime <ver>        use a specific installed runtime tuple\n\
         \x20     --                     end of options (file may start with '-')\n\
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

fn cat_for_short(short: char) -> Option<&'static str> {
    match short {
        'R' => Some("read"),
        'W' => Some("write"),
        'N' => Some("net"),
        'E' => Some("env"),
        'S' => Some("sys"),
        _ => None,
    }
}

/// Merge repeated per-category entries: `*` wins over lists; explicit lists are
/// joined with commas (one DSL line per category).
fn merge_cat(entries: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (cat, list) in entries {
        match out.iter_mut().find(|(c, _)| c == cat) {
            Some((_, cur)) => {
                if list == "*" {
                    *cur = "*".to_string();
                } else if cur != "*" {
                    if !cur.is_empty() {
                        cur.push(',');
                    }
                    cur.push_str(list);
                }
            }
            None => out.push((cat.clone(), list.clone())),
        }
    }
    out
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
            "--" => {
                // end of options: the next token is the file (may start with '-')
                if i + 1 >= args.len() {
                    usage();
                }
                file = Some(PathBuf::from(&args[i + 1]));
                prog = args[i + 2..].to_vec();
                break;
            }
            "--runtime" => {
                // consume <ver> (validated by choose_runtime); skip it here
                i += 1;
                if i >= args.len() {
                    fail("--runtime needs a version like 0.266.0");
                }
            }
            _ if a.len() >= 2 && cat_for_short(a.as_bytes()[1] as char).is_some() => {
                let short = a.as_bytes()[1] as char;
                let cat = cat_for_short(short).unwrap().to_string();
                let body = &a[2..];
                let list = if body.is_empty() {
                    "*".to_string()
                } else if let Some(v) = body.strip_prefix('=') {
                    if v.is_empty() {
                        "*".to_string()
                    } else {
                        v.to_string()
                    }
                } else {
                    fail(&format!(
                        "option '{a}' takes an optional '=<list>' value (e.g. -{short}=./data)"
                    ));
                };
                f.allow.push((cat, list));
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

/// Render the permission DSL string from the parsed flags (against `root`,
/// whose config supplies any -P-selected permission set).
fn permission_dsl(root: &Path, f: &Flags) -> String {
    if f.allow_all {
        let mut lines = vec!["permissions=all".to_string()];
        for (cat, list) in merge_cat(&f.deny) {
            lines.push(format!("deny-{cat}={list}"));
        }
        return lines.join("\n");
    }
    if let Some(name) = &f.permset {
        let (dsl, notes) = crate::config::permission_set_dsl(root, name);
        for n in &notes {
            eprintln!("warning: {n}");
        }
        return dsl;
    }
    let mut lines = Vec::new();
    for (cat, list) in merge_cat(&f.allow) {
        lines.push(format!("allow-{cat}={list}"));
    }
    for (cat, list) in merge_cat(&f.deny) {
        lines.push(format!("deny-{cat}={list}"));
    }
    lines.join("\n")
}

/// Choose the runtime .so: newest installed, or an exact --runtime <ver>.
/// Only option tokens before the file (or before `--`) are considered.
fn choose_runtime(args: &[String]) -> (PathBuf, Option<Version>) {
    let dir = crate::runtime_dir(None);
    let (runtimes, _) = crate::installed_parts(&dir);
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') || a == "--" {
            break; // first positional (or the `--` terminator) ends the options
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

fn is_project_root(dir: &Path) -> bool {
    dir.join("vendored").is_dir()
        || dir.join("package.json").is_file()
        || dir.join("deno.json").is_file()
}

fn join_components(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Determine the execution root and the entry's path relative to it. A file
/// under the cwd uses the cwd as root (today's behavior). An outside-cwd file
/// roots at the nearest ancestor project (vendored/ | package.json | deno.json),
/// falling back to the file's own directory.
fn execution_root(cwd: &Path, file: &Path) -> Result<(PathBuf, String), String> {
    let canon = fs::canonicalize(file)
        .map_err(|e| format!("cannot resolve {}: {e}", file.display()))?;
    if !canon.is_file() {
        return Err(format!("'{}' is not a file", file.display()));
    }
    let canon_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    if let Ok(rel) = canon.strip_prefix(&canon_cwd) {
        let entry = join_components(rel);
        if entry.is_empty() {
            return Err(format!("cannot run a directory: {}", file.display()));
        }
        return Ok((cwd.to_path_buf(), entry));
    }

    // Outside the cwd: find the nearest ancestor project root.
    let file_dir = match canon.parent() {
        Some(d) => d.to_path_buf(),
        None => return Err(format!("no parent directory for {}", file.display())),
    };
    let mut root = file_dir.clone();
    let mut cur = file_dir;
    loop {
        if is_project_root(&cur) {
            root = cur;
            break;
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => break,
        }
    }
    let rel = canon
        .strip_prefix(&root)
        .map_err(|_| format!("cannot relate {} to {}", file.display(), root.display()))?;
    let entry = join_components(rel);
    Ok((root, entry))
}

fn set_default_env(lib: &Path, root: &Path) {
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
    // INKA_VENDOR: only a vendored/ dir under the execution root counts.
    let vroot = root.join("vendored");
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
    let (root, entry) = match execution_root(&cwd, &file) {
        Ok(r) => r,
        Err(e) => fail(&e),
    };
    let perms = permission_dsl(&root, &flags);

    // Informational guard: config declares a default set but nothing was
    // selected for this run (permissions are still deny-by-default).
    if !flags.allow_all
        && flags.permset.is_none()
        && flags.allow.is_empty()
        && flags.deny.is_empty()
        && crate::config::config_has_default_grants(&root)
    {
        eprintln!(
            "[inka] note: config declares permissions but none were selected for this run; \
             the program is deny-by-default (use -P, -A, or --allow-*)"
        );
    }

    let (lib, chosen) = choose_runtime(args);
    if env::var_os("INKA_DEBUG").is_some() {
        eprintln!(
            "[inka] running {} in {} with runtime {}",
            entry,
            root.display(),
            chosen.map(|v| v.to_string()).unwrap_or_default()
        );
    }
    set_default_env(&lib, &root);

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

        let dir_c = match std::ffi::CString::new(root.to_string_lossy().into_owned()) {
            Ok(c) => c,
            Err(_) => exit(2),
        };
        let entry_c = match std::ffi::CString::new(entry.clone()) {
            Ok(c) => c,
            Err(_) => exit(2),
        };
        let perms_c =
            std::ffi::CString::new(perms).unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
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
            eprintln!(
                "[inka] runtime error message: {}",
                CStr::from_ptr(err_msg).to_string_lossy()
            );
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
    fn double_dash_allows_dash_prefixed_file() {
        let (f, file, prog) = parsed(&["--", "-weird.js", "arg"]);
        assert_eq!(file, PathBuf::from("-weird.js"));
        assert_eq!(prog, vec!["arg"]);
        assert!(!f.allow_all && f.permset.is_none());
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
    fn short_flags_map_to_categories() {
        let (f, file, _) = parsed(&["-R", "-W", "-N", "-E", "-S", "app.js"]);
        assert_eq!(file, PathBuf::from("app.js"));
        let cats: Vec<&str> = f.allow.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(cats, vec!["read", "write", "net", "env", "sys"]);
        assert!(f.allow.iter().all(|(_, l)| l == "*"));
    }

    #[test]
    fn short_flag_with_value() {
        let (f, _, _) = parsed(&["-R=./data", "app.js"]);
        assert_eq!(f.allow, vec![("read".to_string(), "./data".to_string())]);
    }

    #[test]
    fn repeated_flags_merge_per_category() {
        let (f, _, _) = parsed(&["--allow-read=./a", "--allow-read=./b", "app.js"]);
        let dsl = permission_dsl(Path::new("."), &f);
        assert!(dsl.contains("allow-read=./a,./b"), "{dsl}");

        // "*" wins over explicit lists
        let (f2, _, _) = parsed(&["--allow-net=a.com", "--allow-net", "app.js"]);
        let dsl2 = permission_dsl(Path::new("."), &f2);
        assert!(dsl2.contains("allow-net=*"), "{dsl2}");
        assert!(!dsl2.contains("a.com"), "{dsl2}");
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
