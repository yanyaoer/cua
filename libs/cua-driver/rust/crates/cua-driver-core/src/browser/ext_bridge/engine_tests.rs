// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Engine-level acceptance for the bridge (design note §5 "s2b-4", items
//! 2–10): a fake extension speaks the bridge frames, the platform hands every
//! DevTools path to the bridge hooks first, and the real engine tools run the
//! whole chain from `browser_prepare` to a dialog.
//!
//! The fake extension answers `cdp` ops through the same scripted handler the
//! v2 tests drive over a WebSocket ([`fixture_handler`]), so these tests
//! exercise the real translation layer and the real engine, not a stub of
//! either. Nothing here starts a listener or touches a socket.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::bridge::{Bridge, BridgeOptions, HostInfo};
use crate::browser::engine::BrowserEngine;
use crate::browser::mock_cdp::{MockCall, MockEvent, MockHandler, MockReply};
use crate::browser::platform::{
    BrowserConsentOutcome, BrowserConsentRequest, BrowserPlatform, ExistingProfileSetupOutcome,
    ExistingProfileSetupRequest, IsolatedBrowserProcess, PrepareAction, PrepareOutcome,
    PrepareRequest,
};
use crate::browser::refusal::{BrowserRefusal, BrowserRefusalCode};
use crate::browser::tools::{
    BrowserClickTool, BrowserDialogTool, BrowserNavigateTool, BrowserPrepareTool,
    BrowserSetInputFilesTool, BrowserTypeTool, GetBrowserStateTool,
};
use crate::browser::types::{
    BrowserClassification, BrowserEngineFamily, BrowserProcessRole, BrowserProduct,
    NativeOwnershipMethod, NativeOwnershipProof, NativeWindowInfo, ProcessFingerprint, Rect,
};
use crate::browser::v2_tests::{fixture_handler, FixtureState, SharedState};
use crate::browser::wire::memory::{self, MemoryPeer};
use crate::browser::wire::Wire;
use crate::tool::Tool;

const PID: i64 = 1;
const WINDOW: u64 = 7;
const SESSION: &str = "run-bridge";
const TRANSPORT: &str = "transport-bridge";
/// The extension tab the fake host owns before the driver connects.
const TAB: u64 = 5;
/// The CDP window id the extension reports for that tab (the fixture's id).
const CDP_WINDOW: i64 = 11;

// ── The fake extension ──────────────────────────────────────────────────────

/// What the fake extension saw, for assertions.
#[derive(Default)]
struct Received {
    frames: Vec<Value>,
    cdp: Vec<(String, Value)>,
}

/// One tab in the fake extension's model.
fn tab_entry(tab: u64, attached: bool) -> Value {
    json!({
        "tab": tab,
        "window": CDP_WINDOW,
        "url": "https://fixture.test/",
        "title": "Fixture",
        "status": "complete",
        "attached": attached,
    })
}

#[derive(Default)]
struct Model {
    tabs: Vec<Value>,
    next: u64,
    /// Tabs the extension no longer lets the driver attach to
    /// (`tab.detached`): attaching answers the spec's `not_owned`.
    revoked: Vec<u64>,
}

/// A scripted reaction for one forwarded CDP call, tried before the fixture.
type Override = Arc<dyn Fn(&MockCall) -> Option<MockReply> + Send + Sync>;

struct Extension {
    received: Arc<StdMutex<Received>>,
    tabs: Arc<StdMutex<Model>>,
    /// `None` after [`Self::shutdown`]: dropping the last sender ends the
    /// host session the way the listener's supersede does.
    inject: Option<mpsc::UnboundedSender<String>>,
    task: tokio::task::JoinHandle<()>,
}

impl Extension {
    /// Starts a fake extension on a fresh memory wire. `overrides` answers
    /// calls the fixture cannot (a file input, a dialog opener); every other
    /// call goes to `handler`.
    fn start(handler: MockHandler, overrides: Override) -> (Wire, Extension) {
        let (wire, mut peer): (Wire, MemoryPeer) = memory::pair();
        let received = Arc::new(StdMutex::new(Received::default()));
        let tabs = Arc::new(StdMutex::new(Model {
            tabs: vec![tab_entry(TAB, true)],
            next: TAB,
            revoked: Vec::new(),
        }));
        let seen = received.clone();
        let model = tabs.clone();
        let inject = peer.inject.clone();
        let sender = inject.clone();
        let task = tokio::spawn(async move {
            while let Some(text) = peer.sent.recv().await {
                let Ok(request) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                seen.lock().unwrap().frames.push(request.clone());
                let Some(response) = answer(&request, &model, &seen, &inject, &handler, &overrides)
                else {
                    // A hung call: no response, like a renderer blocked by a
                    // dialog it has already announced.
                    continue;
                };
                let mut response = response;
                response["v"] = json!(1);
                response["t"] = json!("res");
                response["id"] = request["id"].clone();
                if peer.inject.send(response.to_string()).is_err() {
                    break;
                }
            }
        });
        (
            wire,
            Extension {
                received,
                tabs,
                inject: Some(sender),
                task,
            },
        )
    }

