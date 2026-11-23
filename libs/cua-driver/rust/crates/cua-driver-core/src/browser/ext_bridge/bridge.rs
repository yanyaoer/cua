// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The running bridge: which host is connected, how the engine reaches it,
//! and what the browser platform answers while a bridge config exists.
//!
//! How the engine gets in (design questions 1, 2, 7, 8):
//! - The extension route reuses the upstream existing-profile flow:
//!   `browser_prepare` with `strategy = existing_profile` under the usual
//!   authorization. The platform's discovery returns a [`BRIDGE_SCHEME`]
//!   endpoint instead of a DevTools port, owned by the browser process that
//!   started the verified host.
//! - [`crate::browser::cdp_ws::CdpPool`] dials that URL here instead of
//!   opening a WebSocket. A first claim (from `browser_prepare`) opens one
//!   active `about:blank` tab when the extension owns none; a reconnect
//!   never opens one.
//! - With no host connected, `browser_prepare` refuses at once with a
//!   retryable `browser_requires_setup`; the caller polls (no 35 s wait
//!   inside a tool call).
//! - The host state is written to a status file next to the socket for the
//!   caller's health check, and copied into those refusals.
//!
//! While a bridge config exists, the platform hooks ([`Bridge::hook_*`])
//! keep every DevTools-port path shut (ADR D1): no listening-port scan, no
//! `DevToolsActivePort` read, no remote-debugging settings UI and no
//! isolated launch with `--remote-debugging-port`. A config file that exists
//! but is invalid installs a bridge that never connects, so the ports stay
//! shut then too.
//!
//! Everything here is plain code; only [`run`] (listener and runner
//! threads) is Linux-only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use serde_json::{json, Value};

use super::endpoint::{Endpoint, EndpointOptions};
use crate::browser::cdp_ws::CdpConnection;
use crate::browser::refusal::{BrowserRefusal, BrowserRefusalCode};
use crate::browser::types::{
    EndpointOwnershipMethod, EndpointOwnershipProof, EndpointTransport, OwnedEndpoint,
};
use crate::browser::wire::Wire;

/// URL scheme of an extension endpoint: `cua-ext-bridge://<bridge>/<instance>`.
/// `<bridge>` names the [`Bridge`] in this process, `<instance>` the
/// extension instance (one Chrome run) the URL was proven against.
pub const BRIDGE_SCHEME: &str = "cua-ext-bridge://";

/// File name of the host status, in the socket's directory.
pub const STATUS_FILE_NAME: &str = "cua-driver-ext-bridge.status.json";

/// Whether `url` names an extension endpoint rather than a DevTools socket.
pub fn is_bridge_url(url: &str) -> bool {
    url.starts_with(BRIDGE_SCHEME)
}

fn parse_url(url: &str) -> Option<(u64, &str)> {
    let rest = url.strip_prefix(BRIDGE_SCHEME)?;
    let (bridge, instance) = rest.split_once('/')?;
    if instance.is_empty() || instance.contains('/') {
        return None;
    }
    Some((bridge.parse().ok()?, instance))
}

/// What the verified host told the driver, plus its verified parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub instance: String,
    pub ext: String,
    pub file_access: bool,
    pub host_pid: u32,
    pub browser_pid: i64,
    pub session: String,
}

/// Fixed settings of one bridge.
#[derive(Debug, Clone, Default)]
pub struct BridgeOptions {
    pub upload_roots: Vec<String>,
    /// Where the host status is written; `None` writes nothing.
    pub status_path: Option<PathBuf>,
    /// Why the bridge can never connect (invalid config, listener failed),
    /// reported in refusals and the status file.
    pub disabled_reason: Option<String>,
}

struct ActiveHost {
    info: HostInfo,
    endpoint: Endpoint,
}

/// One bridge per driver process in production; tests make their own.
pub struct Bridge {
    id: u64,
    options: BridgeOptions,
    host: StdMutex<Option<ActiveHost>>,
    /// Last `ping` result, copied into the status.
    ping: StdMutex<Option<Value>>,
}

