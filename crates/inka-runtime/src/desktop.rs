// Desktop runtime support for the shared `libinka_runtime` cdylib.
//
// The laufey backend loads `libinka_runtime-<tuple>.so` (via the per-app shim)
// and calls `laufey_runtime_init/start/shutdown`. `start` runs `run_desktop`
// below, which:
//   1. resolves the app payload (a directory of bundled files) from
//      `INKA_DESKTOP_PAYLOAD`,
//   2. allocates a loopback port and runs the app's declarative server there
//      (`export default { fetch }`) via deno_runtime's auto-serve,
//   3. creates a laufey window and navigates it to that server once it is up,
//   4. pumps laufey's JS-call event loop until the app quits.
//
// The heavy Deno/V8 engine lives in this shared library, so every desktop app
// on the machine reuses it instead of embedding its own copy.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// The entry module inside the payload directory.
const DEFAULT_ENTRY: &str = "main.js";
const DEFAULT_TITLE: &str = "inka app";
const WINDOW_WIDTH: i32 = 1024;
const WINDOW_HEIGHT: i32 = 720;
/// How long to wait for the app's server to accept connections before giving
/// up and showing the window anyway.
const SERVE_WAIT: Duration = Duration::from_secs(30);
/// Fixed interval between connection attempts.
const SERVE_POLL: Duration = Duration::from_millis(200);
/// Reveal the bootstrap window even if its page never finishes loading
/// (matches the laufey/deno desktop behavior of not leaving the user with an
/// invisible app).
const REVEAL_FALLBACK: Duration = Duration::from_secs(10);
/// The bootstrap window allocated by `inka_desktop_state` before the app
/// module runs. The shell navigates this same window to the loopback URL after
/// the app's server is listening, so a programmatic app gets exactly one window
/// instead of the shell's window plus its own.
static INITIAL_WINDOW: OnceLock<u32> = OnceLock::new();

/// Shared set of displayed window ids, placed in OpState so the HMR runner can
/// refresh every window after a reload (the `DesktopApi` trait doesn't expose
/// it).
pub(crate) struct DesktopOpenWindows(
    pub(crate) std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u32>>>,
);

fn allocate_port(host: &str) -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind((host, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Poll until the app's loopback server accepts a connection.
async fn wait_for_server(host: &str, port: u16) -> bool {
    let deadline = tokio::time::Instant::now() + SERVE_WAIT;
    loop {
        if std::net::TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        // The app may have quit before ever serving (a plain script, or a
        // startup failure); stop waiting so the shell can exit promptly.
        if should_shutdown() {
            return false;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(SERVE_POLL).await;
    }
}

/// Apply a staged `.update` next to the loaded app dylib before the app boots,
/// and roll back a previous update that never reached its `.update-ok`
/// sentinel. This mirrors Deno's `cli/rt_desktop` auto-update swap; the file it
/// patches is the per-app `<App>.so` (from `INKA_DESKTOP_APP_DYLIB`), never the
/// shared runtime.
#[cfg(unix)]
fn apply_pending_update(dylib_path: &Path) -> bool {
    let ext = dylib_path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let update_path = dylib_path.with_extension(format!("{ext}.update"));
    let backup_path = dylib_path.with_extension(format!("{ext}.backup"));
    let sentinel_path = dylib_path.with_extension(format!("{ext}.update-ok"));

    if update_path.exists() {
        // New update pending: back up the live dylib without unlinking it, so a
        // failed swap leaves a working file, then move the update in.
        let _ = std::fs::remove_file(&sentinel_path);
        let _ = std::fs::remove_file(&backup_path);
        let backup_ok = std::fs::hard_link(dylib_path, &backup_path).is_ok()
            || std::fs::copy(dylib_path, &backup_path).is_ok();
        if !backup_ok {
            eprintln!("[inka-desktop] could not stage an update backup");
            return false;
        }
        if std::fs::rename(&update_path, dylib_path).is_err() {
            let tmp = dylib_path.with_extension(format!("{ext}.update.tmp"));
            let copy_ok = std::fs::copy(&update_path, &tmp).is_ok()
                && std::fs::rename(&tmp, dylib_path).is_ok();
            if copy_ok {
                let _ = std::fs::remove_file(&update_path);
            } else {
                let _ = std::fs::remove_file(&tmp);
                let _ = std::fs::remove_file(&backup_path);
                eprintln!("[inka-desktop] failed to apply the staged update; retrying next launch");
            }
        }
        return false;
    }

    if backup_path.exists() && !sentinel_path.exists() {
        eprintln!("[inka-desktop] last update failed to start; rolling back");
        let _ = std::fs::rename(&backup_path, dylib_path);
        return true;
    }
    if backup_path.exists() && sentinel_path.exists() {
        let _ = std::fs::remove_file(&backup_path);
        let _ = std::fs::remove_file(&sentinel_path);
    }
    false
}

#[cfg(not(unix))]
fn apply_pending_update(_dylib_path: &Path) -> bool {
    false
}

/// Install a panic hook that reports Rust panics to the configured
/// `errorReporting.url` (JS errors go through the injected error-reporting JS).
/// Ported from Deno's `cli/rt_desktop`. Best-effort: the original hook still
/// runs, so normal panic output is preserved.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let orig = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some((url, app_version)) = deno_runtime::ops::desktop::error_report_config() {
                let message = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "inka runtime panicked".to_string());
                let location = info
                    .location()
                    .map(|l| format!("at {}:{}:{}", l.file(), l.line(), l.column()));
                let body = deno_core::serde_json::json!({
                    "version": 1,
                    "message": message,
                    "stack": location,
                    "appVersion": app_version,
                    "platform": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                });
                deno_runtime::ops::desktop::send_error_report(url, &body.to_string());
            }
            orig(info);
        }));
    });
}