    /// Pushes one extension event the driver has to react to.
    fn send_event(&self, event: &str, tab: u64) {
        self.inject
            .as_ref()
            .expect("the extension is alive")
            .send(json!({"v": 1, "t": "evt", "ev": event, "tab": tab}).to_string())
            .expect("the endpoint is alive");
    }

    /// Marks a tab as no longer attachable, as `tab.detached` does.
    fn revoke(&self, tab: u64) {
        let mut model = self.tabs.lock().unwrap();
        model.revoked.push(tab);
        for entry in &mut model.tabs {
            if entry["tab"] == json!(tab) {
                entry["attached"] = json!(false);
            }
        }
    }

    fn frames(&self) -> Vec<Value> {
        self.received.lock().unwrap().frames.clone()
    }

    fn cdp_methods(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap()
            .cdp
            .iter()
            .map(|(method, _)| method.clone())
            .collect()
    }

    fn count_op(&self, op: &str) -> usize {
        self.frames()
            .iter()
            .filter(|frame| frame["op"] == op)
            .count()
    }

    /// Ends the session the way the listener does when a new host takes over:
    /// stops answering and drops the last frame sender, so the endpoint's
    /// receive stream ends.
    fn shutdown(&mut self) {
        self.task.abort();
        self.inject = None;
    }
}

fn answer(
    request: &Value,
    model: &Arc<StdMutex<Model>>,
    seen: &Arc<StdMutex<Received>>,
    inject: &mpsc::UnboundedSender<String>,
    handler: &MockHandler,
    overrides: &Override,
) -> Option<Value> {
    let ok = |result: Value| Some(json!({"ok": true, "result": result}));
    match request["op"].as_str().unwrap_or("") {
        "tab.list" => ok(Value::Array(model.lock().unwrap().tabs.clone())),
        "tab.attach" => {
            let tab = request["tab"].as_u64().unwrap_or(0);
            let mut model = model.lock().unwrap();
            if model.revoked.contains(&tab) {
                return Some(json!({
                    "ok": false,
                    "error": {
                        "code": "not_owned",
                        "message": format!("the extension no longer owns tab {tab}"),
                    },
                }));
            }
            for entry in &mut model.tabs {
                if entry["tab"] == json!(tab) {
                    entry["attached"] = json!(true);
                }
            }
            ok(json!({}))
        }
        "tab.activate" => ok(json!({})),
        "tab.open" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            assert_eq!(params["url"], "about:blank");
            assert_eq!(params["active"], true);
            let mut model = model.lock().unwrap();
            model.next += 1;
            let tab = model.next;
            model.tabs.push(tab_entry(tab, false));
            ok(json!({"tab": tab}))
        }
        "window.bounds" => ok(json!({
            "window": CDP_WINDOW,
            "left": 0.0,
            "top": 0.0,
            "width": 800.0,
            "height": 600.0,
            "state": "normal",
        })),
        "ping" => ok(json!({
            "installed": true,
            "version": "1.4.0",
            "attached": true,
            "owned": model.lock().unwrap().tabs.len(),
        })),
        "cdp" => {
            let method = request["params"]["method"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let params = request["params"]["params"].clone();
            let tab = request["tab"].as_u64().unwrap_or(0);
            seen.lock()
                .unwrap()
                .cdp
                .push((method.clone(), params.clone()));
            // The fixture branches on its own session names; a bridge tab
            // session plays the role of one of its tab sessions.
            let call = MockCall {
                method,
                params,
                session_id: Some(format!("tab-sess-{tab}")),
            };
            let reply = match overrides(&call) {
                Some(reply) => reply,
                None => handler(&call),
            };
            for event in &reply.events {
                let frame = json!({
                    "v": 1,
                    "t": "evt",
                    "ev": "cdp.event",
                    "tab": tab,
                    "method": event.method,
                    "params": event.params,
                });
                let _ = inject.send(frame.to_string());
            }
            if reply.suppress_response {
                return None;
            }
            Some(match reply.result {
                Ok(result) => json!({"ok": true, "result": result}),
                Err((code, message)) => json!({
                    "ok": false,
                    "error": {
                        "code": "cdp_error",
                        "message": message,
                        "cdp": {"code": code, "message": message},
                    },
                }),
            })
        }
        _ => ok(json!({})),
    }
}

// ── The platform: every DevTools path starts at the bridge hook ─────────────

/// Counts the upstream DevTools paths a call would have taken without a
/// bridge. Acceptance item 7 asserts these stay at zero.
#[derive(Default)]
struct Upstream {
    port_scans: AtomicUsize,
    active_port_reads: AtomicUsize,
    settings_ui: AtomicUsize,
    isolated_launches: AtomicUsize,
}

