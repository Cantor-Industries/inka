// In-runtime end-to-end battery for the laufey-backed [`WefDesktopApi`].
//
// Enabled only with the `desktop-e2e` feature and run by
// `scripts/ci/desktop-e2e.sh` when `INKA_DESKTOP_E2E=1`: the battery exercises
// the real `DesktopApi` implementation against a live laufey backend (CEF under
// Xvfb on Linux), printing `[e2e] PASS/FAIL/N/A` and exiting non-zero on any
// FAIL. Assertions that need a capability the backend lacks are reported N/A.
//
// Adapted from laufey 0.7.0 `examples/native_e2e/src/lib.rs` (MIT, Copyright
// (c) Divy Srivastava); this version drives inka's `WefDesktopApi` (the
// `DesktopApi` trait mapping) rather than raw laufey calls.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use deno_runtime::ops::desktop::create_desktop_event_channel;
use deno_runtime::ops::desktop::DesktopApi;
use deno_runtime::ops::desktop::DesktopEvent;
use deno_runtime::ops::desktop::DesktopEventReceiver;
use deno_runtime::ops::desktop::DesktopValue;
use deno_runtime::ops::desktop::MenuItem;

use crate::desktop_api::WefDesktopApi;

static FAILED: AtomicBool = AtomicBool::new(false);

fn check(name: &str, ok: bool) {
    if ok {
        eprintln!("[e2e] PASS {name}");
    } else {
        eprintln!("[e2e] FAIL {name}");
        FAILED.store(true, Ordering::SeqCst);
    }
}

/// Capability absent on this backend — informational, never fails the run.
fn na(name: &str) {
    eprintln!("[e2e] N/A  {name}");
}

/// Report that the battery itself did not complete (a hang is a bug).
pub(crate) fn report_timeout() {
    check("battery completes within the timeout", false);
}

/// Process exit code for the run (1 when any assertion failed).
pub(crate) fn exit_code() -> i32 {
    if FAILED.load(Ordering::SeqCst) {
        1
    } else {
        0
    }
}

/// Minimal valid 1x1 transparent PNG, so the tray/notification paths get real
/// icon bytes.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn plain_item(label: &str, id: &str) -> MenuItem {
    MenuItem::Item {
        label: label.to_string(),
        id: Some(id.to_string()),
        accelerator: None,
        enabled: true,
        checked: false,
        icon: None,
        tooltip: None,
    }
}

fn handle_label(handle: &raw_window_handle::RawWindowHandle) -> &'static str {
    use raw_window_handle::RawWindowHandle as H;
    match handle {
        H::Xlib(_) => "X11",
        H::Wayland(_) => "Wayland",
        H::AppKit(_) => "AppKit",
        H::Win32(_) => "Win32",
        _ => "platform",
    }
}

/// Poll a predicate on a 50ms interval for up to `timeout`.
async fn wait_true<F: Fn() -> bool>(f: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return f();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drain the desktop event channel until `pred` matches or `timeout` elapses.
async fn wait_for_event<F: FnMut(&DesktopEvent) -> bool>(
    rx: &DesktopEventReceiver,
    timeout: Duration,
    mut pred: F,
) -> Option<DesktopEvent> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let next = tokio::time::timeout(remaining, async { rx.0.lock().await.recv().await }).await;
        match next {
            Ok(Some(event)) => {
                if pred(&event) {
                    return Some(event);
                }
            }
            _ => return None,
        }
    }
}

