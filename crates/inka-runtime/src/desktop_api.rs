// Adapted from Deno 2.9.7 `cli/rt_desktop/lib.rs` (MIT, Copyright 2018-2026
// the Deno authors). Pinned to laufey 0.7.0 / deno_runtime 0.267.0.

use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use deno_runtime::ops::desktop::register_bind_call;
use deno_runtime::ops::desktop::DesktopApi;
use deno_runtime::ops::desktop::DesktopEvent;
use deno_runtime::ops::desktop::DesktopEventTx;
use deno_runtime::ops::desktop::DesktopValue;
use deno_runtime::ops::desktop::MenuItem;
use deno_runtime::ops::desktop::PendingBindResponses;
use deno_runtime::ops::desktop::PermissionState;
use deno_runtime::ops::desktop::MAX_DEPTH;

/// Laufey-backed implementation of [`DesktopApi`].
pub(crate) struct WefDesktopApi {
    pub(crate) event_tx: DesktopEventTx,
    pub(crate) pending_responses: PendingBindResponses,
    pub(crate) closed_windows: Arc<Mutex<HashSet<u32>>>,
    /// IDs of every window currently displayed. Shared with the HMR reload
    /// callback so it can refresh all windows, not just the initial one.
    pub(crate) open_windows: Arc<Mutex<HashSet<u32>>>,
    pub(crate) trays: Arc<Mutex<HashMap<u32, laufey::TrayIcon>>>,
    pub(crate) notifications: Arc<Mutex<HashMap<u32, laufey::NotificationHandle>>>,
    /// Singleton for the unified-mux DevTools window. Without this, every
    /// `openDevtools()` call would spawn another DevTools window.
    pub(crate) devtools_window: Mutex<Option<u32>>,
}

impl WefDesktopApi {
    pub(crate) fn new(event_tx: DesktopEventTx) -> Self {
        Self {
            event_tx,
            pending_responses: PendingBindResponses::new(),
            closed_windows: Arc::new(Mutex::new(HashSet::new())),
            open_windows: Arc::new(Mutex::new(HashSet::new())),
            trays: Arc::new(Mutex::new(HashMap::new())),
            notifications: Arc::new(Mutex::new(HashMap::new())),
            devtools_window: Mutex::new(None),
        }
    }

