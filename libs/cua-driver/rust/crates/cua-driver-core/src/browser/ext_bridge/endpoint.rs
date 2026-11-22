// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The extension endpoint: CDP in, spec frames out (spec §6, §7, §9, §10).
//!
//! A [`CdpConnection`] speaks CDP: JSON objects with `id`, `method`,
//! `params` and an optional `sessionId`. The extension speaks the frames of
//! [`super::frame`]: `req`/`res`/`evt` with `op`, `tab` and `params`. This
//! module owns one [`CdpConnection`] whose transport is a host session's
//! [`Wire`], and translates in both directions:
//!
//! - a session-scoped call becomes `req` with `op` = `cdp`, `tab` derived
//!   from the session id, and `params` = `{method, params}`;
//! - `Target.*` / `Browser.*` calls the extension has no CDP counterpart
//!   for are answered locally from `tab.*` / `window.bounds` ops or refused
//!   with the error the engine already recognises;
//! - `res` becomes the matching CDP reply, `cdp.event` becomes a CDP event
//!   on the tab's session, and `tab.detached` / `tab.closed` become
//!   `Target.detachedFromTarget`.
//!
//! [`super::bridge`] calls [`connect`] for every verified host session; the
//! engine reaches the connection through the bridge's endpoint URL.
//!
//! Invariants that hold here:
//! - Every CDP method the driver may send is already checked against the
//!   upstream existing-profile policy by [`CdpConnection`] itself; a
//!   connection built here reports [`CdpMethodPolicy::ExistingProfile`].
//! - No frame larger than [`MAX_TO_CHROME`] is written to the host.
//! - `DOM.setFileInputFiles` paths are resolved and checked against
//!   `upload_roots` before anything leaves the process (spec §10.1, second
//!   line of defence), and a `hello` with `file_access = false` is refused.
//! - The connection's receive stream ends when the host session ends, so
//!   in-flight calls fail at once instead of waiting out the CDP deadline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::frame::{parse_envelope, FrameType, MAX_TO_CHROME};
use crate::browser::cdp_ws::CdpConnection;
use crate::browser::wire::{Wire, WireReceiver, WireSender};

/// Prefix of a CDP target id generated from an extension tab id.
pub const TARGET_PREFIX: &str = "ext-tab-";
/// Prefix of the fixed CDP session id of an extension tab.
pub const SESSION_PREFIX: &str = "ext-session-";

/// How long a locally handled method waits for its `tab.*` or
/// `window.bounds` answer. Same order as the CDP call deadline; the
/// extension's own deadline for a `cdp` op is longer (spec §12).
pub const INTERNAL_TIMEOUT: Duration = Duration::from_secs(20);

/// The CDP target id the engine sees for one extension tab.
pub fn target_id(tab: u64) -> String {
    format!("{TARGET_PREFIX}{tab}")
}

/// The fixed CDP session id of one extension tab. Repeated attaches return
/// the same value, so the engine's one-tab-one-session assumption holds.
pub fn session_id(tab: u64) -> String {
    format!("{SESSION_PREFIX}{tab}")
}

/// The tab behind a target id, when the id has the endpoint's shape.
pub fn tab_of_target(id: &str) -> Option<u64> {
    id.strip_prefix(TARGET_PREFIX)?.parse().ok()
}

/// The tab behind a session id, when the id has the endpoint's shape.
pub fn tab_of_session(id: &str) -> Option<u64> {
    id.strip_prefix(SESSION_PREFIX)?.parse().ok()
}

/// What the endpoint needs from the bridge handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointOptions {
    /// `driver_hello.config.upload_roots` (spec §10.1).
    pub upload_roots: Vec<String>,
    /// `host_hello.file_access` (spec §10).
    pub file_access: bool,
}

/// One translated CDP connection plus the bridge-level requests that have
/// no CDP counterpart (`ping`, `tab.open`). Clones share the connection.
#[derive(Clone)]
pub struct Endpoint {
    conn: Arc<CdpConnection>,
    state: Arc<State>,
}

impl Endpoint {
    /// The translated CDP connection. It reports
    /// [`CdpMethodPolicy::ExistingProfile`] from the start.
    pub fn connection(&self) -> &Arc<CdpConnection> {
        &self.conn
    }

    /// Whether the host session behind this endpoint has ended.
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::SeqCst) || self.conn.is_closed()
    }

    /// The extension's `ping` result (spec §6), used for the bridge status.
    pub async fn ping(&self) -> Result<Value, String> {
        self.state
            .request("ping", None, None)
            .await
            .map_err(|failure| failure.to_string())
    }

    /// Opens one owned `about:blank` tab when the extension owns none
    /// (design question 2): the extension can drive only tabs it opened, and
    /// no upstream tool opens one. Returns the new tab, or `None` when an
    /// owned tab already existed. The tab is active so screenshots see it.
    pub async fn ensure_owned_tab(&self) -> Result<Option<u64>, String> {
        let tabs = self
            .state
            .tab_list()
            .await
            .map_err(|failure| failure.to_string())?;
        if !tabs.is_empty() {
            return Ok(None);
        }
        let opened = self
            .state
            .request(
                "tab.open",
                None,
                Some(json!({"url": "about:blank", "active": true})),
            )
            .await
            .map_err(|failure| failure.to_string())?;
        let tab = opened
            .get("tab")
            .and_then(Value::as_u64)
            .or_else(|| opened.as_u64())
            .ok_or_else(|| "tab.open returned no tab id".to_owned())?;
        Ok(Some(tab))
    }

    /// Extension requests still waiting for a `res`.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.state.internal_pending.lock().unwrap().len()
            + self.state.forwarded_pending.lock().unwrap().len()
    }
}