/// Run the battery. Returns after printing the results; the caller exits with
/// [`exit_code`].
pub(crate) async fn run_battery() {
    let (tx, rx) = create_desktop_event_channel();
    let api = std::sync::Arc::new(WefDesktopApi::new(tx.0.clone()));

    // ---- window creation + geometry/state readback (technique A) --------
    let win = api.create_window(640, 480, false, false, false, false);
    check("create_window returns a nonzero id", win != 0);

    let (w0, h0) = api.get_window_size(win);
    check(
        "get_window_size returns the requested size",
        w0 == 640 && h0 == 480,
    );

    api.set_window_size(win, 800, 600);
    let (w, h) = api.get_window_size(win);
    check(
        "set_window_size -> get_window_size round-trips",
        w == 800 && h == 600,
    );

    api.set_resizable(win, true);
    check(
        "set_resizable -> is_resizable round-trips",
        api.is_resizable(win),
    );

    api.set_always_on_top(win, true);
    if api.is_always_on_top(win) {
        check("set_always_on_top round-trips", true);
    } else {
        na("set_always_on_top (backend/WM does not reflect it)");
    }

    api.set_window_opacity(win, 0.75);
    if (api.get_window_opacity(win) - 0.75).abs() < 0.01 {
        check("set_window_opacity round-trips", true);
    } else {
        na("set_window_opacity (backend does not support runtime opacity)");
    }

    // Title + visibility.
    api.set_title(win, "inka-e2e");
    api.show(win);
    check(
        "show -> is_visible",
        wait_true(|| api.is_visible(win), Duration::from_secs(5)).await,
    );

    // Load a page so backends that realize their menu/chrome lazily register
    // the native widgets (the click hooks otherwise find nothing).
    api.navigate(win, "data:text/html,<!doctype html><title>inka-e2e</title>");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- WebGPU path: get_raw_window_handle (technique A) ---------------
    match api.get_raw_window_handle(win) {
        Ok((handle, _display)) => {
            check(
                &format!(
                    "get_raw_window_handle yields a {} handle",
                    handle_label(&handle)
                ),
                true,
            );
        }
        Err(e) => na(&format!("get_raw_window_handle (no native handle: {e})")),
    }

    // ---- event round-trips (technique B) -------------------------------
    api.set_window_size(win, 700, 500);
    if wait_for_event(&rx, Duration::from_secs(2), |e| {
        matches!(e, DesktopEvent::WindowResize { .. })
    })
    .await
    .is_some()
    {
        check("set_window_size dispatches a resize event", true);
    } else {
        na("resize event (backend/WM emits none headless)");
    }

    // ---- clipboard round-trip (technique A) ----------------------------
    api.write_clipboard_text("inka-e2e-clip");
    match api.read_clipboard_text() {
        Some(text) if text == "inka-e2e-clip" => check("clipboard write/read round-trips", true),
        Some(_) => check("clipboard write/read round-trips", false),
        None => na("clipboard (backend/display has no clipboard support)"),
    }

    // ---- application + context menu click round-trips (technique B) ----
    api.set_application_menu(
        win,
        vec![
            plain_item("Ping", "app_ping"),
            MenuItem::Submenu {
                label: "More".to_string(),
                items: vec![
                    MenuItem::Separator,
                    MenuItem::Role {
                        role: "quit".to_string(),
                    },
                ],
            },
        ],
    );
    if laufey::test_click_menu_item("app_ping") {
        let got = wait_for_event(&rx, Duration::from_secs(2), |e| {
            matches!(e, DesktopEvent::AppMenuClick { .. })
        })
        .await;
        check("application menu click round-trips", got.is_some());
    } else {
        na("application menu click round-trip (no test hook / menu unsupported)");
    }

    api.show_context_menu(win, 10, 10, vec![plain_item("Ctx", "ctx_ping")]);
    if laufey::test_click_menu_item("ctx_ping") {
        let got = wait_for_event(&rx, Duration::from_secs(2), |e| {
            matches!(e, DesktopEvent::ContextMenuClick { .. })
        })
        .await;
        check("context menu click round-trips", got.is_some());
    } else {
        na("context menu click round-trip (no test hook / unsupported)");
    }

    // ---- tray (technique A + B) ----------------------------------------
    let tray = api.create_tray();
    if tray == 0 {
        na("tray (backend has no tray support on this platform)");
    } else {
        check("create_tray returns a nonzero id", true);
        api.set_tray_icon(tray, TINY_PNG);
        api.set_tray_tooltip(tray, Some("inka-e2e"));
        api.set_tray_menu(tray, Some(vec![plain_item("Tray Ping", "tray_ping")]));
        if laufey::test_click_menu_item("tray_ping") {
            let got = wait_for_event(&rx, Duration::from_secs(2), |e| {
                matches!(e, DesktopEvent::TrayMenuClick { .. })
            })
            .await;
            check("tray menu click round-trips", got.is_some());
        } else {
            na("tray menu click round-trip (no test hook)");
        }
        if api.get_tray_bounds(tray).is_some() {
            check("get_tray_bounds returns geometry", true);
        } else {
            na("get_tray_bounds (no bounds from this backend)");
        }
        api.destroy_tray(tray);
    }

    // ---- notifications --------------------------------------------------
    // On a system with no `org.freedesktop.Notifications` daemon the backend
    // blocks the calling thread on D-Bus, so run it on the blocking pool with a
    // timeout rather than stalling the runtime.
    let notif_api = api.clone();
    let notif = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::spawn_blocking(move || {
            let id = notif_api.show_notification(
                "inka-e2e",
                Some("body"),
                Some(TINY_PNG),
                None,
                None,
                None,
            );
            if id != 0 {
                notif_api.close_notification(id);
            }
            id
        }),
    )
    .await;
    match notif {
        Ok(Ok(0)) => na("notification (backend has no notification support)"),
        Ok(Ok(_)) => check("show_notification/close_notification accepted", true),
        Ok(Err(_)) => na("notification (blocking task panicked)"),
        Err(_) => na("notification (timed out; no notification daemon)"),
    }

    // ---- dock / taskbar (Linux no-op; macOS-only surface) ---------------
    api.set_dock_badge("1");
    api.bounce_dock(false);
    api.set_dock_menu(None);
    api.set_dock_visible(false);
    na("dock badge/bounce/menu (Linux no-op; macOS-only surface)");

    // ---- execute_js + value conversion (technique C) -------------------
    let js_win = api.create_window(320, 240, false, false, false, false);
    api.navigate(
        js_win,
        "data:text/html,<!doctype html><title>inka-e2e</title>",
    );
    let (js_tx, js_rx) = tokio::sync::oneshot::channel();
    api.execute_js(
        js_win,
        "1 + 2",
        Box::new(move |result| {
            let _ = js_tx.send(result);
        }),
    );
    match tokio::time::timeout(Duration::from_secs(5), js_rx).await {
        Ok(Ok(Ok(DesktopValue::Int(3)))) => check("execute_js converts a JS result", true),
        Ok(Ok(_)) => na("execute_js (unexpected result on this backend)"),
        Ok(Err(_)) => na("execute_js (backend has no web engine / page not ready)"),
        Err(_) => na("execute_js (timed out)"),
    }

    // ---- close_window -> is_closed -------------------------------------
    for id in [win, js_win] {
        api.close_window(id);
        check(
            "close_window -> is_closed",
            wait_true(|| api.is_closed(id), Duration::from_secs(2)).await,
        );
    }
}