    /// Set up all event handlers on a newly created window, wiring events
    /// into the shared event channel.
    fn setup_window_events(
        &self,
        window: laufey::Window,
        show_on_first_load: bool,
    ) -> laufey::Window {
        let kb_tx = self.event_tx.clone();
        let mouse_click_tx = self.event_tx.clone();
        let mouse_move_tx = self.event_tx.clone();
        let wheel_tx = self.event_tx.clone();
        let cursor_tx = self.event_tx.clone();
        let focus_tx = self.event_tx.clone();
        let resize_tx = self.event_tx.clone();
        let move_tx = self.event_tx.clone();
        let page_load_tx = self.event_tx.clone();
        let close_tx = self.event_tx.clone();
        let closed_windows = self.closed_windows.clone();
        let open_windows_on_close = self.open_windows.clone();
        let shown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        window
            .on_keyboard_event(move |ev| {
                let _ = kb_tx.try_send(DesktopEvent::KeyboardEvent {
                    window_id: ev.window_id,
                    r#type: match ev.state {
                        laufey::KeyState::Pressed => "keydown".to_string(),
                        laufey::KeyState::Released => "keyup".to_string(),
                    },
                    key: ev.key,
                    code: ev.code,
                    shift: ev.modifiers.shift,
                    control: ev.modifiers.control,
                    alt: ev.modifiers.alt,
                    meta: ev.modifiers.meta,
                    repeat: ev.repeat,
                });
            })
            .on_mouse_click(move |ev| {
                let _ = mouse_click_tx.try_send(DesktopEvent::MouseClick {
                    window_id: ev.window_id,
                    state: match ev.state {
                        laufey::MouseButtonState::Pressed => "pressed".to_string(),
                        laufey::MouseButtonState::Released => "released".to_string(),
                    },
                    button: match ev.button {
                        laufey::MouseButton::Left => 0,
                        laufey::MouseButton::Middle => 1,
                        laufey::MouseButton::Right => 2,
                        laufey::MouseButton::Back => 3,
                        laufey::MouseButton::Forward => 4,
                        laufey::MouseButton::Other(n) => n,
                    },
                    client_x: ev.x,
                    client_y: ev.y,
                    shift: ev.modifiers.shift,
                    control: ev.modifiers.control,
                    alt: ev.modifiers.alt,
                    meta: ev.modifiers.meta,
                    click_count: ev.click_count,
                });
            })
            .on_mouse_move(move |ev| {
                let _ = mouse_move_tx.try_send(DesktopEvent::MouseMove {
                    window_id: ev.window_id,
                    client_x: ev.x,
                    client_y: ev.y,
                    shift: ev.modifiers.shift,
                    control: ev.modifiers.control,
                    alt: ev.modifiers.alt,
                    meta: ev.modifiers.meta,
                });
            })
            .on_wheel(move |ev| {
                let _ = wheel_tx.try_send(DesktopEvent::Wheel {
                    window_id: ev.window_id,
                    delta_x: ev.delta_x,
                    delta_y: ev.delta_y,
                    delta_mode: match ev.delta_mode {
                        laufey::WheelDeltaMode::Pixel => 0,
                        laufey::WheelDeltaMode::Line => 1,
                        laufey::WheelDeltaMode::Page => 2,
                    },
                    client_x: ev.x,
                    client_y: ev.y,
                    shift: ev.modifiers.shift,
                    control: ev.modifiers.control,
                    alt: ev.modifiers.alt,
                    meta: ev.modifiers.meta,
                });
            })
            .on_cursor_enter_leave(move |ev| {
                let _ = cursor_tx.try_send(DesktopEvent::CursorEnterLeave {
                    window_id: ev.window_id,
                    entered: ev.entered,
                    client_x: ev.x,
                    client_y: ev.y,
                    shift: ev.modifiers.shift,
                    control: ev.modifiers.control,
                    alt: ev.modifiers.alt,
                    meta: ev.modifiers.meta,
                });
            })
            .on_focused(move |ev| {
                let _ = focus_tx.try_send(DesktopEvent::FocusChanged {
                    window_id: ev.window_id,
                    focused: ev.focused,
                });
            })
            .on_resize(move |ev| {
                let _ = resize_tx.try_send(DesktopEvent::WindowResize {
                    window_id: ev.window_id,
                    width: ev.width,
                    height: ev.height,
                });
            })
            .on_move(move |ev| {
                let _ = move_tx.try_send(DesktopEvent::WindowMove {
                    window_id: ev.window_id,
                    x: ev.x,
                    y: ev.y,
                });
            })
            .on_page_load(move |ev| {
                if show_on_first_load && !shown.swap(true, Ordering::AcqRel) {
                    laufey::Window::from_id(ev.window_id).show();
                }
                let _ = page_load_tx.try_send(DesktopEvent::PageLoad {
                    window_id: ev.window_id,
                });
            })
            .on_close_requested(move |ev| {
                closed_windows.lock().unwrap().insert(ev.window_id);
                open_windows_on_close.lock().unwrap().remove(&ev.window_id);
                let _ = close_tx.try_send(DesktopEvent::CloseRequested {
                    window_id: ev.window_id,
                });
                // Since laufey 0.7.0 a registered close-requested handler *defers*
                // the close: the window stays open until `Window::close()` is called.
                // The JS "close" event is a plain notification (not cancelable), so
                // complete the close here to keep the native close button working.
                laufey::Window::from_id(ev.window_id).close();
            })
    }

    /// Create the bootstrap window for the app. Unlike windows constructed from
    /// JS via `BrowserWindow`, this one is created *hidden* and revealed only once
    /// its first navigation has finished loading (wired here via `on_page_load`).
    ///
    /// The runtime navigates this window to the app's `http://127.0.0.1:PORT`
    /// URL only after the loopback server is listening, so creating it visible
    /// up front would leave the user staring at an empty webview — which paints
    /// solid black on Wayland, where the compositor presents the pre-load frame
    /// verbatim. Deferring the reveal to load-finished means the window's first
    /// visible frame already has content. See
    /// https://github.com/denoland/deno/issues/35530.
    pub(crate) fn create_initial_window(&self, width: i32, height: i32) -> u32 {
        let window = laufey::Window::new_with_options(
            width,
            height,
            laufey::WindowOptions {
                frameless: false,
                no_activate: false,
                transparent_titlebar: false,
                hidden: true,
                transparent: false,
            },
        );
        let window = self.setup_window_events(window, true);
        let id = window.id();

        self.open_windows.lock().unwrap().insert(id);
        id
    }
}