fn registry() -> &'static StdMutex<HashMap<u64, Weak<Bridge>>> {
    static BRIDGES: OnceLock<StdMutex<HashMap<u64, Weak<Bridge>>>> = OnceLock::new();
    BRIDGES.get_or_init(|| StdMutex::new(HashMap::new()))
}

static INSTALLED: OnceLock<Arc<Bridge>> = OnceLock::new();

/// The bridge of this driver process, when a config file exists.
pub fn installed() -> Option<&'static Arc<Bridge>> {
    INSTALLED.get()
}

/// Makes `bridge` the process bridge the platform hooks consult. Only the
/// first call takes effect.
pub fn install(bridge: Arc<Bridge>) -> bool {
    INSTALLED.set(bridge).is_ok()
}

impl Bridge {
    pub fn new(options: BridgeOptions) -> Arc<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let bridge = Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            options,
            host: StdMutex::new(None),
            ping: StdMutex::new(None),
        });
        let mut bridges = registry().lock().unwrap();
        bridges.retain(|_, bridge| bridge.strong_count() > 0);
        bridges.insert(bridge.id, Arc::downgrade(&bridge));
        drop(bridges);
        bridge.write_status();
        bridge
    }

    /// The endpoint URL for extension `instance` on this bridge.
    pub fn url_for(&self, instance: &str) -> String {
        format!("{BRIDGE_SCHEME}{}/{instance}", self.id)
    }

    /// Makes a newly verified host the active one. The previous host's
    /// endpoint is dropped; its in-flight calls already failed when the
    /// listener closed its session. Must run inside a tokio runtime, which
    /// then carries the endpoint's tasks.
    pub fn attach_host(&self, info: HostInfo, wire: Wire) {
        let endpoint = super::endpoint::connect(
            wire,
            EndpointOptions {
                upload_roots: self.options.upload_roots.clone(),
                file_access: info.file_access,
            },
        );
        *self.ping.lock().unwrap() = None;
        *self.host.lock().unwrap() = Some(ActiveHost { info, endpoint });
        self.write_status();
    }

    /// The active host, when its session is still open.
    pub fn host(&self) -> Option<HostInfo> {
        let host = self.host.lock().unwrap();
        host.as_ref()
            .filter(|host| !host.endpoint.is_closed())
            .map(|host| host.info.clone())
    }

    fn live_connection(&self, instance: &str) -> Result<Arc<CdpConnection>, String> {
        let host = self.host.lock().unwrap();
        let Some(host) = host.as_ref().filter(|host| !host.endpoint.is_closed()) else {
            return Err("the browser extension host is not connected".to_owned());
        };
        if host.info.instance != instance {
            return Err(format!(
                "the browser extension instance changed ({} is now {}); the browser restarted",
                instance, host.info.instance
            ));
        }
        Ok(host.endpoint.connection().clone())
    }

    /// Sends `ping` to the active host and records the result.
    pub async fn refresh_ping(&self) -> Result<Value, String> {
        let endpoint_ping = {
            let host = self.host.lock().unwrap();
            let Some(host) = host.as_ref().filter(|host| !host.endpoint.is_closed()) else {
                return Err("the browser extension host is not connected".to_owned());
            };
            host.endpoint.clone()
        };
        let result = endpoint_ping.ping().await;
        if let Ok(value) = &result {
            *self.ping.lock().unwrap() = Some(value.clone());
        }
        self.write_status();
        result
    }

    /// The status object written to the status file and copied into
    /// refusals. Field names follow the caller's health check (spec §14).
    pub fn status(&self) -> Value {
        let host = self.host();
        let ping = self.ping.lock().unwrap().clone();
        let mut status = json!({
            "v": 1,
            "hostConnected": host.is_some(),
            "installed": host.is_some(),
        });
        if let Some(reason) = &self.options.disabled_reason {
            status["disabled"] = json!(reason);
        }
        if let Some(host) = host {
            status["version"] = json!(host.ext);
            status["instance"] = json!(host.instance);
            status["fileAccess"] = json!(host.file_access);
            status["browserPid"] = json!(host.browser_pid);
        }
        if let Some(ping) = ping {
            if let Some(attached) = ping.get("attached") {
                status["attached"] = attached.clone();
            }
            if let Some(owned) = ping.get("owned") {
                status["owned"] = owned.clone();
            }
        }
        status
    }

    /// Atomically replaces the status file (write to a temporary file in the
    /// same directory, then rename).
    pub fn write_status(&self) {
        let Some(path) = &self.options.status_path else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        let text = self.status().to_string();
        let tmp = dir.join(format!(".{STATUS_FILE_NAME}.{}", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn not_connected(&self) -> BrowserRefusal {
        let reason = if self.options.disabled_reason.is_some() {
            "extension_bridge_disabled"
        } else {
            "extension_host_not_connected"
        };
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserRequiresSetup,
            match &self.options.disabled_reason {
                Some(why) => {
                    format!("the browser extension bridge is configured but unusable: {why}")
                }
                None => {
                    "the browser extension host is not connected yet; retry browser_prepare shortly"
                        .to_owned()
                }
            },
        )
        .with_detail(json!({
            "reason": reason,
            "retryable": self.options.disabled_reason.is_none(),
            "extension_bridge": self.status(),
        }))
    }

    fn endpoint_for(&self, pid: i64) -> Result<OwnedEndpoint, BrowserRefusal> {
        let Some(host) = self.host() else {
            return Err(self.not_connected());
        };
        if host.browser_pid != pid {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserRequiresSetup,
                format!(
                    "the browser extension host belongs to browser process {}, not {pid}",
                    host.browser_pid
                ),
            )
            .with_detail(json!({
                "reason": "extension_host_other_browser",
                "retryable": false,
                "extension_bridge": self.status(),
            })));
        }
        Ok(OwnedEndpoint {
            ws_url: self.url_for(&host.instance),
            http_port: None,
            transport: EndpointTransport::ExtensionBridge,
            ownership: EndpointOwnershipProof {
                method: EndpointOwnershipMethod::PlatformAttested,
                owner_pid: host.browser_pid,
                listener_pid: Some(i64::from(host.host_pid)),
                detail: Some(
                    "browser extension native host; its peer credentials, executable and \
                     browser parent were verified on connect"
                        .to_owned(),
                ),
            },
        })
    }

    // ── Platform hooks ───────────────────────────────────────────────────
    //
    // A platform adapter calls these first in the matching
    // `BrowserPlatform` method and returns their answer whenever a bridge
    // is installed. None of them touches a port, a profile file or UI.

    /// `discover_owned_endpoint`: no DevTools port is ever looked for, and the
    /// answer is a refusal, not "none found". Core's `browser_prepare` with a
    /// `pid` treats "none found" (`browser_requires_setup`) as leave to fall
    /// through to an isolated launch of that pid's executable with
    /// `--remote-debugging-port=0`; that branch never asks
    /// `isolated_browser_executable`, so only an error here stops it. Every
    /// other caller is a driver-owned or embedded endpoint, which do not
    /// exist while a bridge is configured.
    pub fn hook_discover_owned(&self) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        Err(self.ports_closed(
            "no DevTools port is looked for while the browser extension bridge is configured",
        ))
    }

    /// `discover_existing_profile_endpoint`: the extension endpoint of the
    /// host whose browser is `pid`, or a refusal saying why there is none.
    pub fn hook_discover_existing(
        &self,
        pid: i64,
    ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        self.endpoint_for(pid).map(Some)
    }

    /// `reprove_existing_profile_endpoint`: the same endpoint only while the
    /// same extension instance is connected for the same browser.
    pub fn hook_reprove_existing(
        &self,
        pid: i64,
        expected_url: &str,
    ) -> Result<Option<OwnedEndpoint>, BrowserRefusal> {
        match self.endpoint_for(pid) {
            Ok(endpoint) if endpoint.ws_url == expected_url => Ok(Some(endpoint)),
            Ok(_) => Ok(None),
            Err(refusal) => Err(refusal),
        }
    }

    /// `setup_existing_profile_endpoint`: never opens the remote-debugging
    /// settings UI.
    pub fn hook_setup_existing(&self) -> BrowserRefusal {
        let mut refusal = self.not_connected();
        refusal.message = format!(
            "remote-debugging setup is disabled while the browser extension bridge is configured; {}",
            refusal.message
        );
        refusal
    }

    /// `isolated_browser_executable`: the isolated launch opens a DevTools
    /// port (`--remote-debugging-port=0`), so it is off with the bridge.
    pub fn hook_isolated_launch(&self) -> BrowserRefusal {
        self.ports_closed(
            "isolated browser launch opens a DevTools port and is disabled while the browser \
             extension bridge is configured",
        )
    }

    /// The refusal of every DevTools port path (ADR D1). Not
    /// `browser_requires_setup`: callers read that as "set one up".
    fn ports_closed(&self, why: &str) -> BrowserRefusal {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserRouteUnavailable,
            format!("{why}; use strategy=existing_profile"),
        )
        .with_detail(json!({"reason": "extension_bridge_configured"}))
    }
}