impl Upstream {
    fn all(&self) -> [(&'static str, usize); 4] {
        [
            ("port_scans", self.port_scans.load(Ordering::SeqCst)),
            (
                "active_port_reads",
                self.active_port_reads.load(Ordering::SeqCst),
            ),
            ("settings_ui", self.settings_ui.load(Ordering::SeqCst)),
            (
                "isolated_launches",
                self.isolated_launches.load(Ordering::SeqCst),
            ),
        ]
    }
}

struct BridgePlatform {
    /// `None` proves the counters move when no bridge is installed.
    bridge: Option<Arc<Bridge>>,
    upstream: Arc<Upstream>,
    /// The executable `process_fingerprint` proves for the pid (the Linux
    /// adapter reads `/proc/<pid>/exe`); `None` stops core's pid launch
    /// branch before it spawns.
    launch_executable: Option<String>,
}

impl BridgePlatform {
    fn hook(&self) -> Option<&Arc<Bridge>> {
        self.bridge.as_ref()
    }
}

#[async_trait]
impl BrowserPlatform for BridgePlatform {
    fn isolated_browser_executable(&self) -> Result<String, BrowserRefusal> {
        if let Some(bridge) = self.hook() {
            return Err(bridge.hook_isolated_launch());
        }
        self.upstream
            .isolated_launches
            .fetch_add(1, Ordering::SeqCst);
        Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "no installed Chromium executable",
        ))
    }

    async fn classify_browser(&self, _pid: i64) -> Result<BrowserClassification, BrowserRefusal> {
        Ok(BrowserClassification {
            is_browser: true,
            engine: BrowserEngineFamily::Chromium,
            product_kind: BrowserProduct::GoogleChrome,
            product: Some("Fixture Chrome".into()),
            channel: Some("stable".into()),
            process_role: BrowserProcessRole::StandaloneConsumer,
            supports_cdp: true,
        })
    }

    async fn native_window(
        &self,
        pid: i64,
        window_id: u64,
    ) -> Result<NativeWindowInfo, BrowserRefusal> {
        Ok(NativeWindowInfo {
            pid,
            window_id,
            title: "Fixture - Chrome".into(),
            bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
            geometry_exact: true,
            ownership: NativeOwnershipProof {
                method: NativeOwnershipMethod::WindowServerOwner,
                owner_pid: pid,
                detail: None,
            },
        })
    }

    async fn is_only_exact_native_window(
        &self,
        _pid: i64,
        _window_id: u64,
    ) -> Result<Option<bool>, BrowserRefusal> {
        Ok(Some(true))
    }

    async fn discover_owned_endpoint(
        &self,
        _pid: i64,
    ) -> Result<Option<crate::browser::types::OwnedEndpoint>, BrowserRefusal> {
        if let Some(bridge) = self.hook() {
            return bridge.hook_discover_owned();
        }
        self.upstream.port_scans.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn discover_existing_profile_endpoint(
        &self,
        pid: i64,
    ) -> Result<Option<crate::browser::types::OwnedEndpoint>, BrowserRefusal> {
        if let Some(bridge) = self.hook() {
            return bridge.hook_discover_existing(pid);
        }
        self.upstream
            .active_port_reads
            .fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn reprove_existing_profile_endpoint(
        &self,
        pid: i64,
        expected_ws_url: &str,
    ) -> Result<Option<crate::browser::types::OwnedEndpoint>, BrowserRefusal> {
        if let Some(bridge) = self.hook() {
            return bridge.hook_reprove_existing(pid, expected_ws_url);
        }
        self.upstream
            .active_port_reads
            .fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn setup_existing_profile_endpoint(
        &self,
        _request: ExistingProfileSetupRequest,
    ) -> Result<ExistingProfileSetupOutcome, BrowserRefusal> {
        if let Some(bridge) = self.hook() {
            return Err(bridge.hook_setup_existing());
        }
        self.upstream.settings_ui.fetch_add(1, Ordering::SeqCst);
        Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserRequiresSetup,
            "no setup path without a bridge",
        ))
    }

    async fn handle_existing_profile_consent(
        &self,
        _request: BrowserConsentRequest,
    ) -> Result<BrowserConsentOutcome, BrowserRefusal> {
        Ok(BrowserConsentOutcome::Accepted)
    }

    async fn process_fingerprint(&self, pid: i64) -> Result<ProcessFingerprint, BrowserRefusal> {
        Ok(ProcessFingerprint {
            pid,
            start_time: Some(1),
            executable: self.launch_executable.clone(),
        })
    }

    /// The Linux adapter's shape: a pid is answered from
    /// `discover_owned_endpoint`, "none found" is `browser_requires_setup`.
    async fn prepare_endpoint(
        &self,
        request: PrepareRequest,
    ) -> Result<PrepareOutcome, BrowserRefusal> {
        let Some(pid) = request.pid else {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserRequiresSetup,
                "fixture: pid-free isolated launch is handled by shared core",
            ));
        };
        if let Some(endpoint) = self.discover_owned_endpoint(pid).await? {
            return Ok(PrepareOutcome {
                action: PrepareAction::AlreadyPrepared,
                prepared_pid: Some(endpoint.ownership.owner_pid),
                endpoint: Some(endpoint),
                message: "fixture: owned endpoint".into(),
                side_effects: Default::default(),
                attachment: None,
            });
        }
        Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserRequiresSetup,
            "fixture: no owned endpoint",
        ))
    }

    /// Every isolated launch core builds ends here; counted, never run.
    fn spawn_isolated_browser(
        &self,
        _command: Command,
        _profile: &Path,
    ) -> Result<Box<dyn IsolatedBrowserProcess>, BrowserRefusal> {
        self.upstream
            .isolated_launches
            .fetch_add(1, Ordering::SeqCst);
        Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "fixture: isolated launches are counted, not run",
        ))
    }
}