impl DesktopApi for WefDesktopApi {
    fn create_window(
        &self,
        width: i32,
        height: i32,
        frameless: bool,
        no_activate: bool,
        transparent_titlebar: bool,
        transparent: bool,
    ) -> u32 {
        let window = laufey::Window::new_with_options(
            width,
            height,
            laufey::WindowOptions {
                frameless,
                no_activate,
                transparent_titlebar,
                hidden: false,
                transparent,
            },
        );
        let window = self.setup_window_events(window, false);
        let id = window.id();
        self.open_windows.lock().unwrap().insert(id);
        id
    }

    fn close_window(&self, window_id: u32) {
        self.closed_windows.lock().unwrap().insert(window_id);
        self.open_windows.lock().unwrap().remove(&window_id);
        laufey::Window::from_id(window_id).close();
    }

    fn is_closed(&self, window_id: u32) -> bool {
        self.closed_windows.lock().unwrap().contains(&window_id)
    }

    fn set_title(&self, window_id: u32, title: &str) {
        laufey::Window::from_id(window_id).set_title(title);
    }

    fn get_window_size(&self, window_id: u32) -> (i32, i32) {
        laufey::Window::from_id(window_id).get_size()
    }

    fn set_window_size(&self, window_id: u32, width: i32, height: i32) {
        laufey::Window::from_id(window_id).set_size(width, height);
    }

    fn get_window_position(&self, window_id: u32) -> (i32, i32) {
        laufey::Window::from_id(window_id).get_position()
    }

    fn set_window_position(&self, window_id: u32, x: i32, y: i32) {
        laufey::Window::from_id(window_id).set_position(x, y);
    }

    fn is_resizable(&self, window_id: u32) -> bool {
        laufey::Window::from_id(window_id).get_resizable()
    }

    fn set_resizable(&self, window_id: u32, resizable: bool) {
        laufey::Window::from_id(window_id).set_resizable(resizable);
    }

    fn is_always_on_top(&self, window_id: u32) -> bool {
        laufey::Window::from_id(window_id).get_always_on_top()
    }

    fn set_always_on_top(&self, window_id: u32, always_on_top: bool) {
        laufey::Window::from_id(window_id).set_always_on_top(always_on_top);
    }

    fn get_window_opacity(&self, window_id: u32) -> f64 {
        laufey::Window::from_id(window_id).get_opacity()
    }

    fn set_window_opacity(&self, window_id: u32, opacity: f64) {
        laufey::Window::from_id(window_id).set_opacity(opacity);
    }

    fn is_visible(&self, window_id: u32) -> bool {
        laufey::Window::from_id(window_id).get_visible()
    }

    fn show(&self, window_id: u32) {
        laufey::Window::from_id(window_id).show();
    }

    fn hide(&self, window_id: u32) {
        laufey::Window::from_id(window_id).hide();
    }

    fn focus(&self, window_id: u32) {
        laufey::Window::from_id(window_id).focus();
    }

    fn open_devtools(&self, window_id: u32, renderer: bool, deno: bool) {
        if let Ok(mux) = env::var("DENO_DESKTOP_MUX_WS") {
            // Reuse an existing DevTools window when one is already open, so
            // repeated `openDevtools()` calls don't pile up windows.
            if let Some(id) = *self.devtools_window.lock().unwrap() {
                if !self.closed_windows.lock().unwrap().contains(&id) {
                    laufey::Window::from_id(id).focus();
                    return;
                }
            }

            let (endpoint, frontend) = match (renderer, deno) {
                (true, true) => ("/unified", "inspector.html"),
                (true, false) => ("/cef", "inspector.html"),
                (false, true) => ("/deno", "js_app.html"),
                (false, false) => unreachable!(),
            };
            let url = format!("http://{mux}/devtools/{frontend}?ws={mux}{endpoint}");
            eprintln!("[desktop] openDevtools(renderer={renderer}, deno={deno}) → {url}");
            let window = laufey::Window::new(1200, 800);
            window.set_title("Deno Desktop DevTools");
            window.navigate(&url);
            let window = self.setup_window_events(window, false);
            let id = window.id();
            // Track for HMR reload + the singleton check above.
            self.open_windows.lock().unwrap().insert(id);
            *self.devtools_window.lock().unwrap() = Some(id);
            return;
        }
        laufey::Window::from_id(window_id).open_devtools();
    }