/// Wraps a host session's [`Wire`] into a CDP connection.
///
/// Must be called inside a tokio runtime: it spawns the translating task and
/// the connection's reader.
pub fn connect(host: Wire, options: EndpointOptions) -> Endpoint {
    let Wire { sender, receiver } = host;
    let (emit_tx, emit_rx) = mpsc::unbounded_channel::<String>();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let state = Arc::new(State {
        options,
        ext: sender,
        emit: StdMutex::new(Some(emit_tx)),
        internal_pending: StdMutex::new(HashMap::new()),
        forwarded_pending: StdMutex::new(HashMap::new()),
        next_req_id: AtomicU64::new(1),
        cdp: StdMutex::new(Weak::new()),
        closed: AtomicBool::new(false),
    });
    let cdp_wire = Wire {
        sender: Box::new(EndpointSender {
            state: state.clone(),
        }),
        receiver: text_stream(out_rx),
    };
    let conn = Arc::new(CdpConnection::from_wire(cdp_wire));
    *state.cdp.lock().unwrap() = Arc::downgrade(&conn);
    conn.restrict_to_existing_profile();
    tokio::spawn(pump(receiver, emit_rx, state.clone(), out_tx));
    Endpoint { conn, state }
}

fn text_stream(rx: mpsc::UnboundedReceiver<String>) -> WireReceiver {
    Box::pin(futures_util::stream::unfold(
        rx,
        |mut rx: mpsc::UnboundedReceiver<String>| async move {
            let frame = rx.recv().await?;
            Some((frame, rx))
        },
    ))
}

/// A CDP error that was produced locally or by the extension.
#[derive(Debug, Clone, PartialEq)]
struct Failure {
    /// Numeric CDP code when one exists; `None` renders as text only.
    code: Option<i64>,
    message: String,
}

impl Failure {
    /// An extension error code (`not_owned`, `path_denied`, …) carried as
    /// text, so the upstream caller sees the code it recognises (spec §12).
    fn extension(code: &str, message: &str) -> Self {
        Self {
            code: None,
            message: format!("{code}: {message}"),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: Some(-32601),
            message: format!("{method} is not available over the extension bridge"),
        }
    }

    fn route(message: impl Into<String>) -> Self {
        Self {
            code: Some(-32000),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One received `res`, parsed the way the endpoint consumes it.
fn parse_result(value: &Value) -> Result<Value, Failure> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
    let error = value.get("error").cloned().unwrap_or(Value::Null);
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    if code == "cdp_error" {
        // Chrome's own code and message survive unchanged so upstream's
        // code-based classification keeps working (spec §12).
        let cdp = error.get("cdp").cloned().unwrap_or(Value::Null);
        return Err(Failure {
            code: cdp.get("code").and_then(Value::as_i64),
            message: cdp
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Chrome returned an error")
                .to_owned(),
        });
    }
    Err(Failure::extension(
        code,
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message"),
    ))
}

/// A `tab.list` entry.
#[derive(Debug, Clone, PartialEq)]
struct TabEntry {
    tab: u64,
    window: i64,
    url: String,
    title: String,
    attached: bool,
}

impl TabEntry {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            tab: value.get("tab").and_then(Value::as_u64)?,
            window: value.get("window").and_then(Value::as_i64).unwrap_or(0),
            url: value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            title: value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            attached: value
                .get("attached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn target_info(&self) -> Value {
        json!({
            "targetId": target_id(self.tab),
            "type": "page",
            "title": self.title,
            "url": self.url,
            "attached": self.attached,
        })
    }
}

fn parse_tab_list(value: &Value) -> Result<Vec<TabEntry>, Failure> {
    let items = match value {
        Value::Array(items) => items.clone(),
        Value::Object(object) => object
            .get("tabs")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| Failure::route("tab.list returned no tab array"))?,
        _ => return Err(Failure::route("tab.list returned no tab array")),
    };
    items
        .iter()
        .map(|item| TabEntry::parse(item).ok_or_else(|| Failure::route("tab.list entry lacks tab")))
        .collect()
}

/// `window.bounds` result as the endpoint uses it.
fn bounds_of(value: &Value) -> Result<Value, Failure> {
    let number = |field: &str| value.get(field).and_then(Value::as_f64);
    let window = value
        .get("window")
        .and_then(Value::as_i64)
        .ok_or_else(|| Failure::route("window.bounds returned no window"))?;
    let (Some(left), Some(top), Some(width), Some(height)) = (
        number("left"),
        number("top"),
        number("width"),
        number("height"),
    ) else {
        return Err(Failure::route("window.bounds returned no numeric geometry"));
    };
    Ok(json!({
        "window": window,
        "left": left,
        "top": top,
        "width": width,
        "height": height,
        "windowState": value.get("state").and_then(Value::as_str).unwrap_or("normal"),
    }))
}

/// State shared by the sender side, the pump and (weakly) the connection.
struct State {
    options: EndpointOptions,
    ext: Box<dyn WireSender>,
    /// Replies produced by the sender side, handed to the pump so the
    /// receive stream ends when the pump does.
    emit: StdMutex<Option<mpsc::UnboundedSender<String>>>,
    /// Locally handled calls waiting for a `tab.*` / `window.bounds` answer.
    internal_pending: StdMutex<HashMap<u64, oneshot::Sender<Result<Value, Failure>>>>,
    /// Forwarded `cdp` calls: extension request id → CDP call id.
    forwarded_pending: StdMutex<HashMap<u64, u64>>,
    next_req_id: AtomicU64,
    /// The translated connection, registered for dialog-session state.
    cdp: StdMutex<Weak<CdpConnection>>,
    closed: AtomicBool,
}

impl State {
    fn next_req_id(&self) -> u64 {
        self.next_req_id.fetch_add(1, Ordering::Relaxed)
    }