// ── Harness ────────────────────────────────────────────────────────────────

struct Harness {
    bridge: Arc<Bridge>,
    extension: Extension,
    upstream: Arc<Upstream>,
    engine: Arc<BrowserEngine>,
}

fn host_info(instance: &str) -> HostInfo {
    HostInfo {
        instance: instance.to_owned(),
        ext: "1.4.0".to_owned(),
        file_access: true,
        host_pid: 4242,
        browser_pid: PID,
        session: format!("driver-session-{instance}"),
    }
}

fn engine_for(bridge: Option<Arc<Bridge>>, upstream: Arc<Upstream>) -> Arc<BrowserEngine> {
    engine_on(BridgePlatform {
        bridge,
        upstream,
        launch_executable: None,
    })
}

fn engine_on(platform: BridgePlatform) -> Arc<BrowserEngine> {
    BrowserEngine::new_with_protected_consent_provider(
        Arc::new(platform),
        Some(Arc::new(AcceptProvider)),
    )
}

struct AcceptProvider;

#[async_trait]
impl crate::consent::ProtectedConsentProvider for AcceptProvider {
    fn provider_id(&self) -> &'static str {
        "test.browser-bridge-provider"
    }

    async fn request_consent(
        &self,
        request: &crate::consent::ConsentRequest,
    ) -> Result<crate::consent::ProviderDecision, String> {
        Ok(crate::consent::ProviderDecision {
            action: crate::consent::ConsentAction::Accept,
            request_digest: request.request_digest.clone(),
        })
    }
}

fn no_overrides() -> Override {
    Arc::new(|_: &MockCall| None)
}

/// A harness whose bridge has a connected host.
fn connected_harness(overrides: Override) -> Harness {
    connected_harness_with(BridgeOptions::default(), overrides)
}

fn connected_harness_with(options: BridgeOptions, overrides: Override) -> Harness {
    let state: SharedState = Arc::new(StdMutex::new(FixtureState::default()));
    let (wire, extension) = Extension::start(fixture_handler(state), overrides);
    let bridge = Bridge::new(options);
    bridge.attach_host(host_info("instance-1"), wire);
    let upstream = Arc::new(Upstream::default());
    let engine = engine_for(Some(bridge.clone()), upstream.clone());
    Harness {
        bridge,
        extension,
        upstream,
        engine,
    }
}

/// A harness whose bridge is configured but has no host yet.
fn waiting_harness() -> Harness {
    let state: SharedState = Arc::new(StdMutex::new(FixtureState::default()));
    let (wire, extension) = Extension::start(fixture_handler(state), no_overrides());
    // The wire is dropped: no host is connected, the config exists.
    drop(wire);
    let bridge = Bridge::new(BridgeOptions::default());
    let upstream = Arc::new(Upstream::default());
    let engine = engine_for(Some(bridge.clone()), upstream.clone());
    Harness {
        bridge,
        extension,
        upstream,
        engine,
    }
}

fn structured(result: &crate::protocol::ToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("structured content")
}

async fn prepare(h: &Harness) -> Value {
    let result = BrowserPrepareTool::new(h.engine.clone())
        .invoke(json!({
            "pid": PID,
            "window_id": WINDOW,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "strategy": {"kind": "existing_profile"},
        }))
        .await;
    structured(&result)
}

async fn bind(h: &Harness) -> (String, String, Value) {
    let result = GetBrowserStateTool::new(h.engine.clone())
        .invoke(json!({
            "pid": PID,
            "window_id": WINDOW,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
        }))
        .await;
    let bound = structured(&result);
    assert_eq!(bound["status"], "ok", "bind must succeed: {bound}");
    assert_eq!(bound["binding_quality"], "exact", "{bound}");
    let target = bound["target_id"].as_str().unwrap().to_owned();
    let tab = bound["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
    (target, tab, bound)
}

async fn snapshot(h: &Harness, target: &str, tab: &str) -> Value {
    let result = GetBrowserStateTool::new(h.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
        }))
        .await;
    structured(&result)
}

