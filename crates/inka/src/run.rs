// inka run: execute a .ts/.js file directly via the installed runtime tuple,
// without building an artifact. Mirrors the launcher's multi-file execution:
// dlopen the chosen runtime and call inka_runtime_run_module_dir with dir =
// the execution root and entry = the file's path relative to it, so relative
// imports, project node_modules, and node built-ins all resolve the way a
// built artifact would.
//
// Permissions mirror `deno run --no-prompt`: deny-by-default, with explicit
// grants only:
//   -A / --allow-all                            everything (trimmed by --deny-*)
//   -R/-W/-N/-E/-S[=list]                       deno short forms (read/write/net/
//                                               env/sys) + long --allow-<cat>
//   -P[=<name>] / --permission-set[=<n>]        a named config permission set
//                                               (bare -P = the config `default`)
//   --deny-<cat>[=list]                         trim an allowed category
// `--` ends option parsing. Only -P/config/flag-grants apply — compile.permissions
// and auto-defaults never do.

use std::env;
use std::ffi::CStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::permissions::{self, Flags, PermFlag};
use crate::Version;

fn usage_text() -> &'static str {
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
}

fn usage() -> ! {
    println!("{}", usage_text());
    exit(0);
}

/// A usage error (unknown option, missing file): print to stderr and exit 2.
fn usage_err(msg: &str) -> ! {
    eprintln!("error: {msg}");
    eprintln!("{}", usage_text());
    exit(2);
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(2);
}

fn parse_flags(args: &[String]) -> (Flags, PathBuf, Vec<String>) {
    let mut f = Flags::default();
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
            "--" => {
                // end of options: the next token is the file (may start with '-')
                if i + 1 >= args.len() {
                    usage_err("`--` must be followed by a file");
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
            _ => {
                match permissions::parse_perm_flag(&mut f, a) {
                    Ok(PermFlag::Once) => {}
                    Ok(PermFlag::ConsumeNext) => {
                        i += 1;
                        if i >= args.len() {
                            fail("--permission-set needs a name (or use bare -P for the `default` set)");
                        }
                        f.permset = Some(args[i].clone());
                    }
                    Ok(PermFlag::Not) => {
                        if a.starts_with('-') {
                            usage_err(&format!("unknown option '{a}'"));
                        }
                        file = Some(PathBuf::from(a));
                        prog = args[i + 1..].to_vec();
                        break;
                    }
                    Err(e) => fail(&e),
                }
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        usage_err("no file given");
    };
    (f, file, prog)
}

/// Choose the runtime .so: newest installed, or an exact --runtime <ver>.
/// Only option tokens before the file (or before `--`) are considered.
fn choose_runtime(args: &[String]) -> (PathBuf, Option<Version>) {
    let dirs = crate::runtime_search_dirs();
    let runtimes = crate::installed_parts_all(&dirs);
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
                        "no runtime {} installed (have: {})",
                        v,
                        runtimes
                            .iter()
                            .map(|(v, _)| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                None => fail(&format!(
                    "--runtime needs a version like 0.266.0, got '{ver}'"
                )),
            }
        }
        i += 1;
    }
    match runtimes.last() {
        Some((v, p)) => (p.clone(), Some(*v)),
        None => {
            let searched = dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            fail(&format!(
                "no runtime installed (searched: {searched}); run `inka update` first"
            ))
        }
    }
}

fn is_project_root(dir: &Path) -> bool {
    dir.join("package.json").is_file() || dir.join("deno.json").is_file()
}

/// The execution root for a file under the cwd. Normally the cwd, but when the
/// cwd has no `node_modules` we climb to the nearest ancestor that both is a
/// project root (`package.json`/`deno.json`) and has a `node_modules`. That
/// matches `inka build`'s Node-style upward `node_modules` resolution for
/// monorepos/workspaces, where dependencies are hoisted to the workspace root.
/// The climb is bounded by a project marker so confinement never broadens to an
/// unrelated directory that merely happens to contain `node_modules`.
fn execution_root_for_cwd(cwd: &Path) -> PathBuf {
    if cwd.join("node_modules").is_dir() {
        return cwd.to_path_buf();
    }
    let mut cur = cwd.to_path_buf();
    loop {
        match cur.parent() {
            Some(parent) if parent != cur => {
                if parent.join("node_modules").is_dir() && is_project_root(parent) {
                    return parent.to_path_buf();
                }
                cur = parent.to_path_buf();
            }
            _ => return cwd.to_path_buf(),
        }
    }
}