    fn emit_frame(&self, value: Value) -> anyhow::Result<()> {
        let emit = self.emit.lock().unwrap();
        let Some(sender) = emit.as_ref() else {
            anyhow::bail!("extension bridge connection is closed");
        };
        sender
            .send(value.to_string())
            .map_err(|_| anyhow::anyhow!("extension bridge connection is closed"))
    }

    fn register_dialog_session(&self, tab: u64) {
        if let Some(conn) = self.cdp.lock().unwrap().upgrade() {
            conn.register_dialog_session(&session_id(tab), &target_id(tab));
        }
    }

    fn unregister_dialog_session(&self, tab: u64) {
        if let Some(conn) = self.cdp.lock().unwrap().upgrade() {
            conn.unregister_dialog_session(&session_id(tab), &target_id(tab));
        }
    }

    /// Sends one `req` and awaits its `res`.
    async fn request(
        &self,
        op: &str,
        tab: Option<u64>,
        params: Option<Value>,
    ) -> Result<Value, Failure> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Failure::route("extension bridge connection is closed"));
        }
        let id = self.next_req_id();
        let mut frame = json!({"v": 1, "t": "req", "id": id, "op": op});
        if let Some(tab) = tab {
            frame["tab"] = json!(tab);
        }
        if let Some(params) = params {
            frame["params"] = params;
        }
        let text = frame.to_string();
        if text.len() > MAX_TO_CHROME {
            return Err(Failure::extension(
                "frame_too_large",
                &format!("request of {} bytes exceeds {MAX_TO_CHROME}", text.len()),
            ));
        }
        let (tx, rx) = oneshot::channel();
        self.internal_pending.lock().unwrap().insert(id, tx);
        if let Err(error) = self.ext.send_text(text).await {
            self.internal_pending.lock().unwrap().remove(&id);
            return Err(Failure::route(format!("bridge send failed: {error}")));
        }
        match tokio::time::timeout(INTERNAL_TIMEOUT, rx).await {
            Err(_) => {
                self.internal_pending.lock().unwrap().remove(&id);
                Err(Failure::extension(
                    "timeout",
                    &format!("{op} got no answer within {INTERNAL_TIMEOUT:?}"),
                ))
            }
            Ok(Err(_)) => Err(Failure::route("extension bridge connection is closed")),
            Ok(Ok(result)) => result,
        }
    }

    async fn tab_list(&self) -> Result<Vec<TabEntry>, Failure> {
        self.request("tab.list", None, None)
            .await
            .and_then(|value| parse_tab_list(&value))
    }

    /// Fails every local caller at once; the receive stream ends when the
    /// pump returns.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.internal_pending.lock().unwrap().clear();
        self.forwarded_pending.lock().unwrap().clear();
        self.emit.lock().unwrap().take();
    }

    /// Reads one extension frame. Returns `false` when the connection ended.
    fn handle_frame(&self, text: &str, out: &mpsc::UnboundedSender<String>) -> bool {
        let envelope = match parse_envelope(text.as_bytes()) {
            Ok(envelope) => envelope,
            Err(_) => return true,
        };
        match envelope.t {
            FrameType::Res => {
                if let Some(id) = envelope.id {
                    self.handle_res(id, &envelope.value, out);
                }
                true
            }
            FrameType::Evt => {
                self.handle_evt(&envelope.value, out);
                true
            }
            FrameType::Bye => false,
            _ => true,
        }
    }

    fn handle_res(&self, id: u64, value: &Value, out: &mpsc::UnboundedSender<String>) {
        let result = parse_result(value);
        if let Some(tx) = self.internal_pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
            return;
        }
        let Some(cdp_id) = self.forwarded_pending.lock().unwrap().remove(&id) else {
            // A reply to a call that already timed out, or an id this
            // endpoint never issued: dropped, like upstream's reader does.
            return;
        };
        let reply = match result {
            Ok(result) => json!({"id": cdp_id, "result": result}),
            Err(failure) => error_reply(cdp_id, &failure),
        };
        let _ = out.send(reply.to_string());
    }

    fn handle_evt(&self, value: &Value, out: &mpsc::UnboundedSender<String>) {
        let event = value.get("ev").and_then(Value::as_str).unwrap_or("");
        let tab = value.get("tab").and_then(Value::as_u64);
        match event {
            "cdp.event" => {
                let (Some(tab), Some(method)) = (tab, value.get("method").and_then(Value::as_str))
                else {
                    return;
                };
                // Every tab has exactly one session, and that session is the
                // Page-enabled subscriber the dialog state hangs off.
                self.register_dialog_session(tab);
                let frame = json!({
                    "method": method,
                    "params": value.get("params").cloned().unwrap_or(Value::Null),
                    "sessionId": session_id(tab),
                });
                let _ = out.send(frame.to_string());
            }
            "tab.detached" | "tab.closed" => {
                let Some(tab) = tab else {
                    return;
                };
                self.unregister_dialog_session(tab);
                let reason = value
                    .get("params")
                    .and_then(|params| params.get("reason"))
                    .or_else(|| value.get("reason"))
                    .cloned();
                let mut params = json!({
                    "sessionId": session_id(tab),
                    "targetId": target_id(tab),
                });
                if let Some(reason) = reason {
                    params["reason"] = reason;
                }
                let frame = json!({"method": "Target.detachedFromTarget", "params": params});
                let _ = out.send(frame.to_string());
            }
            "host.error" => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let Some(id) = params.get("id").and_then(Value::as_u64) else {
                    return;
                };
                let code = params
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("host_error");
                let failure = Failure::extension(code, "the host refused the frame");
                if let Some(tx) = self.internal_pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(Err(failure));
                    return;
                }
                if let Some(cdp_id) = self.forwarded_pending.lock().unwrap().remove(&id) {
                    let _ = out.send(error_reply(cdp_id, &failure).to_string());
                }
            }
            _ => {}
        }
    }
}