fn ref_of(snapshot: &Value, label: &str) -> String {
    snapshot["refs"]
        .as_array()
        .unwrap_or_else(|| panic!("no refs in {snapshot}"))
        .iter()
        .find(|entry| entry["label"].as_str().unwrap_or("").contains(label))
        .unwrap_or_else(|| panic!("no ref labelled ~{label:?}: {snapshot}"))["ref"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn click(h: &Harness, target: &str, tab: &str) -> Value {
    let result = BrowserClickTool::new(h.engine.clone())
        .invoke(json!({
            "target_id": target,
            "tab_id": tab,
            "session": SESSION,
            "x": 10,
            "y": 20,
            "delivery_mode": "foreground",
        }))
        .await;
    structured(&result)
}

// ── Acceptance 7: D1 — nothing touches a DevTools path ─────────────────────

#[tokio::test]
async fn without_a_host_prepare_refuses_and_no_devtools_path_runs() {
    let h = waiting_harness();
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "refused", "{prepared}");
    assert_eq!(prepared["refusal"]["code"], "browser_requires_setup");
    assert_eq!(
        prepared["refusal"]["detail"]["reason"],
        "extension_host_not_connected"
    );
    assert_eq!(prepared["refusal"]["detail"]["retryable"], true);
    assert_eq!(
        prepared["refusal"]["detail"]["extension_bridge"]["hostConnected"],
        false
    );
    assert!(
        prepared["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("retry browser_prepare"),
        "{prepared}"
    );

    assert_eq!(
        h.upstream.all(),
        [
            ("port_scans", 0),
            ("active_port_reads", 0),
            ("settings_ui", 0),
            ("isolated_launches", 0),
        ]
    );
}

#[tokio::test]
async fn without_a_bridge_the_same_call_does_reach_the_upstream_paths() {
    // The counters above are only meaningful if they can move; this control
    // runs the same prepare against a platform with no bridge installed.
    let upstream = Arc::new(Upstream::default());
    let engine = engine_for(None, upstream.clone());
    let result = BrowserPrepareTool::new(engine)
        .invoke(json!({
            "pid": PID,
            "window_id": WINDOW,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "strategy": {"kind": "existing_profile"},
        }))
        .await;
    let refused = structured(&result);
    assert_eq!(refused["status"], "refused", "{refused}");
    let counters = upstream.all();
    assert!(
        counters[1].1 >= 1,
        "the control must read DevToolsActivePort: {counters:?}"
    );
    assert!(
        counters[2].1 >= 1,
        "the control must reach the settings UI path: {counters:?}"
    );
}

/// `browser_prepare` with a pid, no strategy and `allow_launch`: core asks
/// the platform's `prepare_endpoint` and, on `browser_requires_setup`, falls
/// through to an isolated launch of that pid's own executable with
/// `--remote-debugging-port=0`. That branch never calls
/// `isolated_browser_executable`, so its hook does not cover it.
async fn prepare_isolated_launch_of_pid(bridge: Option<Arc<Bridge>>) -> (Value, Arc<Upstream>) {
    let upstream = Arc::new(Upstream::default());
    let engine = engine_on(BridgePlatform {
        bridge,
        upstream: upstream.clone(),
        launch_executable: Some("/opt/google/chrome/chrome".into()),
    });
    let result = BrowserPrepareTool::new(engine)
        .invoke(json!({
            "pid": PID,
            "window_id": WINDOW,
            "session": SESSION,
            "_transport_session_id": TRANSPORT,
            "allow_launch": true,
            "profile": {"mode": "isolated_new"},
        }))
        .await;
    (structured(&result), upstream)
}

#[tokio::test]
async fn an_isolated_launch_of_the_pids_browser_is_refused_with_a_bridge() {
    let (refused, upstream) =
        prepare_isolated_launch_of_pid(Some(Bridge::new(BridgeOptions::default()))).await;
    assert_eq!(refused["status"], "refused", "{refused}");
    assert_eq!(
        refused["refusal"]["code"], "browser_route_unavailable",
        "{refused}"
    );
    assert_eq!(
        refused["refusal"]["detail"]["reason"], "extension_bridge_configured",
        "{refused}"
    );
    assert_eq!(
        upstream.all(),
        [
            ("port_scans", 0),
            ("active_port_reads", 0),
            ("settings_ui", 0),
            ("isolated_launches", 0),
        ]
    );
}

#[tokio::test]
async fn without_a_bridge_the_pid_branch_does_reach_an_isolated_launch() {
    // The control: the same call with no bridge must spawn, or the test above
    // proves nothing.
    let (_, upstream) = prepare_isolated_launch_of_pid(None).await;
    let counters = upstream.all();
    assert!(
        counters[0].1 >= 1,
        "the control must scan for a port: {counters:?}"
    );
    assert_eq!(
        counters[3].1, 1,
        "the control must reach spawn_isolated_browser: {counters:?}"
    );
}

// ── Acceptance 2: the full chain ───────────────────────────────────────────

#[tokio::test]
async fn prepare_then_bind_navigate_click_and_type_succeed() {
    let h = connected_harness(no_overrides());
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    assert_eq!(
        prepared["action"], "attached_existing_profile",
        "{prepared}"
    );
    assert_eq!(
        prepared["side_effects"]["displayed_consent_prompt"], false,
        "the extension route opens no browser-level prompt"
    );

    let (target, tab, bound) = bind(&h).await;
    assert_eq!(bound["tabs"].as_array().unwrap().len(), 1, "{bound}");

    let navigated = structured(
        &BrowserNavigateTool::new(h.engine.clone())
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "url": "https://fixture.test/next",
                "session": SESSION,
            }))
            .await,
    );
    assert_eq!(navigated["status"], "ok", "{navigated}");

    let clicked = click(&h, &target, &tab).await;
    assert_eq!(clicked["status"], "ok", "{clicked}");

    let snap = snapshot(&h, &target, &tab).await;
    let input = ref_of(&snap, "Shadow Input");
    let typed = structured(
        &BrowserTypeTool::new(h.engine.clone())
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "ref": input,
                "text": "hello bridge",
                "session": SESSION,
            }))
            .await,
    );
    assert_eq!(typed["status"], "ok", "{typed}");

    let methods = h.extension.cdp_methods();
    for want in [
        "Page.navigate",
        "Input.dispatchMouseEvent",
        "Input.insertText",
    ] {
        assert!(
            methods.iter().any(|method| method == want),
            "{want} missing from {methods:?}"
        );
    }
}