    fn execute_js(
        &self,
        window_id: u32,
        script: &str,
        callback: Box<dyn FnOnce(Result<DesktopValue, DesktopValue>) + Send + 'static>,
    ) {
        laufey::Window::from_id(window_id).execute_js(
            script,
            Some(move |result: Result<laufey::Value, laufey::Value>| {
                // A value too deep to convert is reported to the caller as a
                // rejection rather than taken as a successful result.
                callback(match result {
                    Ok(val) => laufey_value_to_desktop_value(val).map_err(DesktopValue::String),
                    Err(err) => {
                        Err(laufey_value_to_desktop_value(err).unwrap_or_else(DesktopValue::String))
                    }
                });
            }),
        );
    }

    fn bind(&self, window_id: u32, name: &str) {
        let tx = self.event_tx.clone();
        let responses = self.pending_responses.clone();
        let name_owned = name.to_string();
        laufey::Window::from_id(window_id).add_binding_async(name, move |mut js_call| {
            let tx = tx.clone();
            let responses = responses.clone();
            let name = name_owned.clone();
            async move {
                // `mem::take` rather than `.iter().cloned()`: `js_call.resolve`
                // below consumes `js_call`, so the args can't simply be moved out
                // of the field, and cloning would deep-copy every argument —
                // binary payloads included — on the exact path this transport is
                // meant to make cheap for large buffers (#36498).
                let args = match std::mem::take(&mut js_call.args)
                    .into_iter()
                    .map(laufey_value_to_desktop_value)
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(args) => args,
                    Err(err) => {
                        // Too deeply nested to convert without risking the runtime
                        // thread's stack. Reject this call; the app keeps running.
                        js_call.reject(laufey::Value::String(err));
                        return;
                    }
                };
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                let call_id = register_bind_call(&responses, resp_tx);
                let event = DesktopEvent::BindCall {
                    // Attribute the call to the window the binding was registered on,
                    // not to `js_call.window_id`. The backend's per-call renderer id
                    // can drift from the id `bind()` recorded the callback under (seen
                    // on CEF/Windows when a larger module graph delays startup; see
                    // denoland/deno#35647), which would make the runtime-side lookup
                    // in `windowBindCallbacks` miss and reject an otherwise-registered
                    // call. The registration id always matches that map's key.
                    window_id,
                    name,
                    args,
                    call_id,
                };
                if let Err(err) = tx.try_send(event) {
                    let msg = match err {
                        tokio::sync::mpsc::error::TrySendError::Full(_) => {
                            "event channel saturated".to_string()
                        }
                        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                            "event channel closed".to_string()
                        }
                    };
                    js_call.reject(laufey::Value::String(msg));
                    return;
                }
                match resp_rx.await {
                    Ok(Ok(result)) => {
                        js_call.resolve(desktop_value_to_laufey_value(result));
                    }
                    Ok(Err(error)) => {
                        js_call.reject(laufey::Value::String(error));
                    }
                    Err(_) => {
                        js_call.reject(laufey::Value::String(
                            "bind response channel dropped".to_string(),
                        ));
                    }
                }
            }
        });
    }

    fn unbind(&self, window_id: u32, name: &str) {
        laufey::Window::from_id(window_id).unbind(name);
    }

    fn navigate(&self, window_id: u32, url: &str) {
        laufey::Window::from_id(window_id).navigate(url);
    }

    fn quit(&self) {
        laufey::quit();
    }

    fn set_application_menu(&self, window_id: u32, menu: Vec<MenuItem>) {
        let menu = menu
            .into_iter()
            .map(desktop_menu_item_to_laufey_menu_item)
            .collect::<Vec<_>>();
        let tx = self.event_tx.clone();
        laufey::Window::from_id(window_id).set_menu(&menu, move |id: &str| {
            let _ = tx.try_send(DesktopEvent::AppMenuClick {
                window_id,
                id: id.to_string(),
            });
        });
    }

    fn show_context_menu(&self, window_id: u32, x: i32, y: i32, menu: Vec<MenuItem>) {
        let menu = menu
            .into_iter()
            .map(desktop_menu_item_to_laufey_menu_item)
            .collect::<Vec<_>>();
        let tx = self.event_tx.clone();
        laufey::Window::from_id(window_id).show_context_menu(x, y, &menu, move |id: &str| {
            let _ = tx.try_send(DesktopEvent::ContextMenuClick {
                window_id,
                id: id.to_string(),
            });
        });
    }

    fn get_raw_window_handle(
        &self,
        window_id: u32,
    ) -> Result<
        (
            raw_window_handle::RawWindowHandle,
            raw_window_handle::RawDisplayHandle,
        ),
        deno_error::JsErrorBox,
    > {
        let window = laufey::Window::from_id(window_id);
        let handle_type = window.get_window_handle_type();
        let raw_win = window.get_window_handle();
        let raw_display = window.get_display_handle();

        let null_window =
            || deno_error::JsErrorBox::generic("Laufey returned a null window handle");
        let null_display =
            || deno_error::JsErrorBox::generic("Laufey returned a null display handle");

        match handle_type {
            laufey::LAUFEY_WINDOW_HANDLE_APPKIT => {
                use raw_window_handle::*;
                let win = RawWindowHandle::AppKit(AppKitWindowHandle::new(
                    std::ptr::NonNull::new(raw_win).ok_or_else(null_window)?,
                ));
                let display = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
                Ok((win, display))
            }
            laufey::LAUFEY_WINDOW_HANDLE_WIN32 => {
                use raw_window_handle::*;
                let mut handle = Win32WindowHandle::new(
                    std::num::NonZeroIsize::new(raw_win as isize).ok_or_else(null_window)?,
                );
                handle.hinstance = std::num::NonZeroIsize::new(raw_display as isize);
                let win = RawWindowHandle::Win32(handle);
                let display = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
                Ok((win, display))
            }
            laufey::LAUFEY_WINDOW_HANDLE_X11 => {
                use raw_window_handle::*;
                let win = RawWindowHandle::Xlib(XlibWindowHandle::new(raw_win as _));
                let display = RawDisplayHandle::Xlib(XlibDisplayHandle::new(
                    std::ptr::NonNull::new(raw_display),
                    0,
                ));
                Ok((win, display))
            }
            laufey::LAUFEY_WINDOW_HANDLE_WAYLAND => {
                use raw_window_handle::*;
                let win = RawWindowHandle::Wayland(WaylandWindowHandle::new(
                    std::ptr::NonNull::new(raw_win).ok_or_else(null_window)?,
                ));
                let display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                    std::ptr::NonNull::new(raw_display).ok_or_else(null_display)?,
                ));
                Ok((win, display))
            }
            other => Err(deno_error::JsErrorBox::generic(format!(
                "unknown Laufey window handle type: {other}",
            ))),
        }
    }

    fn alert(&self, title: &str, message: &str) {
        laufey::alert(title, message);
    }

    fn confirm(&self, title: &str, message: &str) -> bool {
        laufey::confirm(title, message)
    }

    fn prompt(&self, title: &str, message: &str, default_value: &str) -> Option<String> {
        laufey::prompt(title, message, default_value)
    }

    fn read_clipboard_text(&self) -> Option<String> {
        laufey::read_clipboard_text()
    }

    fn write_clipboard_text(&self, text: &str) {
        laufey::write_clipboard_text(text);
    }

    fn set_dock_badge(&self, text: &str) {
        laufey::set_dock_badge(if text.is_empty() { None } else { Some(text) });
    }

    fn bounce_dock(&self, critical: bool) {
        laufey::bounce_dock(if critical {
            laufey::DockBounceType::Critical
        } else {
            laufey::DockBounceType::Informational
        });
    }

    fn set_dock_menu(&self, menu: Option<Vec<MenuItem>>) {
        match menu {
            Some(menu) => {
                let menu = menu
                    .into_iter()
                    .map(desktop_menu_item_to_laufey_menu_item)
                    .collect::<Vec<_>>();
                let tx = self.event_tx.clone();
                laufey::set_dock_menu(&menu, move |id: &str| {
                    let _ = tx.try_send(DesktopEvent::DockMenuClick { id: id.to_string() });
                });
            }
            None => laufey::clear_dock_menu(),
        }
    }

    fn set_dock_visible(&self, visible: bool) {
        laufey::set_dock_visible(visible);
    }

    fn create_tray(&self) -> u32 {
        let tray = laufey::TrayIcon::new();
        let tray_id = tray.id();
        if tray_id == 0 {
            return 0;
        }
        let click_tx = self.event_tx.clone();
        let tray = tray.on_click(move || {
            let _ = click_tx.try_send(DesktopEvent::TrayClick { tray_id });
        });
        let dblclick_tx = self.event_tx.clone();
        tray.set_double_click_handler(move || {
            let _ = dblclick_tx.try_send(DesktopEvent::TrayDoubleClick { tray_id });
        });
        self.trays.lock().unwrap().insert(tray_id, tray);
        tray_id
    }

    fn destroy_tray(&self, tray_id: u32) {
        self.trays.lock().unwrap().remove(&tray_id);
    }

    fn set_tray_icon(&self, tray_id: u32, png_bytes: &[u8]) {
        if let Some(tray) = self.trays.lock().unwrap().get(&tray_id) {
            tray.set_icon(png_bytes);
        }
    }

    fn set_tray_icon_dark(&self, tray_id: u32, png_bytes: Option<&[u8]>) {
        if let Some(tray) = self.trays.lock().unwrap().get(&tray_id) {
            tray.set_icon_dark(png_bytes.unwrap_or(&[]));
        }
    }

    fn set_tray_tooltip(&self, tray_id: u32, text: Option<&str>) {
        if let Some(tray) = self.trays.lock().unwrap().get(&tray_id) {
            tray.set_tooltip(text);
        }
    }

    fn set_tray_menu(&self, tray_id: u32, menu: Option<Vec<MenuItem>>) {
        let trays = self.trays.lock().unwrap();
        let Some(tray) = trays.get(&tray_id) else {
            return;
        };
        match menu {
            Some(menu) => {
                let menu = menu
                    .into_iter()
                    .map(desktop_menu_item_to_laufey_menu_item)
                    .collect::<Vec<_>>();
                let tx = self.event_tx.clone();
                tray.set_menu(&menu, move |id: &str| {
                    let _ = tx.try_send(DesktopEvent::TrayMenuClick {
                        tray_id,
                        id: id.to_string(),
                    });
                });
            }
            None => tray.clear_menu(),
        }
    }

    fn get_tray_bounds(&self, tray_id: u32) -> Option<(i32, i32, i32, i32)> {
        let trays = self.trays.lock().unwrap();
        trays.get(&tray_id)?.get_bounds()
    }

    fn show_notification(
        &self,
        title: &str,
        body: Option<&str>,
        icon: Option<&[u8]>,
        tag: Option<&str>,
        silent: Option<bool>,
        require_interaction: Option<bool>,
    ) -> u32 {
        let mut builder = laufey::Notification::new(title);
        if let Some(body) = body {
            builder = builder.body(body);
        }
        if let Some(icon) = icon {
            builder = builder.icon(icon.to_vec());
        }
        if let Some(tag) = tag {
            builder = builder.tag(tag);
        }
        if let Some(silent) = silent {
            builder = builder.silent(silent);
        }
        if let Some(require) = require_interaction {
            builder = builder.require_interaction(require);
        }

        // The laufey handler closure receives only the event; it needs the
        // notification id to route the event through the desktop channel.
        // We can't know the id until `on_event` returns, so we capture it
        // through a shared slot populated immediately after.
        let id_slot: Arc<std::sync::OnceLock<u32>> = Arc::new(std::sync::OnceLock::new());
        let id_for_handler = id_slot.clone();
        let tx = self.event_tx.clone();
        let notifications = self.notifications.clone();

        let handle = builder.on_event(move |event| {
            let Some(&nid) = id_for_handler.get() else {
                return;
            };
            use laufey::NotificationEvent;
            let desktop_event = match event {
                NotificationEvent::Shown => DesktopEvent::NotificationShow {
                    notification_id: nid,
                },
                NotificationEvent::Clicked => DesktopEvent::NotificationClick {
                    notification_id: nid,
                },
                NotificationEvent::Closed => DesktopEvent::NotificationClose {
                    notification_id: nid,
                },
                // The Web Notification API has no "action" event in window context;
                // surface action button clicks as a click event for compatibility.
                NotificationEvent::Action(_) => DesktopEvent::NotificationClick {
                    notification_id: nid,
                },
            };
            let is_terminal = matches!(event, laufey::NotificationEvent::Closed);
            let _ = tx.try_send(desktop_event);
            if is_terminal {
                notifications.lock().unwrap().remove(&nid);
            }
        });

        let id = handle.id();
        if id == 0 {
            // Backend doesn't support notifications. Emit a synthetic error
            // event so the user can observe the failure.
            let _ = self
                .event_tx
                .try_send(DesktopEvent::NotificationError { notification_id: 0 });
            return 0;
        }
        let _ = id_slot.set(id);
        self.notifications.lock().unwrap().insert(id, handle);
        id
    }

    fn close_notification(&self, notification_id: u32) {
        if let Some(handle) = self.notifications.lock().unwrap().get(&notification_id) {
            handle.close();
        }
    }

    fn request_notification_permission(
        &self,
        cb: Box<dyn FnOnce(PermissionState) + Send + 'static>,
    ) {
        laufey::request_permission(laufey::PermissionKind::Notifications, move |status| {
            cb(map_permission_status(status))
        });
    }

    fn query_notification_permission(&self, cb: Box<dyn FnOnce(PermissionState) + Send + 'static>) {
        laufey::query_permission(laufey::PermissionKind::Notifications, move |status| {
            cb(map_permission_status(status))
        });
    }
}