/// Dials an extension endpoint URL for [`crate::browser::cdp_ws::CdpPool`].
/// `open_tab` is set for a first claim from `browser_prepare` and opens one
/// owned tab when the extension owns none; a reconnect never opens one.
pub async fn dial(url: &str, open_tab: bool) -> anyhow::Result<Arc<CdpConnection>> {
    let (id, instance) =
        parse_url(url).ok_or_else(|| anyhow::anyhow!("malformed extension endpoint URL {url}"))?;
    let bridge = registry()
        .lock()
        .unwrap()
        .get(&id)
        .and_then(Weak::upgrade)
        .ok_or_else(|| anyhow::anyhow!("the extension bridge of {url} is gone"))?;
    let conn = bridge
        .live_connection(instance)
        .map_err(|error| anyhow::anyhow!(error))?;
    if open_tab {
        let endpoint = {
            let host = bridge.host.lock().unwrap();
            host.as_ref().map(|host| host.endpoint.clone())
        };
        if let Some(endpoint) = endpoint {
            endpoint
                .ensure_owned_tab()
                .await
                .map_err(|error| anyhow::anyhow!("could not open an extension tab: {error}"))?;
        }
    }
    Ok(conn)
}

/// Starts the bridge from the config file next to the executable. Does
/// nothing when the file does not exist. Linux-only; other platforms never
/// install a bridge.
#[cfg(target_os = "linux")]
pub fn start_from_default_config() {
    // `build_driver` can run more than once per process; a second listener
    // would remove the first one's socket.
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(start_once);
}