// ── Acceptance 8: one owned tab on a first claim ───────────────────────────

#[tokio::test]
async fn a_first_claim_opens_exactly_one_own_tab_and_a_rebind_opens_none() {
    let state: SharedState = Arc::new(StdMutex::new(FixtureState::default()));
    let (wire, extension) = Extension::start(fixture_handler(state.clone()), no_overrides());
    let bridge = Bridge::new(BridgeOptions::default());
    bridge.attach_host(host_info("instance-1"), wire);
    let upstream = Arc::new(Upstream::default());
    let engine = engine_for(Some(bridge.clone()), upstream.clone());
    // The extension owns no tab at first.
    extension.tabs.lock().unwrap().tabs.clear();
    let h = Harness {
        bridge,
        extension,
        upstream,
        engine,
    };

    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    assert_eq!(h.extension.count_op("tab.open"), 1);
    let opens: Vec<Value> = h
        .extension
        .frames()
        .into_iter()
        .filter(|frame| frame["op"] == "tab.open")
        .collect();
    assert_eq!(
        opens[0]["params"],
        json!({"url": "about:blank", "active": true})
    );

    let _ = bind(&h).await;
    assert_eq!(
        h.extension.count_op("tab.open"),
        1,
        "a later bind must reuse the owned tab"
    );
}

// ── Acceptance 3: a suspended mouse call and a dialog ──────────────────────

fn dialog_overrides() -> Override {
    let blocked = Arc::new(StdMutex::new(false));
    Arc::new(move |call: &MockCall| {
        let mut blocked = blocked.lock().unwrap();
        match call.method.as_str() {
            "Input.dispatchMouseEvent" if call.params["type"] == "mouseReleased" => {
                *blocked = true;
                Some(MockReply::hang().with_events(vec![MockEvent {
                    method: "Page.javascriptDialogOpening".into(),
                    params: json!({"type": "confirm", "message": "Delete?", "url": "https://fixture.test/"}),
                    session_id: None,
                }]))
            }
            "Input.dispatchMouseEvent" | "Emulation.setFocusEmulationEnabled" if *blocked => {
                Some(MockReply::hang())
            }
            "Page.handleJavaScriptDialog" if *blocked => {
                *blocked = false;
                Some(MockReply::ok(json!({})).with_events(vec![MockEvent {
                    method: "Page.javascriptDialogClosed".into(),
                    params: json!({"result": call.params["accept"], "userInput": ""}),
                    session_id: None,
                }]))
            }
            _ => None,
        }
    })
}