/// Entry point run by `laufey_runtime_start`.
fn run_desktop() {
    // Apply/roll back any staged per-app update before anything else runs.
    let rolled_back = std::env::var("INKA_DESKTOP_APP_DYLIB")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| apply_pending_update(Path::new(&p)))
        .unwrap_or(false);
    if rolled_back {
        unsafe {
            std::env::set_var("INKA_DESKTOP_ROLLED_BACK", "1");
        }
    }

    // Error-reporting endpoint/version for the runtime's error handlers.
    if let Ok(url) = std::env::var("INKA_DESKTOP_ERROR_REPORTING") {
        if !url.trim().is_empty() {
            deno_runtime::ops::desktop::set_error_report_config(
                url,
                std::env::var("INKA_DESKTOP_APP_VERSION").ok(),
            );
            install_panic_hook();
        }
    }

    // Desktop dev inspector: the CLI (`inka desktop --inspect*`) binds the
    // user-visible port and runs the CDP mux; here we just listen on the
    // internal port it allocated. Creating the global inspector server before
    // the worker boots makes the worker register with it automatically.
    if let Ok(addr) = std::env::var("INKA_DESKTOP_INSPECT_INTERNAL_PORT") {
        match addr.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                let published = deno_runtime::deno_inspector_server::InspectPublishUid {
                    console: false,
                    http: true,
                };
                match deno_runtime::deno_inspector_server::create_inspector_server(
                    addr,
                    "inka-desktop",
                    published,
                ) {
                    Ok(_) => eprintln!("[inka-desktop] inspector server bound on {addr}"),
                    Err(e) => eprintln!("[inka-desktop] inspector server failed: {e}"),
                }
            }
            Err(e) => {
                eprintln!("[inka-desktop] invalid INKA_DESKTOP_INSPECT_INTERNAL_PORT: {e}")
            }
        }
    }

    let payload = std::env::var("INKA_DESKTOP_PAYLOAD").unwrap_or_else(|_| ".".to_string());
    let entry = std::env::var("INKA_DESKTOP_ENTRY").unwrap_or_else(|_| DEFAULT_ENTRY.to_string());
    let host = "127.0.0.1".to_string();

    let port = match allocate_port(&host) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[inka-desktop] failed to allocate a serve port: {e}");
            laufey::quit();
            return;
        }
    };
    let url = format!("http://{host}:{port}/");
    eprintln!("[inka-desktop] payload={payload} entry={entry} url={url}");

    // Publish the address so `Deno.serve()` without an explicit port binds to
    // the same loopback endpoint the window is navigated to. Set before any
    // thread the runtime spawns (setenv is not thread-safe afterwards).
    unsafe {
        std::env::set_var("DENO_SERVE_ADDRESS", format!("tcp:{host}:{port}"));
    }

    // Permissions come from the build manifest when provided; otherwise grant
    // the loopback address the shell serves on and read access to the app's own
    // payload (a generated static server needs it).
    let perms = std::env::var("INKA_DESKTOP_PERMS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("allow-net={host}\nallow-read={payload}"));

    // Run the app's Deno server on its own thread; when it exits, quit.
    // The bootstrap window is created inside the module thread by
    // `inka_desktop_state` (see `INITIAL_WINDOW`), so the shell only has to
    // navigate it once the server is up.
    // Dev-run HMR: `inka desktop --hmr` sets this to the source tree, which the
    // module thread both watches and executes from (the payload *is* the tree).
    let hmr = std::env::var("INKA_DESKTOP_HMR_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(|dir| super::HmrOptions {
            watch_dir: std::path::PathBuf::from(dir),
            vfs_root: std::path::PathBuf::new(),
            reload_url: Some(url.clone()),
        });
    let worker_payload = payload.clone();
    let worker_entry = entry.clone();
    let worker_host = host.clone();
    let worker_perms = perms.clone();
    std::thread::Builder::new()
        .name("inka-desktop-module".to_string())
        .spawn(move || {
            match super::run_tree(
                &worker_payload,
                &worker_entry,
                &[],
                Some(&worker_perms),
                Some((port, worker_host)),
                hmr,
            ) {
                Ok(code) => eprintln!("[inka-desktop] app exited with {code}"),
                Err(e) => eprintln!("[inka-desktop] app error: {e}"),
            }
            laufey::quit();
        })
        .expect("spawn desktop module thread");

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[inka-desktop] failed to build tokio runtime: {e}");
            laufey::quit();
            return;
        }
    };

    rt.block_on(async {
        // Wait for the app's server *first*. The bootstrap window is created on
        // the module thread during worker bootstrap, which strictly precedes the
        // app module starting its server; reading `INITIAL_WINDOW` before this
        // point races the module thread and can spuriously fall back to a
        // second window. On Linux the backend only quits once every window is
        // destroyed, so a stray hidden window keeps the process alive after the
        // visible one is closed.
        let server_up = wait_for_server(&host, port).await;
        let window_id = INITIAL_WINDOW.get().copied();

        if !should_shutdown() {
            match window_id {
                Some(id) => {
                    // Reveal the hidden bootstrap window even if its page never
                    // finishes loading (`create_initial_window` normally shows
                    // it on first page load).
                    tokio::spawn(async move {
                        tokio::time::sleep(REVEAL_FALLBACK).await;
                        laufey::Window::from_id(id).show();
                    });
                    if server_up {
                        laufey::Window::from_id(id).navigate(&url);
                    } else {
                        eprintln!(
                            "[inka-desktop] app server did not come up; showing an empty window"
                        );
                        laufey::Window::from_id(id).show();
                    }
                }
                None => {
                    // The module thread never reached worker bootstrap. Open a
                    // visible fallback so the user still sees a window.
                    eprintln!("[inka-desktop] no bootstrap window; opening a fallback window");
                    let window = laufey::Window::new(WINDOW_WIDTH, WINDOW_HEIGHT);
                    window.show();
                    if server_up {
                        window.navigate(&url);
                    }
                }
            }
        }
        laufey::run().await;
        eprintln!("[inka-desktop] event loop ended");
    });
}