#[cfg(target_os = "linux")]
fn start_once() {
    let path = match super::config::default_config_path() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(target: "cua_driver::ext_bridge", "no executable path for the bridge config: {error}");
            return;
        }
    };
    match super::config::load(&path) {
        Ok(None) => {}
        Ok(Some(config)) => {
            if let Err(error) = run(&config) {
                tracing::error!(target: "cua_driver::ext_bridge", "extension bridge listener failed: {error}");
                install(Bridge::new(BridgeOptions {
                    disabled_reason: Some(format!("listener failed: {error}")),
                    ..BridgeOptions::default()
                }));
            }
        }
        Err(error) => {
            tracing::error!(target: "cua_driver::ext_bridge", "{error}");
            install(Bridge::new(BridgeOptions {
                disabled_reason: Some(error.to_string()),
                ..BridgeOptions::default()
            }));
        }
    }
}

/// How often the runner refreshes `ping` for the status file.
pub const STATUS_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// Starts the listener and the runner thread that hands each verified host
/// to the installed bridge.
#[cfg(target_os = "linux")]
pub fn run(config: &super::config::BridgeConfig) -> std::io::Result<Arc<Bridge>> {
    use super::listener::{self, ListenerOptions};
    use super::peer::PeerPolicy;

    let status_path = config
        .socket_path
        .parent()
        .map(|dir| dir.join(STATUS_FILE_NAME));
    let policy = PeerPolicy::for_this_process(&config.browser_executables)?;
    let mut handle = listener::spawn(ListenerOptions {
        socket_path: config.socket_path.clone(),
        hello_config: config.driver_hello_config(),
        policy,
    })?;
    let bridge = Bridge::new(BridgeOptions {
        upload_roots: config.upload_roots.clone(),
        status_path,
        disabled_reason: None,
    });
    install(bridge.clone());
    let runner = bridge.clone();
    std::thread::Builder::new()
        .name("cua-ext-bridge-hosts".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let mut tick = tokio::time::interval(STATUS_REFRESH);
                loop {
                    tokio::select! {
                        session = handle.sessions.recv() => {
                            let Some(session) = session else { break };
                            runner.attach_host(
                                HostInfo {
                                    instance: session.hello.instance,
                                    ext: session.hello.ext,
                                    file_access: session.hello.file_access,
                                    host_pid: session.hello.host_pid,
                                    browser_pid: i64::from(session.browser_pid),
                                    session: session.session,
                                },
                                session.wire,
                            );
                            let _ = runner.refresh_ping().await;
                        }
                        _ = tick.tick() => {
                            let _ = runner.refresh_ping().await;
                        }
                    }
                }
            });
        })?;
    Ok(bridge)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};

    use super::super::endpoint;
    use super::*;
    use crate::browser::refusal::BrowserRefusalCode;
    use crate::browser::wire::memory::{self, MemoryPeer};

    fn options() -> BridgeOptions {
        BridgeOptions::default()
    }

    fn host_info(instance: &str, browser_pid: i64) -> HostInfo {
        HostInfo {
            instance: instance.to_owned(),
            ext: "1.2.3".to_owned(),
            file_access: true,
            host_pid: 4242,
            browser_pid,
            session: format!("session-{instance}"),
        }
    }

    /// The far end of a memory wire, playing the extension: records every
    /// frame it receives and answers through `reply`.
    struct Script {
        frames: Arc<StdMutex<Vec<Value>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    fn script(
        mut peer: MemoryPeer,
        reply: impl Fn(&Value) -> Option<Value> + Send + 'static,
    ) -> Script {
        let frames = Arc::new(StdMutex::new(Vec::new()));
        let seen = frames.clone();
        let task = tokio::spawn(async move {
            while let Some(text) = peer.sent.recv().await {
                let Ok(request) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                seen.lock().unwrap().push(request.clone());
                if let Some(mut response) = reply(&request) {
                    response["v"] = json!(1);
                    response["t"] = json!("res");
                    response["id"] = request["id"].clone();
                    if peer.inject.send(response.to_string()).is_err() {
                        break;
                    }
                }
            }
        });
        Script {
            frames,
            _task: task,
        }
    }

    fn tab(tab: u64) -> Value {
        json!({
            "tab": tab,
            "window": 3,
            "url": "https://example.test/",
            "title": "Example",
            "status": "complete",
            "attached": true,
        })
    }

    #[test]
    fn urls_round_trip_and_reject_junk() {
        let bridge = Bridge::new(options());
        let url = bridge.url_for("instance-1");
        assert_eq!(url, format!("cua-ext-bridge://{}/instance-1", bridge.id));
        assert!(is_bridge_url(&url));
        assert_eq!(parse_url(&url), Some((bridge.id, "instance-1")));
        for bad in [
            "cua-ext-bridge://",
            "cua-ext-bridge://1/",
            "cua-ext-bridge://x/instance",
            "cua-ext-bridge://1/a/b",
        ] {
            assert_eq!(parse_url(bad), None, "{bad}");
        }
        assert!(is_bridge_url("cua-ext-bridge://1/x"));
        assert!(!is_bridge_url("ws://127.0.0.1:9222/devtools/browser/abc"));
        assert!(!is_bridge_url("ws://[::1]:9222/devtools/browser/abc"));
    }

    #[test]
    fn without_a_host_every_devtools_path_is_shut() {
        let bridge = Bridge::new(options());
        // A refusal, not "none found": core reads browser_requires_setup from
        // discovery as leave to launch the pid's browser with a DevTools port.
        let refusal = bridge.hook_discover_owned().unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRouteUnavailable);
        assert_eq!(
            refusal.detail.expect("detail")["reason"],
            "extension_bridge_configured"
        );

        let refusal = bridge.hook_discover_existing(7).unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRequiresSetup);
        let detail = refusal.detail.expect("refusal detail");
        assert_eq!(detail["reason"], "extension_host_not_connected");
        assert_eq!(detail["retryable"], true);
        assert_eq!(detail["extension_bridge"]["hostConnected"], false);

        let refusal = bridge
            .hook_reprove_existing(7, "cua-ext-bridge://1/old")
            .unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRequiresSetup);

        let refusal = bridge.hook_setup_existing();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRequiresSetup);
        assert!(
            refusal
                .message
                .contains("remote-debugging setup is disabled"),
            "{}",
            refusal.message
        );

        let refusal = bridge.hook_isolated_launch();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRouteUnavailable);
        assert_eq!(
            refusal.detail.expect("detail")["reason"],
            "extension_bridge_configured"
        );
    }

    #[test]
    fn a_disabled_bridge_refuses_as_unusable_and_says_why() {
        let bridge = Bridge::new(BridgeOptions {
            disabled_reason: Some("invalid config".to_owned()),
            ..BridgeOptions::default()
        });
        let refusal = bridge.hook_discover_existing(7).unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRequiresSetup);
        let detail = refusal.detail.expect("detail");
        assert_eq!(detail["reason"], "extension_bridge_disabled");
        assert_eq!(detail["retryable"], false);
        assert_eq!(detail["extension_bridge"]["disabled"], "invalid config");
        assert!(
            refusal.message.contains("configured but unusable"),
            "{}",
            refusal.message
        );
        let status = bridge.status();
        assert_eq!(status["hostConnected"], false);
        assert_eq!(status["installed"], false);
        assert_eq!(status["disabled"], "invalid config");
    }

    #[tokio::test]
    async fn dial_reports_malformed_unknown_missing_and_mismatched_endpoints() {
        let error = dial("cua-ext-bridge://nope", true)
            .await
            .err()
            .expect("malformed URL")
            .to_string();
        assert!(error.contains("malformed"), "{error}");
        let error = dial("cua-ext-bridge://999999/x", true)
            .await
            .err()
            .expect("unknown bridge")
            .to_string();
        assert!(error.contains("is gone"), "{error}");

        let bridge = Bridge::new(options());
        let error = dial(&bridge.url_for("i1"), true)
            .await
            .err()
            .expect("no host")
            .to_string();
        assert!(error.contains("not connected"), "{error}");

        let (wire, peer) = memory::pair();
        let state = Arc::new(StdMutex::new(vec![tab(5)]));
        let _ext = script(peer, move |request| match request["op"].as_str() {
            Some("tab.list") => Some(json!({"ok": true, "result": state.lock().unwrap().clone()})),
            _ => Some(json!({"ok": true, "result": {}})),
        });
        bridge.attach_host(host_info("i1", 7), wire);

        let error = dial(&bridge.url_for("i2"), true)
            .await
            .err()
            .expect("instance mismatch")
            .to_string();
        assert!(error.contains("instance changed"), "{error}");
        assert!(error.contains("i2 is now i1"), "{error}");

        let connection = dial(&bridge.url_for("i1"), true).await.unwrap();
        assert!(!connection.is_closed());
    }

    #[tokio::test]
    async fn a_first_claim_opens_exactly_one_blank_tab_and_a_reconnect_opens_none() {
        let bridge = Bridge::new(options());
        let (wire, peer) = memory::pair();
        let tabs: Arc<StdMutex<Vec<Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let model = tabs.clone();
        let ext = script(peer, move |request| match request["op"].as_str() {
            Some("tab.list") => Some(json!({"ok": true, "result": model.lock().unwrap().clone()})),
            Some("tab.open") => {
                model.lock().unwrap().push(tab(5));
                Some(json!({"ok": true, "result": {"tab": 5}}))
            }
            _ => Some(json!({"ok": true, "result": {}})),
        });
        bridge.attach_host(host_info("i1", 7), wire);

        dial(&bridge.url_for("i1"), true).await.unwrap();
        assert_eq!(tabs.lock().unwrap().len(), 1);

        // A reconnect lists the owned tab and opens nothing.
        dial(&bridge.url_for("i1"), false).await.unwrap();
        let frames = ext.frames.lock().unwrap().clone();
        let opens: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["op"] == "tab.open")
            .collect();
        assert_eq!(opens.len(), 1, "{frames:?}");
        assert_eq!(
            opens[0]["params"],
            json!({"url": "about:blank", "active": true})
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["op"] == "tab.list")
                .count(),
            1,
            "a reconnect dials without listing or opening: {frames:?}"
        );
    }

    #[tokio::test]
    async fn the_status_file_tracks_the_host_and_its_ping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATUS_FILE_NAME);
        let read = |path: &std::path::Path| -> Value {
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
        };

        let bridge = Bridge::new(BridgeOptions {
            status_path: Some(path.clone()),
            ..BridgeOptions::default()
        });
        let status = read(&path);
        assert_eq!(status["hostConnected"], false);
        assert_eq!(status["installed"], false);

        let (wire, peer) = memory::pair();
        let ext = script(peer, |request| match request["op"].as_str() {
            Some("ping") => Some(json!({"ok": true, "result": {
                "installed": true, "version": "9.9.9", "attached": true, "owned": 2,
            }})),
            _ => Some(json!({"ok": true, "result": {}})),
        });
        bridge.attach_host(host_info("instance-a", 7), wire);
        let status = read(&path);
        assert_eq!(status["hostConnected"], true);
        assert_eq!(status["installed"], true);
        assert_eq!(status["version"], "1.2.3");
        assert_eq!(status["instance"], "instance-a");
        assert_eq!(status["fileAccess"], true);
        assert_eq!(status["browserPid"], 7);
        assert!(status.get("attached").is_none(), "{status}");

        bridge.refresh_ping().await.expect("the host answers ping");
        let status = read(&path);
        assert_eq!(status["attached"], true);
        assert_eq!(status["owned"], 2);
        assert!(
            ext.frames
                .lock()
                .unwrap()
                .iter()
                .any(|frame| frame["op"] == "ping"),
            "refresh_ping must ask the extension"
        );
    }

    #[tokio::test]
    async fn a_host_answers_only_for_its_own_browser_process() {
        let bridge = Bridge::new(options());
        let (wire, peer) = memory::pair();
        let _ext = script(peer, |_| Some(json!({"ok": true, "result": {}})));
        bridge.attach_host(host_info("instance-a", 7), wire);

        let endpoint = bridge
            .hook_discover_existing(7)
            .expect("the bridge answers")
            .expect("an endpoint");
        assert_eq!(endpoint.ws_url, bridge.url_for("instance-a"));
        assert_eq!(endpoint.transport, EndpointTransport::ExtensionBridge);
        assert_eq!(endpoint.http_port, None);
        assert_eq!(endpoint.ownership.owner_pid, 7);
        assert_eq!(endpoint.ownership.listener_pid, Some(4242));
        let detail = endpoint
            .ownership
            .detail
            .expect("ownership detail names the proof");
        assert!(detail.contains("verified on connect"), "{detail}");

        let refusal = bridge.hook_discover_existing(8).unwrap_err();
        assert_eq!(refusal.code, BrowserRefusalCode::BrowserRequiresSetup);
        let detail = refusal.detail.expect("detail");
        assert_eq!(detail["reason"], "extension_host_other_browser");
        assert_eq!(detail["retryable"], false);
        assert!(
            refusal.message.contains("belongs to browser process 7"),
            "{}",
            refusal.message
        );
    }

    #[tokio::test]
    async fn reprove_accepts_only_the_same_connected_instance() {
        let bridge = Bridge::new(options());
        let (wire, peer) = memory::pair();
        let _ext = script(peer, |_| Some(json!({"ok": true, "result": {}})));
        bridge.attach_host(host_info("instance-a", 7), wire);
        let url = bridge.url_for("instance-a");

        let reproven = bridge
            .hook_reprove_existing(7, &url)
            .expect("the bridge answers")
            .expect("the same endpoint");
        assert_eq!(reproven.ws_url, url);
        assert!(
            bridge
                .hook_reprove_existing(7, "cua-ext-bridge://1/stale")
                .expect("the bridge answers")
                .is_none(),
            "a stale URL must not be re-proven"
        );
        assert!(bridge.hook_reprove_existing(8, &url).is_err());
    }

    #[tokio::test]
    async fn a_finished_host_leaves_the_bridge_disconnected() {
        let bridge = Bridge::new(options());
        let (wire, peer) = memory::pair();
        bridge.attach_host(host_info("instance-a", 7), wire);
        assert!(bridge.host().is_some());

        drop(peer);
        let mut disconnected = false;
        for _ in 0..200 {
            if bridge.host().is_none() {
                disconnected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(disconnected, "the bridge must notice the host ended");
        let error = dial(&bridge.url_for("instance-a"), false)
            .await
            .err()
            .expect("the host ended")
            .to_string();
        assert!(error.contains("not connected"), "{error}");
        assert!(bridge.refresh_ping().await.is_err());
    }

    #[tokio::test]
    async fn upload_roots_reach_the_endpoint_before_anything_is_forwarded() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("ok.txt");
        std::fs::write(&inside, "hello").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("no.txt");
        std::fs::write(&outside, "hello").unwrap();

        let bridge = Bridge::new(BridgeOptions {
            upload_roots: vec![root.path().to_string_lossy().into_owned()],
            ..BridgeOptions::default()
        });
        let (wire, peer) = memory::pair();
        let ext = script(peer, |request| match request["op"].as_str() {
            Some("tab.list") => Some(json!({"ok": true, "result": [tab(5)]})),
            _ => Some(json!({"ok": true, "result": {}})),
        });
        bridge.attach_host(host_info("instance-a", 7), wire);

        let endpoint = bridge.hook_discover_existing(7).unwrap().unwrap();
        let connection = dial(&endpoint.ws_url, false).await.unwrap();
        let session = endpoint::session_id(5);

        let denied = connection
            .call(
                Some(&session),
                "DOM.setFileInputFiles",
                json!({"backendNodeId": 20, "files": [outside.to_string_lossy()]}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(denied.contains("path_denied"), "{denied}");
        assert!(
            ext.frames
                .lock()
                .unwrap()
                .iter()
                .all(|frame| frame["op"] != "cdp"),
            "a denied path must never leave the process"
        );

        connection
            .call(
                Some(&session),
                "DOM.setFileInputFiles",
                json!({"backendNodeId": 20, "files": [inside.to_string_lossy()]}),
            )
            .await
            .expect("an approved path is forwarded");
        assert!(
            ext.frames
                .lock()
                .unwrap()
                .iter()
                .any(|frame| frame["op"] == "cdp"),
            "an approved path reaches the extension"
        );
    }
}