#[tokio::test]
async fn a_click_returns_while_its_mouse_call_is_suspended_and_a_dialog_opens() {
    let h = connected_harness(dialog_overrides());
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    let (target, tab, _) = bind(&h).await;

    let started = std::time::Instant::now();
    let clicked = click(&h, &target, &tab).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the click must not wait out the CDP deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(clicked["status"], "ok", "{clicked}");
    assert_eq!(clicked["dialog"]["present"], true, "{clicked}");
    assert_eq!(clicked["dialog"]["kind"], "confirm", "{clicked}");
    let dialog_id = clicked["dialog"]["dialog_id"].as_str().unwrap().to_owned();

    let accepted = structured(
        &BrowserDialogTool::new(h.engine.clone())
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "action": "accept",
                "dialog_id": dialog_id,
                "session": SESSION,
                "delivery_mode": "foreground",
            }))
            .await,
    );
    assert_eq!(accepted["status"], "ok", "{accepted}");
    let handled: Vec<(String, Value)> = h
        .extension
        .received
        .lock()
        .unwrap()
        .cdp
        .iter()
        .filter(|(method, _)| method == "Page.handleJavaScriptDialog")
        .cloned()
        .collect();
    assert_eq!(handled.len(), 1, "{handled:?}");
    assert_eq!(handled[0].1["accept"], true);
}

// ── Acceptances 4 and 5: a closed or detached tab ──────────────────────────

async fn bound_harness() -> (Harness, String, String) {
    let h = connected_harness(no_overrides());
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    let (target, tab, _) = bind(&h).await;
    (h, target, tab)
}

#[tokio::test]
async fn a_closed_tab_refuses_the_next_mutation_with_tab_not_found() {
    let (h, target, tab) = bound_harness().await;

    h.extension.send_event("tab.closed", TAB);
    h.extension
        .tabs
        .lock()
        .unwrap()
        .tabs
        .retain(|entry| entry["tab"] != json!(TAB));
    // Let the endpoint turn the event into `Target.detachedFromTarget`.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let clicked = click(&h, &target, &tab).await;
    assert_eq!(clicked["status"], "refused", "{clicked}");
    assert_eq!(
        clicked["refusal"]["code"], "browser_tab_not_found",
        "the pinned error code for a closed tab: {clicked}"
    );
}

#[tokio::test]
async fn a_detached_tab_refuses_the_next_mutation_without_a_silent_retry() {
    let (h, target, tab) = bound_harness().await;
    let before = h
        .extension
        .cdp_methods()
        .iter()
        .filter(|method| *method == "Input.dispatchMouseEvent")
        .count();

    h.extension.send_event("tab.detached", TAB);
    h.extension.revoke(TAB);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    let clicked = click(&h, &target, &tab).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a detached tab must refuse at once"
    );
    assert_eq!(clicked["status"], "refused", "{clicked}");
    // Pinned at implementation time (acceptance 5): the endpoint maps
    // `tab.detached` and `tab.closed` onto the same lost target.
    assert_eq!(
        clicked["refusal"]["code"], "browser_tab_not_found",
        "{clicked}"
    );
    let after = h
        .extension
        .cdp_methods()
        .iter()
        .filter(|method| *method == "Input.dispatchMouseEvent")
        .count();
    assert_eq!(
        after, before,
        "a refused mutation must not retry over the wire"
    );
}

// ── Acceptance 6: a new extension instance ─────────────────────────────────

#[tokio::test]
async fn a_new_instance_fails_in_flight_calls_and_invalidates_the_old_binding() {
    let hanging: Override = Arc::new(|call: &MockCall| {
        (call.method == "Input.dispatchMouseEvent").then(MockReply::hang)
    });
    let (mut h, target, tab) = bound_harness_with(hanging).await;

    // A click is in flight when the extension restarts: its mouse call is
    // suspended like a renderer that never answers.
    let engine = h.engine.clone();
    let in_flight = tokio::spawn(async move {
        BrowserClickTool::new(engine)
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "session": SESSION,
                "x": 10,
                "y": 20,
                "delivery_mode": "foreground",
            }))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!in_flight.is_finished(), "the mouse call must be suspended");

    // The listener supersedes the old host session and hands over the new
    // instance; the old wire ends.
    let state: SharedState = Arc::new(StdMutex::new(FixtureState::default()));
    let (wire, new_extension) = Extension::start(fixture_handler(state.clone()), no_overrides());
    h.bridge.attach_host(host_info("instance-2"), wire);
    h.extension.shutdown();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), in_flight)
        .await
        .expect("the in-flight call must fail, not wait out the deadline")
        .expect("the task must finish");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the in-flight call must fail as soon as the session ends"
    );
    let failed = structured(&result);
    assert_eq!(failed["status"], "refused", "{failed}");
    assert!(
        new_extension.cdp_methods().is_empty(),
        "an in-flight call from the old session must not run on the new instance"
    );
}

