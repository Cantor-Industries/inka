// Simplified HMR (hot module replacement) for the inka desktop runtime,
// adapted from Deno 2.9.7 `cli/rt/hmr.rs` (MIT, Copyright (c) the Deno
// authors). Watches source files on disk, transpiles changed
// TypeScript/TSX/JSX files with `deno_ast`, and hot-replaces them via V8's
// `Debugger.setScriptSource`, falling back to a window reload when a change
// can't be applied in place.
//
// inka's dev-run path (`inka desktop --hmr`) executes the source tree directly
// (the payload directory *is* the source directory), so the watch dir and the
// V8 script root coincide and `setScriptSource` maps changes 1:1.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::Arc;
use std::time::Duration;

use deno_core::parking_lot::Mutex;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::url::Url;
use deno_core::LocalInspectorSession;
use deno_error::JsErrorBox;
use notify::event::ModifyKind;
use notify::RecursiveMode;
use notify::Watcher;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Coalesce events that arrive within this window into one transpile +
/// `setScriptSource` round-trip. Editors that save-then-format (or do
/// atomic-rename saves) emit several events per keystroke.
const HMR_DEBOUNCE: Duration = Duration::from_millis(50);

static NEXT_MSG_ID: AtomicI32 = AtomicI32::new(0);
fn next_id() -> i32 {
    NEXT_MSG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// Minimal CDP types needed for HMR.
mod cdp {
    use deno_core::serde::Deserialize;
    use deno_core::serde_json::Value;

    #[derive(Debug, Deserialize)]
    pub struct Notification {
        pub method: String,
        pub params: Value,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ScriptParsed {
        pub script_id: String,
        pub url: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ExceptionThrown {
        pub exception_details: ExceptionDetails,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ExceptionDetails {
        pub text: String,
        pub exception: Option<RemoteObject>,
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct RemoteObject {
        pub description: Option<String>,
    }

    impl ExceptionDetails {
        pub fn get_message_and_description(&self) -> (String, String) {
            let description = self
                .exception
                .clone()
                .and_then(|ex| ex.description)
                .unwrap_or_else(|| "undefined".to_string());
            (self.text.to_string(), description)
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SetScriptSourceResponse {
        pub status: Status,
        pub exception_details: Option<ExceptionDetails>,
    }

    #[derive(Debug, Deserialize)]
    pub enum Status {
        Ok,
        CompileError,
        BlockedByActiveGenerator,
        BlockedByActiveFunction,
        BlockedByTopLevelEsModuleChange,
    }
}

fn explain(response: &cdp::SetScriptSourceResponse) -> String {
    match response.status {
        cdp::Status::Ok => "OK".to_string(),
        cdp::Status::CompileError => {
            if let Some(details) = &response.exception_details {
                let (message, description) = details.get_message_and_description();
                format!(
                    "compile error: {}{}",
                    message,
                    if description == "undefined" {
                        String::new()
                    } else {
                        format!(" - {description}")
                    }
                )
            } else {
                "compile error: No exception details available".to_string()
            }
        }
        cdp::Status::BlockedByActiveGenerator => "blocked by active generator".to_string(),
        cdp::Status::BlockedByActiveFunction => "blocked by active function".to_string(),
        cdp::Status::BlockedByTopLevelEsModuleChange => {
            "blocked by top-level ES module change".to_string()
        }
    }
}

fn should_retry(status: &cdp::Status) -> bool {
    matches!(
        status,
        cdp::Status::BlockedByActiveGenerator | cdp::Status::BlockedByActiveFunction
    )
}

/// Transpile a TypeScript/TSX/JSX source file to JavaScript for HMR.
fn transpile_for_hmr(specifier: &Url, source_code: String) -> Result<String, JsErrorBox> {
    use deno_ast::*;
    let media_type = deno_media_type::MediaType::from_specifier(specifier);
    match media_type {
        deno_media_type::MediaType::TypeScript
        | deno_media_type::MediaType::Mts
        | deno_media_type::MediaType::Cts
        | deno_media_type::MediaType::Jsx
        | deno_media_type::MediaType::Tsx => {
            let parsed = parse_module(ParseParams {
                specifier: specifier.clone(),
                text: source_code.into(),
                media_type,
                capture_tokens: false,
                scope_analysis: false,
                maybe_syntax: None,
            })
            .map_err(JsErrorBox::from_err)?;

            let transpiled = parsed
                .transpile(
                    &TranspileOptions::default(),
                    &TranspileModuleOptions::default(),
                    &EmitOptions {
                        source_map: SourceMapOption::None,
                        ..Default::default()
                    },
                )
                .map_err(JsErrorBox::from_err)?
                .into_source();
            Ok(transpiled.text)
        }
        // JS files don't need transpilation.
        _ => Ok(source_code),
    }
}

#[derive(Debug)]
enum InspectorMessageState {
    Ready(Value),
    WaitingFor(oneshot::Sender<Value>),
}

#[derive(Debug)]
struct HmrStateInner {
    script_ids: HashMap<String, String>,
    messages: HashMap<i32, InspectorMessageState>,
    exception_tx: mpsc::UnboundedSender<JsErrorBox>,
}

#[derive(Clone, Debug)]
pub struct HmrState(Arc<Mutex<HmrStateInner>>);

impl HmrState {
    fn new(exception_tx: mpsc::UnboundedSender<JsErrorBox>) -> Self {
        Self(Arc::new(Mutex::new(HmrStateInner {
            script_ids: HashMap::new(),
            messages: HashMap::new(),
            exception_tx,
        })))
    }

    pub fn callback(&self, msg: deno_core::InspectorMsg) {
        let deno_core::InspectorMsgKind::Message(msg_id) = msg.kind else {
            match serde_json::from_str::<cdp::Notification>(&msg.content) {
                Ok(notification) => self.handle_notification(notification),
                Err(e) => eprintln!("[inka-desktop] HMR: bad CDP notification: {e}"),
            }
            return;
        };

        let message: Value = match serde_json::from_str(&msg.content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[inka-desktop] HMR: bad CDP response {msg_id}: {e}");
                return;
            }
        };
        let mut state = self.0.lock();
        let Some(message_state) = state.messages.remove(&msg_id) else {
            state
                .messages
                .insert(msg_id, InspectorMessageState::Ready(message));
            return;
        };
        let InspectorMessageState::WaitingFor(sender) = message_state else {
            return;
        };
        let _ = sender.send(message);
    }

    fn handle_notification(&self, notification: cdp::Notification) {
        if notification.method == "Runtime.exceptionThrown" {
            let exception_thrown =
                match serde_json::from_value::<cdp::ExceptionThrown>(notification.params) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[inka-desktop] HMR: malformed Runtime.exceptionThrown: {e}");
                        return;
                    }
                };
            let (message, description) = exception_thrown
                .exception_details
                .get_message_and_description();
            let _ = self
                .0
                .lock()
                .exception_tx
                .send(JsErrorBox::generic(format!("{message} {description}")));
        } else if notification.method == "Debugger.scriptParsed" {
            let params = match serde_json::from_value::<cdp::ScriptParsed>(notification.params) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[inka-desktop] HMR: malformed Debugger.scriptParsed: {e}");
                    return;
                }
            };
            if params.url.starts_with("file://") {
                self.0
                    .lock()
                    .script_ids
                    .insert(params.url.clone(), params.script_id);
            }
        }
    }
}

/// Callback invoked after a change is handled (hot-replaced or a reload).
pub type HmrReloadCallback = Box<dyn Fn() + Send + Sync>;

/// Result of attempting to apply a single change.
enum ChangeOutcome {
    Replaced(String),
    NeedsReload(String),
    Skipped,
}

/// What happened to a watched source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileChange {
    Updated,
    Removed,
}

/// Desktop HMR runner: watches source files and hot-replaces changed modules.
pub struct DesktopHmrRunner {
    session: LocalInspectorSession,
    state: HmrState,
    changed_rx: mpsc::UnboundedReceiver<(PathBuf, FileChange)>,
    exception_rx: mpsc::UnboundedReceiver<JsErrorBox>,
    watch_dir: PathBuf,
    vfs_root: PathBuf,
    _watcher: notify::RecommendedWatcher,
    on_reload: Option<HmrReloadCallback>,
    desktop_event_tx: Option<deno_runtime::ops::desktop::DesktopEventTx>,
}

impl DesktopHmrRunner {
    pub fn new(
        session: LocalInspectorSession,
        state: HmrState,
        watch_dir: PathBuf,
        vfs_root: PathBuf,
        exception_rx: mpsc::UnboundedReceiver<JsErrorBox>,
    ) -> Result<Self, JsErrorBox> {
        let (changed_tx, changed_rx) = mpsc::unbounded_channel();

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(event) = res else {
                return;
            };
            let change = match event.kind {
                notify::EventKind::Create(_) => FileChange::Updated,
                notify::EventKind::Modify(ModifyKind::Metadata(_)) => return,
                notify::EventKind::Modify(_) => FileChange::Updated,
                notify::EventKind::Remove(_) => FileChange::Removed,
                _ => return,
            };
            for path in event.paths {
                let matches_ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| matches!(ext, "js" | "ts" | "jsx" | "tsx" | "mjs" | "cjs"));
                if matches_ext {
                    let _ = changed_tx.send((path, change));
                }
            }
        })
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;

        watcher
            .watch(&watch_dir, RecursiveMode::Recursive)
            .map_err(|e| JsErrorBox::generic(e.to_string()))?;

        let watch_dir_canonical = watch_dir
            .canonicalize()
            .unwrap_or_else(|_| watch_dir.clone());

        Ok(Self {
            session,
            state,
            changed_rx,
            exception_rx,
            watch_dir: watch_dir_canonical,
            vfs_root,
            _watcher: watcher,
            on_reload: None,
            desktop_event_tx: None,
        })
    }

    pub fn set_on_reload(&mut self, cb: HmrReloadCallback) {
        self.on_reload = Some(cb);
    }

    pub fn start(&mut self) {
        self.session
            .post_message::<()>(next_id(), "Debugger.enable", None);
        self.session
            .post_message::<()>(next_id(), "Runtime.enable", None);
    }

    pub async fn run(&mut self) -> Result<(), deno_core::error::CoreError> {
        loop {
            tokio::select! {
                biased;

                maybe_error = self.exception_rx.recv() => {
                    if let Some(err) = maybe_error {
                        eprintln!("[inka-desktop] HMR exception: {err}");
                        if let Some(tx) = &self.desktop_event_tx {
                            let _ = tx.try_send(
                                deno_runtime::ops::desktop::DesktopEvent::RuntimeError {
                                    message: err.to_string(),
                                    stack: None,
                                },
                            );
                        }
                    }
                }

                maybe_path = self.changed_rx.recv() => {
                    let Some(first) = maybe_path else {
                        break Ok(());
                    };
                    let mut pending: HashMap<PathBuf, FileChange> = HashMap::new();
                    pending.insert(first.0, first.1);
                    loop {
                        match tokio::time::timeout(HMR_DEBOUNCE, self.changed_rx.recv()).await {
                            Ok(Some((path, change))) => {
                                pending.insert(path, change);
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }

                    let mut needs_reload = false;
                    let mut handled: HashSet<String> = HashSet::new();
                    for (path, change) in pending {
                        match self.handle_change(&path, change).await {
                            ChangeOutcome::Replaced(url) => {
                                handled.insert(url);
                            }
                            ChangeOutcome::NeedsReload(reason) => {
                                eprintln!("[inka-desktop] HMR: {reason} - reloading");
                                needs_reload = true;
                            }
                            ChangeOutcome::Skipped => {}
                        }
                    }
                    for url in &handled {
                        self.dispatch_hmr_event(url);
                        eprintln!("[inka-desktop] HMR: replaced {url}");
                    }
                    if needs_reload || !handled.is_empty() {
                        if let Some(on_reload) = &self.on_reload {
                            on_reload();
                        }
                    }
                }
            }
        }
    }

    async fn handle_change(&mut self, path: &Path, change: FileChange) -> ChangeOutcome {
        let canonical = match (change, path.canonicalize()) {
            (FileChange::Updated, Ok(p)) => p,
            (FileChange::Updated, Err(_)) => return ChangeOutcome::Skipped,
            (FileChange::Removed, _) => path.to_path_buf(),
        };

        let Ok(relative) = canonical.strip_prefix(&self.watch_dir) else {
            return ChangeOutcome::Skipped;
        };
        let vfs_path = self.vfs_root.join(relative);
        let Ok(module_url) = Url::from_file_path(&vfs_path) else {
            return ChangeOutcome::Skipped;
        };

        let script_id = self
            .state
            .0
            .lock()
            .script_ids
            .get(module_url.as_str())
            .cloned();

        if change == FileChange::Removed {
            return if script_id.is_some() {
                ChangeOutcome::NeedsReload(format!("{module_url} removed"))
            } else {
                ChangeOutcome::Skipped
            };
        }

        let Some(script_id) = script_id else {
            return ChangeOutcome::Skipped;
        };

        let source_code = match tokio::fs::read_to_string(&canonical).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[inka-desktop] HMR: failed to read {}: {e}",
                    canonical.display()
                );
                return ChangeOutcome::Skipped;
            }
        };

        let source_code = match transpile_for_hmr(&module_url, source_code) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[inka-desktop] HMR: transpile error for {module_url}: {e}");
                return ChangeOutcome::Skipped;
            }
        };

        let mut tries = 1;
        loop {
            let msg_id = self.set_script_source(&script_id, &source_code);
            let value = match self.wait_for_response(msg_id).await {
                Some(v) => v,
                None => {
                    eprintln!("[inka-desktop] HMR: inspector dropped response for {module_url}");
                    return ChangeOutcome::Skipped;
                }
            };
            if let Some(err) = value.get("error") {
                eprintln!("[inka-desktop] HMR: setScriptSource error for {module_url}: {err}");
                return ChangeOutcome::Skipped;
            }
            let result_value = value.get("result").cloned().unwrap_or(Value::Null);
            let result: cdp::SetScriptSourceResponse = match serde_json::from_value(result_value) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!(
                        "[inka-desktop] HMR: bad CDP response for {module_url}: {e} ({value})"
                    );
                    return ChangeOutcome::Skipped;
                }
            };

            if matches!(result.status, cdp::Status::Ok) {
                return ChangeOutcome::Replaced(module_url.into());
            }

            eprintln!(
                "[inka-desktop] HMR: failed to reload {module_url}: {}",
                explain(&result)
            );

            if matches!(result.status, cdp::Status::BlockedByTopLevelEsModuleChange) {
                return ChangeOutcome::NeedsReload(format!(
                    "{module_url} requires a top-level module reload"
                ));
            }
            if should_retry(&result.status) && tries <= 2 {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            return ChangeOutcome::Skipped;
        }
    }

    async fn wait_for_response(&self, msg_id: i32) -> Option<Value> {
        if let Some(message_state) = self.state.0.lock().messages.remove(&msg_id) {
            let InspectorMessageState::Ready(value) = message_state else {
                unreachable!();
            };
            return Some(value);
        }

        let (tx, rx) = oneshot::channel();
        self.state
            .0
            .lock()
            .messages
            .insert(msg_id, InspectorMessageState::WaitingFor(tx));
        match rx.await {
            Ok(value) => Some(value),
            Err(_) => {
                self.state.0.lock().messages.remove(&msg_id);
                None
            }
        }
    }

    fn set_script_source(&mut self, script_id: &str, source: &str) -> i32 {
        let msg_id = next_id();
        self.session.post_message(
            msg_id,
            "Debugger.setScriptSource",
            Some(json!({
                "scriptId": script_id,
                "scriptSource": source,
                "allowTopFrameEditing": true,
            })),
        );
        msg_id
    }

    fn dispatch_hmr_event(&mut self, module_url: &str) {
        let detail = json!({ "path": module_url }).to_string();
        let expr = format!("dispatchEvent(new CustomEvent(\"hmr\", {{ detail: {detail} }}));");
        self.session.post_message(
            next_id(),
            "Runtime.evaluate",
            Some(json!({ "expression": expr })),
        );
    }
}