laufey::main!(run_desktop);

/// Whether the laufey backend has asked the runtime to stop (last window
/// closed or the shell requested quit). The desktop event loop polls this so
/// it can tear down cleanly even if the app still has pending work.
pub(crate) fn should_shutdown() -> bool {
    laufey::should_shutdown()
}

/// Post-`DESKTOP_JS` fixup. The cppgc object templates capture their prototype
/// before the vendored script runs, so its `Object.setPrototypeOf(proto,
/// EventTarget.prototype)` never reaches instances. Patch the real prototype
/// on first construction so `addEventListener`/`dispatchEvent` exist.
pub(crate) const DESKTOP_PROTO_FIX_JS: &str = r#"
(() => {
  const ET = globalThis.EventTarget;
  if (typeof ET !== "function") return;
  const fix = (inst) => {
    try {
      const proto = Object.getPrototypeOf(inst);
      if (proto && Object.getPrototypeOf(proto) !== ET.prototype) {
        Object.setPrototypeOf(proto, ET.prototype);
      }
    } catch (_) {}
    return inst;
  };
  const wrap = (obj, name) => {
    const Orig = obj && obj[name];
    if (typeof Orig !== "function") return;
    const P = new Proxy(Orig, {
      construct(target, args, newTarget) {
        return fix(Reflect.construct(target, args, newTarget));
      },
    });
    try {
      Object.defineProperty(obj, name, {
        value: P, writable: true, enumerable: false, configurable: true,
      });
    } catch (_) {}
  };
  wrap(globalThis.Deno, "BrowserWindow");
  wrap(globalThis.Deno, "Tray");
  wrap(globalThis, "Notification");
  if (globalThis.Deno && globalThis.Deno.dock) fix(globalThis.Deno.dock);
})();
"#;