fn map_permission_status(status: laufey::PermissionStatus) -> PermissionState {
    match status {
        laufey::PermissionStatus::Granted => PermissionState::Granted,
        laufey::PermissionStatus::Denied => PermissionState::Denied,
        laufey::PermissionStatus::Prompt => PermissionState::Prompt,
        laufey::PermissionStatus::Unsupported => PermissionState::Unsupported,
    }
}

fn desktop_menu_item_to_laufey_menu_item(item: MenuItem) -> laufey::MenuItem {
    match item {
        MenuItem::Item {
            label,
            id,
            accelerator,
            enabled,
            checked,
            icon,
            tooltip,
        } => laufey::MenuItem::Item {
            label,
            id,
            accelerator,
            enabled,
            checked,
            icon,
            tooltip,
        },
        MenuItem::Submenu { label, items } => laufey::MenuItem::Submenu {
            label,
            items: items
                .into_iter()
                .map(desktop_menu_item_to_laufey_menu_item)
                .collect(),
        },
        MenuItem::Separator => laufey::MenuItem::Separator,
        MenuItem::Role { role } => laufey::MenuItem::Role { role },
    }
}

/// Convert a laufey::Value to a DesktopValue for direct V8 conversion.
fn laufey_value_to_desktop_value(v: laufey::Value) -> Result<DesktopValue, String> {
    laufey_value_to_desktop_value_at(v, 0)
}