/// Set up HMR for the desktop runtime. Returns a runner that should be polled
/// concurrently with the event loop. `watch_dir` is the source directory on
/// disk; `vfs_root` is the path V8 scripts are registered under (the payload
/// root — the same directory for inka's dev-run). `reload_url`, when set,
/// re-navigates every open window after a reload.
pub fn setup_desktop_hmr(
    worker: &mut deno_runtime::worker::MainWorker,
    watch_dir: PathBuf,
    vfs_root: PathBuf,
    reload_url: Option<String>,
) -> Result<DesktopHmrRunner, JsErrorBox> {
    let (exception_tx, exception_rx) = mpsc::unbounded_channel();
    let state = HmrState::new(exception_tx);
    let state_clone = state.clone();
    let cb = Box::new(move |msg| state_clone.callback(msg));
    let session = worker.create_inspector_session(cb);

    let mut runner = DesktopHmrRunner::new(session, state, watch_dir, vfs_root, exception_rx)?;

    {
        let op_state = worker.js_runtime.op_state();
        let op_state = op_state.borrow();
        runner.desktop_event_tx = op_state
            .try_borrow::<deno_runtime::ops::desktop::DesktopEventSender>()
            .map(|s| s.0.clone());
        if let (Some(url), Some(windows)) = (
            reload_url,
            op_state.try_borrow::<crate::desktop::DesktopOpenWindows>(),
        ) {
            let windows = windows.0.clone();
            runner.set_on_reload(Box::new(move || {
                let ids: Vec<u32> = windows.lock().unwrap().iter().copied().collect();
                for id in ids {
                    laufey::Window::from_id(id).navigate(&url);
                }
            }));
        }
    }

    runner.start();
    eprintln!(
        "[inka-desktop] HMR watching {} (root {})",
        runner.watch_dir.display(),
        runner.vfs_root.display()
    );
    Ok(runner)
}