#[tokio::test]
async fn a_new_instance_leaves_the_old_binding_refused() {
    let mut h = connected_harness(no_overrides());
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    let (target, tab, _) = bind(&h).await;

    // A fresh Chrome run connects with a new instance.
    let state: SharedState = Arc::new(StdMutex::new(FixtureState::default()));
    let (wire, new_extension) = Extension::start(fixture_handler(state.clone()), no_overrides());
    h.bridge.attach_host(host_info("instance-2"), wire);
    h.extension.shutdown();

    let clicked = click(&h, &target, &tab).await;
    assert_eq!(clicked["status"], "refused", "{clicked}");
    // Pinned at implementation time: with the old host session gone and the
    // endpoint no longer provable against the new instance, the binding is
    // refused as requiring setup again instead of silently acting on the new
    // host.
    assert_eq!(
        clicked["refusal"]["code"], "browser_requires_setup",
        "{clicked}"
    );
    assert_eq!(
        clicked["refusal"]["message"],
        "the approved existing-profile DevTools endpoint is no longer available",
        "{clicked}"
    );
    assert!(
        new_extension.cdp_methods().is_empty(),
        "the old binding must never reach the new instance: {:?}",
        new_extension.cdp_methods()
    );
}

async fn bound_harness_with(overrides: Override) -> (Harness, String, String) {
    let h = connected_harness(overrides);
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    let (target, tab, _) = bind(&h).await;
    (h, target, tab)
}

// ── Acceptance 9: upload roots fence the tool ──────────────────────────────

fn file_input_document() -> Value {
    json!({
        "root": {
            "nodeType": 9,
            "nodeName": "#document",
            "documentURL": "https://fixture.test/",
            "frameId": "F_MAIN",
            "children": [{
                "nodeType": 1,
                "nodeName": "HTML",
                "backendNodeId": 1,
                "children": [{
                    "nodeType": 1,
                    "nodeName": "INPUT",
                    "backendNodeId": 40,
                    "attributes": ["type", "file", "id", "upload"],
                }]
            }]
        }
    })
}

fn file_input_overrides() -> Override {
    Arc::new(|call: &MockCall| match call.method.as_str() {
        "DOM.getDocument" => Some(MockReply::ok(file_input_document())),
        "DOM.getBoxModel" if call.params["backendNodeId"] == 40 => Some(MockReply::ok(json!({
            "model": {"content": [10.0, 10.0, 30.0, 10.0, 30.0, 20.0, 10.0, 20.0]}
        }))),
        "DOM.describeNode" if call.params["backendNodeId"] == 40 => Some(MockReply::ok(json!({
            "node": {"nodeName": "INPUT", "attributes": ["type", "file", "id", "upload"]}
        }))),
        "DOM.setFileInputFiles" => Some(MockReply::ok(json!({}))),
        _ => None,
    })
}

#[tokio::test]
async fn upload_roots_fence_browser_set_input_files() {
    let root = tempfile::tempdir().unwrap();
    let inside = root.path().join("ok.txt");
    std::fs::write(&inside, "hello").unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("no.txt");
    std::fs::write(&outside, "hello").unwrap();

    let h = connected_harness_with(
        BridgeOptions {
            upload_roots: vec![root.path().to_string_lossy().into_owned()],
            ..BridgeOptions::default()
        },
        file_input_overrides(),
    );
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    let (target, tab, _) = bind(&h).await;
    let snap = snapshot(&h, &target, &tab).await;
    let file_ref = ref_of(&snap, "upload");

    let denied = structured(
        &BrowserSetInputFilesTool::new(h.engine.clone())
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "ref": file_ref,
                "files": [outside.to_string_lossy()],
                "session": SESSION,
            }))
            .await,
    );
    assert_eq!(denied["status"], "refused", "{denied}");
    assert!(
        denied["refusal"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("path_denied"),
        "{denied}"
    );

    let accepted = structured(
        &BrowserSetInputFilesTool::new(h.engine.clone())
            .invoke(json!({
                "target_id": target,
                "tab_id": tab,
                "ref": file_ref,
                "files": [inside.to_string_lossy()],
                "session": SESSION,
            }))
            .await,
    );
    assert_eq!(accepted["status"], "ok", "{accepted}");
}

// ── Acceptance 10: the bridge state is exposed ─────────────────────────────

#[tokio::test]
async fn the_bridge_status_names_the_connected_host_and_the_whats_missing_case() {
    let h = connected_harness(no_overrides());
    let prepared = prepare(&h).await;
    assert_eq!(prepared["status"], "ok", "{prepared}");
    // `attached` / `owned` come from a ping; in the driver the runner thread
    // refreshes it every 30 s and right after a host connects.
    h.bridge
        .refresh_ping()
        .await
        .expect("the host answers ping");
    let status = h.bridge.status();
    assert_eq!(status["hostConnected"], true);
    assert_eq!(status["installed"], true);
    assert_eq!(status["version"], "1.4.0");
    assert_eq!(status["instance"], "instance-1");
    assert_eq!(status["fileAccess"], true);
    assert_eq!(status["browserPid"], PID);
    assert_eq!(status["attached"], true);
    assert_eq!(status["owned"], 1);
}