/// Depth-bounded body of [`laufey_value_to_desktop_value`].
///
/// The value comes from the renderer — binding arguments, or an `execute_js`
/// result — so its nesting is whatever the page sent. This conversion recurses
/// once per level, and `DesktopValue::to_v8` recurses over the result again,
/// so an unbounded value would walk the runtime thread off the end of its
/// stack. A `laufey::Value` can't be cyclic (the backend would have had to
/// resolve the cycle to build it), but depth alone is enough.
///
/// Bounded by the same `MAX_DEPTH` the `DesktopValue` deserializer uses, so
/// both directions agree on what is too deep.
fn laufey_value_to_desktop_value_at(
    v: laufey::Value,
    depth: usize,
) -> Result<DesktopValue, String> {
    Ok(match v {
        laufey::Value::Null => DesktopValue::Null,
        laufey::Value::Bool(b) => DesktopValue::Bool(b),
        laufey::Value::Int(i) => DesktopValue::Int(i),
        laufey::Value::Double(d) => DesktopValue::Double(d),
        laufey::Value::String(s) => DesktopValue::String(s),
        laufey::Value::List(l) => {
            let depth = nested_depth(depth)?;
            DesktopValue::List(
                l.into_iter()
                    .map(|v| laufey_value_to_desktop_value_at(v, depth))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
        laufey::Value::Dict(d) => {
            let depth = nested_depth(depth)?;
            DesktopValue::Dict(
                d.into_iter()
                    .map(|(k, v)| laufey_value_to_desktop_value_at(v, depth).map(|v| (k, v)))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
        laufey::Value::Binary(b) => DesktopValue::Binary(b),
    })
}

fn nested_depth(depth: usize) -> Result<usize, String> {
    match depth.checked_add(1).filter(|d| *d <= MAX_DEPTH) {
        Some(d) => Ok(d),
        None => Err(format!(
            "binding value nested deeper than {MAX_DEPTH} levels"
        )),
    }
}

/// Convert a DesktopValue back to a laufey::Value for delivery to the
/// renderer. The inverse of `laufey_value_to_desktop_value`; `Binary` maps to
/// `laufey::Value::Binary` so binding results carrying byte data arrive in
/// the webview as a `Uint8Array` (denoland/deno#36498).
fn desktop_value_to_laufey_value(v: DesktopValue) -> laufey::Value {
    match v {
        DesktopValue::Null => laufey::Value::Null,
        DesktopValue::Bool(b) => laufey::Value::Bool(b),
        DesktopValue::Int(i) => laufey::Value::Int(i),
        DesktopValue::Double(d) => laufey::Value::Double(d),
        DesktopValue::String(s) => laufey::Value::String(s),
        DesktopValue::List(l) => {
            laufey::Value::List(l.into_iter().map(desktop_value_to_laufey_value).collect())
        }
        DesktopValue::Dict(d) => laufey::Value::Dict(
            d.into_iter()
                .map(|(k, v)| (k, desktop_value_to_laufey_value(v)))
                .collect(),
        ),
        DesktopValue::Binary(b) => laufey::Value::Binary(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_status_maps_each_variant() {
        assert!(matches!(
            map_permission_status(laufey::PermissionStatus::Granted),
            PermissionState::Granted
        ));
        assert!(matches!(
            map_permission_status(laufey::PermissionStatus::Denied),
            PermissionState::Denied
        ));
        assert!(matches!(
            map_permission_status(laufey::PermissionStatus::Prompt),
            PermissionState::Prompt
        ));
        assert!(matches!(
            map_permission_status(laufey::PermissionStatus::Unsupported),
            PermissionState::Unsupported
        ));
    }

    #[test]
    fn menu_items_map_all_variants() {
        let mapped = desktop_menu_item_to_laufey_menu_item(MenuItem::Item {
            label: "Open".into(),
            id: Some("open".into()),
            accelerator: Some("CmdOrCtrl+O".into()),
            enabled: false,
            checked: true,
            icon: Some(vec![1, 2, 3]),
            tooltip: Some("hint".into()),
        });
        match mapped {
            laufey::MenuItem::Item {
                label,
                id,
                accelerator,
                enabled,
                checked,
                icon,
                tooltip,
            } => {
                assert_eq!(label, "Open");
                assert_eq!(id.as_deref(), Some("open"));
                assert_eq!(accelerator.as_deref(), Some("CmdOrCtrl+O"));
                assert!(!enabled);
                assert!(checked);
                assert_eq!(icon, Some(vec![1, 2, 3]));
                assert_eq!(tooltip.as_deref(), Some("hint"));
            }
            other => panic!("expected Item, got {other:?}"),
        }

        let submenu = desktop_menu_item_to_laufey_menu_item(MenuItem::Submenu {
            label: "File".into(),
            items: vec![
                MenuItem::Separator,
                MenuItem::Role {
                    role: "quit".into(),
                },
            ],
        });
        match submenu {
            laufey::MenuItem::Submenu { label, items } => {
                assert_eq!(label, "File");
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0], laufey::MenuItem::Separator));
                assert!(matches!(items[1], laufey::MenuItem::Role { .. }));
            }
            other => panic!("expected Submenu, got {other:?}"),
        }
    }

    /// Convert to `DesktopValue`, back to `laufey::Value`, then to
    /// `DesktopValue` again — the two `DesktopValue`s must agree. (`laufey::Value`
    /// has no `PartialEq`, so compare through the lossless second conversion.)
    fn round_trip(v: DesktopValue) -> DesktopValue {
        let laufey = desktop_value_to_laufey_value(v);
        laufey_value_to_desktop_value(laufey).expect("re-conversion")
    }

    #[test]
    fn value_conversion_round_trips_scalars_and_binary() {
        assert_eq!(round_trip(DesktopValue::Null), DesktopValue::Null);
        assert_eq!(
            round_trip(DesktopValue::Bool(true)),
            DesktopValue::Bool(true)
        );
        assert_eq!(round_trip(DesktopValue::Int(-7)), DesktopValue::Int(-7));
        assert_eq!(
            round_trip(DesktopValue::Double(1.5)),
            DesktopValue::Double(1.5)
        );
        assert_eq!(
            round_trip(DesktopValue::String("hi".into())),
            DesktopValue::String("hi".into())
        );
        assert_eq!(
            round_trip(DesktopValue::Binary(vec![0, 255, 1])),
            DesktopValue::Binary(vec![0, 255, 1])
        );
    }

    #[test]
    fn value_conversion_round_trips_nested() {
        let v = DesktopValue::List(vec![
            DesktopValue::Int(1),
            DesktopValue::Dict(vec![("k".into(), DesktopValue::Bool(false))]),
        ]);
        assert_eq!(round_trip(v.clone()), v);
    }

    #[test]
    fn nested_depth_enforces_max() {
        assert_eq!(nested_depth(0).unwrap(), 1);
        assert_eq!(nested_depth(MAX_DEPTH - 1).unwrap(), MAX_DEPTH);
        assert!(nested_depth(MAX_DEPTH).is_err());
    }

    #[test]
    fn value_nested_past_max_is_rejected() {
        let mut v = laufey::Value::Null;
        for _ in 0..(MAX_DEPTH + 2) {
            v = laufey::Value::List(vec![v]);
        }
        assert!(laufey_value_to_desktop_value(v).is_err());
    }
}