// OpState populated for desktop mode: the laufey-backed `DesktopApi`, the
// shared event channel, and the app-name / initial-window slots the desktop
// ops read. Registered via `WorkerOptions.extensions`.
deno_core::extension!(
    inka_desktop_state,
    state = |state: &mut deno_core::OpState| {
        use deno_runtime::ops::desktop::create_desktop_event_channel;
        use deno_runtime::ops::desktop::DesktopApi;
        use deno_runtime::ops::desktop::DesktopAppName;
        use deno_runtime::ops::desktop::InitialWindowId;
        let (tx, rx) = create_desktop_event_channel();
        let api = crate::desktop_api::WefDesktopApi::new(tx.0.clone());
        let app_name =
            std::env::var("INKA_DESKTOP_APP_NAME").unwrap_or_else(|_| DEFAULT_TITLE.to_string());
        state.put(DesktopAppName(app_name.clone()));
        // Allocate the bootstrap window here, before the app module runs, so
        // `Deno.BrowserWindow` sees an existing window id and the shell can
        // navigate the *same* window instead of opening a second one. Created
        // hidden; `create_initial_window` reveals it on first page load.
        // `get_or_init` keeps the allocation single even if the state extension
        // is initialized more than once.
        let initial_id = *INITIAL_WINDOW.get_or_init(|| {
            let id = api.create_initial_window(WINDOW_WIDTH, WINDOW_HEIGHT);
            api.set_title(id, &app_name);
            id
        });
        state.put(InitialWindowId(std::sync::Mutex::new(Some(initial_id))));
        let open_windows = api.open_windows.clone();
        state.put(std::sync::Arc::new(api) as std::sync::Arc<dyn DesktopApi>);
        state.put(rx);
        state.put(tx);
        state.put(DesktopOpenWindows(open_windows));
        // Auto-update state: the per-app `.so` the ops patch, its version, and
        // whether we rolled back from a failed update on this launch.
        if let Ok(dylib) = std::env::var("INKA_DESKTOP_APP_DYLIB") {
            if !dylib.is_empty() {
                state.put(deno_runtime::ops::desktop::AutoUpdateState {
                    dylib_path: std::path::PathBuf::from(dylib),
                    app_version: std::env::var("INKA_DESKTOP_APP_VERSION").ok(),
                    rolled_back: std::env::var("INKA_DESKTOP_ROLLED_BACK")
                        .map(|v| v == "1")
                        .unwrap_or(false),
                });
            }
        }
    }
);