fn error_reply(cdp_id: u64, failure: &Failure) -> Value {
    match failure.code {
        Some(code) => json!({
            "id": cdp_id,
            "error": {"code": code, "message": failure.message},
        }),
        None => json!({
            "id": cdp_id,
            "error": {"message": failure.message},
        }),
    }
}

/// The driver side of one host session: turns CDP commands into frames.
struct EndpointSender {
    state: Arc<State>,
}

impl WireSender for EndpointSender {
    fn send_text(&self, text: String) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { self.handle(&text).await })
    }
}

impl EndpointSender {
    async fn handle(&self, text: &str) -> anyhow::Result<()> {
        let state = &self.state;
        if state.closed.load(Ordering::SeqCst) {
            anyhow::bail!("extension bridge connection is closed");
        }
        let message: Value = serde_json::from_str(text)
            .map_err(|error| anyhow::anyhow!("CDP command is not JSON: {error}"))?;
        let cdp_id = message
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("CDP command lacks an id"))?;
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("CDP command lacks a method"))?
            .to_owned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = message.get("sessionId").and_then(Value::as_str);
        match method.as_str() {
            "Target.getTargets" => {
                let result = self.get_targets().await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            "Target.getTargetInfo" => {
                let result = self.get_target_info(&params).await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            "Browser.getWindowForTarget" => {
                let result = self.get_window_for_target(&params).await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            "Browser.getWindowBounds" => {
                let result = self.get_window_bounds(&params).await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            "Target.attachToTarget" => {
                let result = self.attach_to_target(&params).await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            "Target.detachFromTarget" => {
                // Local success: `tab.detach` would give up ownership, which
                // the engine never asked for.
                state.emit_frame(json!({"id": cdp_id, "result": {}}))
            }
            "Target.setAutoAttach" | "Browser.setDownloadBehavior" => {
                state.emit_frame(error_reply(cdp_id, &Failure::method_not_found(&method)))
            }
            "Target.activateTarget" => {
                let result = self.activate_target(&params).await;
                state.emit_frame(match result {
                    Ok(result) => json!({"id": cdp_id, "result": result}),
                    Err(failure) => error_reply(cdp_id, &failure),
                })
            }
            _ => match session {
                Some(session) => self.forward_cdp(cdp_id, session, &method, params).await,
                None => {
                    // Unknown browser-level methods are refused locally and
                    // never reach the extension (spec §18, fork risk note).
                    eprintln!(
                        "cua-driver ext-bridge: refusing browser-level method {method}; \
                         the extension endpoint has no mapping for it"
                    );
                    state.emit_frame(error_reply(cdp_id, &Failure::method_not_found(&method)))
                }
            },
        }
    }

    async fn get_targets(&self) -> Result<Value, Failure> {
        let tabs = self.state.tab_list().await?;
        Ok(json!({
            "targetInfos": tabs.iter().map(TabEntry::target_info).collect::<Vec<_>>(),
        }))
    }

    async fn get_target_info(&self, params: &Value) -> Result<Value, Failure> {
        let target = params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::route("Target.getTargetInfo needs targetId"))?;
        let tab = tab_of_target(target)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        let entry = self
            .state
            .tab_list()
            .await?
            .into_iter()
            .find(|entry| entry.tab == tab)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        Ok(json!({"targetInfo": entry.target_info()}))
    }

    async fn get_window_for_target(&self, params: &Value) -> Result<Value, Failure> {
        let target = params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::route("Browser.getWindowForTarget needs targetId"))?;
        let tab = tab_of_target(target)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        let bounds = self.window_bounds(tab).await?;
        Ok(json!({
            "windowId": bounds["window"],
            "bounds": without_window(&bounds),
        }))
    }

    async fn get_window_bounds(&self, params: &Value) -> Result<Value, Failure> {
        let window = params
            .get("windowId")
            .and_then(Value::as_i64)
            .ok_or_else(|| Failure::route("Browser.getWindowBounds needs windowId"))?;
        let tabs = self.state.tab_list().await?;
        let tab = tabs
            .iter()
            .find(|entry| entry.window == window && entry.attached)
            .or_else(|| tabs.iter().find(|entry| entry.window == window))
            .ok_or_else(|| Failure::route(format!("no window with id {window}")))?;
        let bounds = self.window_bounds(tab.tab).await?;
        Ok(json!({"bounds": without_window(&bounds)}))
    }

    async fn attach_to_target(&self, params: &Value) -> Result<Value, Failure> {
        let target = params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::route("Target.attachToTarget needs targetId"))?;
        let tab = tab_of_target(target)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        let entry = self
            .state
            .tab_list()
            .await?
            .into_iter()
            .find(|entry| entry.tab == tab)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        if !entry.attached {
            self.state.request("tab.attach", Some(tab), None).await?;
        }
        self.state.register_dialog_session(tab);
        Ok(json!({"sessionId": session_id(tab)}))
    }

    async fn activate_target(&self, params: &Value) -> Result<Value, Failure> {
        let target = params
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::route("Target.activateTarget needs targetId"))?;
        let tab = tab_of_target(target)
            .ok_or_else(|| Failure::route(format!("no target with id {target}")))?;
        self.state.request("tab.activate", Some(tab), None).await?;
        Ok(json!({}))
    }

    async fn window_bounds(&self, tab: u64) -> Result<Value, Failure> {
        let value = self.state.request("window.bounds", Some(tab), None).await?;
        bounds_of(&value)
    }

    /// Forwards one session-scoped CDP command as a `cdp` op; the reply
    /// arrives through the pump keyed by the extension request id.
    async fn forward_cdp(
        &self,
        cdp_id: u64,
        session: &str,
        method: &str,
        params: Value,
    ) -> anyhow::Result<()> {
        let state = &self.state;
        let tab = tab_of_session(session)
            .ok_or_else(|| anyhow::anyhow!("unknown CDP session {session}"))?;
        // Unknown sessions are reported to the caller, not as a send failure.
        if let Err(failure) = self.check_upload_paths(method, &params) {
            return state.emit_frame(error_reply(cdp_id, &failure));
        }
        let id = state.next_req_id();
        let frame = json!({
            "v": 1,
            "t": "req",
            "id": id,
            "op": "cdp",
            "tab": tab,
            "params": {"method": method, "params": params},
        });
        let text = frame.to_string();
        if text.len() > MAX_TO_CHROME {
            return state.emit_frame(error_reply(
                cdp_id,
                &Failure::extension(
                    "frame_too_large",
                    &format!(
                        "{method} serialises to {} bytes, over the {MAX_TO_CHROME}-byte limit",
                        text.len()
                    ),
                ),
            ));
        }
        state.forwarded_pending.lock().unwrap().insert(id, cdp_id);
        if let Err(error) = state.ext.send_text(text).await {
            state.forwarded_pending.lock().unwrap().remove(&id);
            return state.emit_frame(error_reply(
                cdp_id,
                &Failure::route(format!("bridge send failed: {error}")),
            ));
        }
        Ok(())
    }

    /// The second upload check (spec §10.1): `DOM.setFileInputFiles` only
    /// leaves the process for regular files under an `upload_roots` entry.
    fn check_upload_paths(&self, method: &str, params: &Value) -> Result<(), Failure> {
        if method != "DOM.setFileInputFiles" {
            return Ok(());
        }
        if !self.state.options.file_access {
            return Err(Failure::extension(
                "file_access_off",
                "the extension does not have file-scheme access",
            ));
        }
        let Some(files) = params.get("files").and_then(Value::as_array) else {
            return Ok(());
        };
        let roots: Vec<PathBuf> = self
            .state
            .options
            .upload_roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        for file in files {
            let Some(file) = file.as_str() else {
                return Err(Failure::extension(
                    "path_denied",
                    "a file entry is not a string",
                ));
            };
            let resolved = std::fs::canonicalize(file).map_err(|error| {
                Failure::extension(
                    "path_denied",
                    &format!("{file}: cannot resolve the path: {error}"),
                )
            })?;
            if !resolved.is_file() {
                return Err(Failure::extension(
                    "path_denied",
                    &format!("{} is not a regular file", resolved.display()),
                ));
            }
            if !inside_any(&resolved, &roots) {
                return Err(Failure::extension(
                    "path_denied",
                    &format!("{} is outside the upload roots", resolved.display()),
                ));
            }
        }
        Ok(())
    }
}