fn join_components(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Determine the execution root and the entry's path relative to it. A file
/// under the cwd uses the cwd as root (today's behavior). An outside-cwd file
/// roots at the nearest ancestor project (package.json | deno.json), falling
/// back to the file's own directory.
fn execution_root(cwd: &Path, file: &Path) -> Result<(PathBuf, String), String> {
    let canon =
        fs::canonicalize(file).map_err(|e| format!("cannot resolve {}: {e}", file.display()))?;
    if !canon.is_file() {
        return Err(format!("'{}' is not a file", file.display()));
    }
    let canon_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    if let Ok(rel) = canon.strip_prefix(&canon_cwd) {
        let entry = join_components(rel);
        if entry.is_empty() {
            return Err(format!("cannot run a directory: {}", file.display()));
        }
        // When the cwd has no node_modules but a workspace ancestor does, root
        // there so parent-hoisted dependencies resolve as they do in a build.
        let root = execution_root_for_cwd(&canon_cwd);
        if root == canon_cwd {
            return Ok((cwd.to_path_buf(), entry));
        }
        let entry = canon
            .strip_prefix(&root)
            .map(join_components)
            .unwrap_or(entry);
        return Ok((root, entry));
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

/// Load the runtime with `RTLD_GLOBAL`. Native `.node` addons are `dlopen`ed
/// later by the runtime's `op_napi_open`; they resolve N-API/uv symbols from the
/// global scope, which `RTLD_LOCAL` (the `Library::new` default) hides — the
/// addon then aborts with `undefined symbol: napi_module_register`.
fn load_runtime_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    #[cfg(unix)]
    {
        use libloading::os::unix::{Library as UnixLibrary, RTLD_GLOBAL, RTLD_LAZY};
        unsafe {
            UnixLibrary::open(Some(path), RTLD_LAZY | RTLD_GLOBAL).map(libloading::Library::from)
        }
    }
    #[cfg(not(unix))]
    {
        unsafe { libloading::Library::new(path) }
    }
}

pub(crate) fn cmd_run(args: &[String]) {
    let (flags, file, prog) = parse_flags(args);
    if let Err(e) = permissions::validate(&flags) {
        fail(&e);
    }
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
    let (perms, perm_notes) = permissions::dsl(&root, &flags);
    for n in &perm_notes {
        eprintln!("warning: {n}");
    }

    // Informational guard: config declares build-intent or default permissions
    // but nothing was selected for this run (permissions are still
    // deny-by-default). `run` never auto-applies them.
    if !flags.allow_all
        && flags.permset.is_none()
        && flags.allow.is_empty()
        && flags.deny.is_empty()
    {
        if let Some(hint) = crate::config::build_intent_permission_hint(&root) {
            match &hint.set_name {
                Some(name) => eprintln!(
                    "[inka] note: {} is baked by `inka build`; `inka run` does not apply it. \
                     Use `-P={name}` (or -A/--allow-*) to run with those permissions.",
                    hint.source
                ),
                None => eprintln!(
                    "[inka] note: {} is baked by `inka build`; `inka run` does not apply it. \
                     Pass -A/--allow-* (or define a named set and use -P) to match.",
                    hint.source
                ),
            }
        } else if crate::config::config_has_default_grants(&root) {
            eprintln!(
                "[inka] note: config declares permissions but none were selected for this run; \
                 the program is deny-by-default (use -P, -A, or --allow-*)"
            );
        }
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

    let library = match load_runtime_library(&lib) {
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
        let dsl = permissions::dsl(Path::new("."), &f).0;
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
        let dsl = permissions::dsl(Path::new("."), &f).0;
        assert!(dsl.contains("allow-read=./a,./b"), "{dsl}");

        // "*" wins over explicit lists
        let (f2, _, _) = parsed(&["--allow-net=a.com", "--allow-net", "app.js"]);
        let dsl2 = permissions::dsl(Path::new("."), &f2).0;
        assert!(dsl2.contains("allow-net=*"), "{dsl2}");
        assert!(!dsl2.contains("a.com"), "{dsl2}");
    }

    #[test]
    fn allow_all_trims_by_deny() {
        let (f, _, _) = parsed(&["-A", "--deny-read=x", "app.js"]);
        let dsl = permissions::dsl(Path::new("."), &f).0;
        assert_eq!(dsl, "permissions=all\ndeny-read=x");
    }

    #[test]
    fn no_flags_is_deny_by_default() {
        let (f, _, _) = parsed(&["app.js"]);
        assert_eq!(permissions::dsl(Path::new("."), &f).0, "");
    }

    #[test]
    fn execution_root_climbs_to_workspace_node_modules() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("inkarun-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let sub = base.join("apps/sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(base.join("package.json"), "{}").unwrap();
        fs::create_dir_all(base.join("node_modules")).unwrap();
        fs::write(sub.join("deno.json"), "{}").unwrap();

        // cwd has no node_modules; the workspace root does -> climb.
        assert_eq!(execution_root_for_cwd(&sub), base);

        // cwd with its own node_modules stays put.
        fs::create_dir_all(sub.join("node_modules")).unwrap();
        assert_eq!(execution_root_for_cwd(&sub), sub);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn execution_root_does_not_climb_without_project_marker() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("inkarun-nomarker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let sub = base.join("loose");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(base.join("node_modules")).unwrap();
        assert_eq!(execution_root_for_cwd(&sub), sub);
        let _ = fs::remove_dir_all(&base);
    }
}