fn inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn without_window(bounds: &Value) -> Value {
    let mut bounds = bounds.clone();
    if let Some(object) = bounds.as_object_mut() {
        object.remove("window");
    }
    bounds
}

/// Reads extension frames and hands translated CDP text to the connection.
async fn pump(
    mut frames: WireReceiver,
    mut emit: mpsc::UnboundedReceiver<String>,
    state: Arc<State>,
    out: mpsc::UnboundedSender<String>,
) {
    let mut emit_open = true;
    loop {
        tokio::select! {
            frame = frames.next() => match frame {
                Some(text) => {
                    if !state.handle_frame(&text, &out) {
                        break;
                    }
                }
                None => break,
            },
            reply = emit.recv(), if emit_open => match reply {
                Some(text) => {
                    if out.send(text).is_err() {
                        break;
                    }
                }
                None => emit_open = false,
            },
        }
    }
    state.close();
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::browser::ext_bridge::frame::tests::FIXTURE;
    use crate::browser::wire::memory::{self, MemoryPeer};

    const TAB: u64 = 12;
    const SESSION: &str = "ext-session-12";
    const TARGET: &str = "ext-tab-12";

    fn fixture(name: &str) -> Value {
        let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
        fixture["valid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("fixture case {name}"))["json"]
            .clone()
    }

    struct FakeExtension {
        peer: MemoryPeer,
    }

    impl FakeExtension {
        fn start(options: EndpointOptions) -> (Endpoint, Self) {
            let (wire, peer) = memory::pair();
            (connect(wire, options), Self { peer })
        }

        async fn next(&mut self) -> Value {
            let text = tokio::time::timeout(Duration::from_secs(2), self.peer.sent.recv())
                .await
                .expect("the endpoint sent a frame")
                .expect("the endpoint is alive");
            serde_json::from_str(&text).unwrap()
        }

        async fn expect_none(&mut self) {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), self.peer.sent.recv())
                    .await
                    .is_err(),
                "the endpoint sent a frame it should not have"
            );
        }

        fn send(&self, value: Value) {
            self.peer.inject.send(value.to_string()).unwrap();
        }

        fn ok(&self, id: u64, result: Value) {
            self.send(json!({"v": 1, "t": "res", "id": id, "ok": true, "result": result}));
        }

        fn close(self) {
            drop(self);
        }
    }

    fn default_options() -> EndpointOptions {
        EndpointOptions {
            upload_roots: Vec::new(),
            file_access: true,
        }
    }

    fn tab_list_reply(attached: bool) -> Value {
        json!([{
            "tab": TAB,
            "window": 3,
            "url": "https://example.com/",
            "title": "Example",
            "status": "complete",
            "attached": attached,
        }])
    }

    #[tokio::test]
    async fn navigate_sends_the_sample_frame_except_id() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let pending = tokio::spawn(async move {
            conn.call(
                Some(SESSION),
                "Page.navigate",
                json!({"url": "https://example.com/"}),
            )
            .await
        });
        let frame = ext.next().await;
        let mut want = fixture("req_cdp");
        want["id"] = frame["id"].clone();
        assert_eq!(frame, want);
        ext.ok(
            frame["id"].as_u64().unwrap(),
            json!({"frameId": "F1", "loaderId": "L1"}),
        );
        let result = pending.await.unwrap().unwrap();
        assert_eq!(result["frameId"], "F1");
        assert_eq!(endpoint.pending_len(), 0);
    }

    #[tokio::test]
    async fn results_and_errors_map_onto_cdp() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();

        let call = conn.clone();
        let ok = tokio::spawn(async move {
            call.call(Some(SESSION), "Runtime.evaluate", json!({}))
                .await
        });
        let frame = ext.next().await;
        let mut res_ok = fixture("res_ok");
        res_ok["id"] = frame["id"].clone();
        ext.send(res_ok);
        let result = ok.await.unwrap().unwrap();
        assert_eq!(result["frameId"], "F1");
        assert_eq!(result["loaderId"], "L1");

        let call = conn.clone();
        let denied = tokio::spawn(async move {
            call.call(Some(SESSION), "Runtime.evaluate", json!({}))
                .await
        });
        let frame = ext.next().await;
        let mut not_owned = fixture("res_not_owned");
        not_owned["id"] = frame["id"].clone();
        ext.send(not_owned);
        let error = denied.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("not_owned"), "{error}");

        let call = conn.clone();
        let failed =
            tokio::spawn(async move { call.call(Some(SESSION), "Page.navigate", json!({})).await });
        let frame = ext.next().await;
        let mut cdp_error = fixture("res_cdp_error");
        cdp_error["id"] = frame["id"].clone();
        ext.send(cdp_error);
        let error = failed.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("(-32000)"), "{error}");
        assert!(error.contains("Cannot navigate to invalid URL"), "{error}");
    }

    #[tokio::test]
    async fn dialog_event_carries_the_tab_session_and_sets_state() {
        let (endpoint, ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let mut events = conn.subscribe();
        ext.send(fixture("evt_dialog"));
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.method, "Page.javascriptDialogOpening");
        assert_eq!(event.session_id.as_deref(), Some(SESSION));
        let dialog = conn.dialog_state(TARGET).expect("dialog state");
        assert_eq!(dialog.kind, "confirm");
        assert_eq!(dialog.session_id, SESSION);
    }

    #[tokio::test]
    async fn closed_tab_reports_detached_from_target() {
        let (endpoint, ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let mut events = conn.subscribe();
        ext.send(json!({"v": 1, "t": "evt", "ev": "tab.closed", "tab": TAB}));
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.method, "Target.detachedFromTarget");
        assert_eq!(event.params["sessionId"], SESSION);
    }

    #[tokio::test]
    async fn target_listing_and_info_use_tab_ops() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();

        let call = conn.clone();
        let targets =
            tokio::spawn(async move { call.call(None, "Target.getTargets", json!({})).await });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.list");
        assert!(frame.get("tab").is_none());
        ext.ok(frame["id"].as_u64().unwrap(), tab_list_reply(true));
        let result = targets.await.unwrap().unwrap();
        assert_eq!(result["targetInfos"][0]["targetId"], TARGET);
        assert_eq!(result["targetInfos"][0]["type"], "page");
        assert_eq!(result["targetInfos"][0]["title"], "Example");
        assert_eq!(result["targetInfos"][0]["attached"], true);

        let call = conn.clone();
        let info = tokio::spawn(async move {
            call.call(
                None,
                "Target.getTargetInfo",
                json!({"targetId": "ext-tab-12"}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.list");
        ext.ok(frame["id"].as_u64().unwrap(), tab_list_reply(true));
        let result = info.await.unwrap().unwrap();
        assert_eq!(result["targetInfo"]["targetId"], TARGET);
    }

    #[tokio::test]
    async fn window_bounds_answers_both_browser_methods() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let bounds = json!({
            "window": 3, "left": 0, "top": 27, "width": 1279, "height": 799,
            "state": "normal",
        });

        let call = conn.clone();
        let for_target = tokio::spawn(async move {
            call.call(
                None,
                "Browser.getWindowForTarget",
                json!({"targetId": "ext-tab-12"}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "window.bounds");
        assert_eq!(frame["tab"], TAB);
        ext.ok(frame["id"].as_u64().unwrap(), bounds.clone());
        let result = for_target.await.unwrap().unwrap();
        assert_eq!(result["windowId"], 3);
        assert_eq!(result["bounds"]["width"], 1279.0);
        assert_eq!(result["bounds"]["windowState"], "normal");
        assert!(result["bounds"].get("window").is_none());

        let call = conn.clone();
        let for_window = tokio::spawn(async move {
            call.call(None, "Browser.getWindowBounds", json!({"windowId": 3}))
                .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.list");
        ext.ok(frame["id"].as_u64().unwrap(), tab_list_reply(true));
        let frame = ext.next().await;
        assert_eq!(frame["op"], "window.bounds");
        ext.ok(frame["id"].as_u64().unwrap(), bounds);
        let result = for_window.await.unwrap().unwrap();
        assert_eq!(result["bounds"]["height"], 799.0);
    }

    #[tokio::test]
    async fn attach_returns_a_fixed_session_and_attaches_when_needed() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();

        let call = conn.clone();
        let attached = tokio::spawn(async move {
            call.call(
                None,
                "Target.attachToTarget",
                json!({"targetId": "ext-tab-12", "flatten": true}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.list");
        ext.ok(frame["id"].as_u64().unwrap(), tab_list_reply(true));
        let result = attached.await.unwrap().unwrap();
        assert_eq!(result["sessionId"], SESSION);
        ext.expect_none().await;

        let call = conn.clone();
        let needs_attach = tokio::spawn(async move {
            call.call(
                None,
                "Target.attachToTarget",
                json!({"targetId": "ext-tab-12", "flatten": true}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.list");
        ext.ok(frame["id"].as_u64().unwrap(), tab_list_reply(false));
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.attach");
        assert_eq!(frame["tab"], TAB);
        ext.ok(frame["id"].as_u64().unwrap(), Value::Null);
        let result = needs_attach.await.unwrap().unwrap();
        assert_eq!(result["sessionId"], SESSION);
    }

    #[tokio::test]
    async fn detach_is_local_and_set_auto_attach_is_refused() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();

        let result = conn
            .call(
                None,
                "Target.detachFromTarget",
                json!({"sessionId": SESSION}),
            )
            .await
            .unwrap();
        assert_eq!(result, json!({}));
        ext.expect_none().await;

        let error = conn
            .call(None, "Target.setAutoAttach", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("(-32601)"), "{error}");
        ext.expect_none().await;

        let error = conn
            .call(
                None,
                "Browser.setDownloadBehavior",
                json!({"behavior": "default"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("(-32601)"), "{error}");
        ext.expect_none().await;
    }

    #[tokio::test]
    async fn activate_target_uses_tab_activate() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let call = conn.clone();
        let pending = tokio::spawn(async move {
            call.call(
                None,
                "Target.activateTarget",
                json!({"targetId": "ext-tab-12"}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "tab.activate");
        assert_eq!(frame["tab"], TAB);
        ext.ok(frame["id"].as_u64().unwrap(), Value::Null);
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unknown_browser_method_and_policy_refusals_send_no_frame() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        assert_eq!(
            conn.method_policy(),
            crate::browser::cdp_ws::CdpMethodPolicy::ExistingProfile
        );

        let error = conn
            .call(None, "Browser.getVersion", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("existing-profile CDP policy refused"),
            "{error}"
        );
        ext.expect_none().await;

        for method in ["Network.getAllCookies", "Runtime.getProperties"] {
            let error = conn
                .call(None, method, json!({}))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("existing-profile CDP policy refused"),
                "{method}: {error}"
            );
        }
        ext.expect_none().await;
    }

    fn upload_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("upload.txt");
        std::fs::write(&file, b"hello").unwrap();
        (dir, file)
    }

    #[allow(dead_code)]
    fn upload_options(root: &std::path::Path) -> EndpointOptions {
        EndpointOptions {
            upload_roots: vec![root.to_string_lossy().into_owned()],
            file_access: true,
        }
    }

    #[tokio::test]
    async fn uploads_inside_the_root_are_forwarded() {
        let (dir, file) = upload_fixture();
        let (endpoint, mut ext) = FakeExtension::start(upload_options(dir.path()));
        let conn = endpoint.connection().clone();
        let call = conn.clone();
        let path = file.to_string_lossy().into_owned();
        let pending = tokio::spawn(async move {
            call.call(
                Some(SESSION),
                "DOM.setFileInputFiles",
                json!({"files": [path], "nodeId": 1}),
            )
            .await
        });
        let frame = ext.next().await;
        assert_eq!(frame["op"], "cdp");
        assert_eq!(frame["params"]["method"], "DOM.setFileInputFiles");
        assert_eq!(
            frame["params"]["params"]["files"][0],
            file.to_string_lossy().as_ref()
        );
        ext.ok(frame["id"].as_u64().unwrap(), Value::Null);
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn uploads_outside_the_root_and_symlinks_are_denied() {
        let (dir, _file) = upload_fixture();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, b"nope").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&outside_file, &link).unwrap();

        let (endpoint, mut ext) = FakeExtension::start(upload_options(dir.path()));
        let conn = endpoint.connection().clone();

        for path in [outside_file, link] {
            let error = conn
                .call(
                    Some(SESSION),
                    "DOM.setFileInputFiles",
                    json!({"files": [path.to_string_lossy()], "nodeId": 1}),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("path_denied"), "{error}");
            ext.expect_none().await;
        }
    }

    #[tokio::test]
    async fn uploads_are_refused_when_file_access_is_off() {
        let (dir, file) = upload_fixture();
        let options = EndpointOptions {
            upload_roots: vec![dir.path().to_string_lossy().into_owned()],
            file_access: false,
        };
        let (endpoint, mut ext) = FakeExtension::start(options);
        let conn = endpoint.connection().clone();
        let error = conn
            .call(
                Some(SESSION),
                "DOM.setFileInputFiles",
                json!({"files": [file.to_string_lossy()], "nodeId": 1}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("file_access_off"), "{error}");
        ext.expect_none().await;
    }

    #[tokio::test]
    async fn oversized_commands_are_refused_locally() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let error = conn
            .call(
                Some(SESSION),
                "Runtime.evaluate",
                json!({"expression": "a".repeat(MAX_TO_CHROME)}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("frame_too_large"), "{error}");
        ext.expect_none().await;
    }

    #[tokio::test]
    async fn a_hung_input_call_does_not_block_the_dialog_call() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();

        let mouse = conn.clone();
        let mouse = tokio::spawn(async move {
            mouse
                .call(
                    Some(SESSION),
                    "Input.dispatchMouseEvent",
                    json!({"type": "mousePressed"}),
                )
                .await
        });
        let mouse_frame = ext.next().await;

        let dialog = conn.clone();
        let dialog = tokio::spawn(async move {
            dialog
                .call(
                    Some(SESSION),
                    "Page.handleJavaScriptDialog",
                    json!({"accept": true}),
                )
                .await
        });
        let dialog_frame = ext.next().await;

        ext.ok(dialog_frame["id"].as_u64().unwrap(), Value::Null);
        dialog.await.unwrap().unwrap();
        assert!(!mouse.is_finished());

        ext.ok(mouse_frame["id"].as_u64().unwrap(), Value::Null);
        mouse.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_late_reply_is_dropped_and_its_entry_reclaimed() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let error = conn
            .call_with_timeout(
                Some(SESSION),
                "Runtime.evaluate",
                json!({}),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        let frame = ext.next().await;
        assert_eq!(endpoint.pending_len(), 1);
        ext.ok(frame["id"].as_u64().unwrap(), json!({"result": 1}));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(endpoint.pending_len(), 0);

        // The connection still works afterwards.
        let call = conn.clone();
        let pending = tokio::spawn(async move {
            call.call(Some(SESSION), "Runtime.evaluate", json!({}))
                .await
        });
        let frame = ext.next().await;
        ext.ok(frame["id"].as_u64().unwrap(), json!({"result": 2}));
        assert_eq!(pending.await.unwrap().unwrap()["result"], 2);
    }

    #[tokio::test]
    async fn a_host_disconnect_fails_in_flight_calls_at_once() {
        let (endpoint, mut ext) = FakeExtension::start(default_options());
        let conn = endpoint.connection().clone();
        let call = conn.clone();
        let pending = tokio::spawn(async move {
            call.call(Some(SESSION), "Runtime.evaluate", json!({}))
                .await
        });
        let _frame = ext.next().await;
        ext.close();
        let error = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("the call failed at once")
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains("closed"), "{error}");
        assert_eq!(endpoint.pending_len(), 0);
    }
}
