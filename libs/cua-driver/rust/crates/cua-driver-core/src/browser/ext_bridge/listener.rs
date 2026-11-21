// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The driver side of the bridge socket (spec §5, §14).
//!
//! The listener owns its own thread and single-threaded runtime and hands
//! verified host connections to the engine over a channel, so it does not
//! depend on the caller's runtime.
//!
//! - The socket's directory is created (or reset) to mode 0700; a stale
//!   socket at the path is removed before binding. Anything else at the
//!   path is left alone and binding fails.
//! - A connection is answered with `driver_hello` only after its
//!   `host_hello` arrived and the peer passed [`super::peer::verify_peer`];
//!   otherwise it is closed without a single frame.
//! - Exactly one host connection is active: a newly verified host first
//!   makes the previous one receive `bye superseded` and close.
//!
//! Each accepted host becomes a [`HostSession`] whose [`Wire`] carries the
//! JSON text of every frame; the sender refuses frames over the 1 MiB
//! Chrome limit.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use super::frame::{
    parse_envelope, read_frame, write_frame, FrameType, MAX_FROM_CHROME, MAX_TO_CHROME,
};
use super::peer::{verify_peer, PeerPolicy};
use crate::browser::wire::{Wire, WireReceiver, WireSender};

/// How long a connected peer has to send `host_hello`.
pub const HOST_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a superseded connection gets to flush its `bye`.
const SUPERSEDE_GRACE: Duration = Duration::from_secs(1);
const MAX_HANDSHAKE_FRAME: usize = 64 * 1024;

/// The `host_hello` of an accepted host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHello {
    pub instance: String,
    pub ext: String,
    pub file_access: bool,
    pub host_pid: u32,
}

/// One verified, active host connection.
pub struct HostSession {
    pub hello: HostHello,
    /// The `session` value sent in `driver_hello`.
    pub session: String,
    /// Frames to and from the extension, as JSON text. The receive stream
    /// ends when the host disconnects or is superseded.
    pub wire: Wire,
}

/// What the listener needs to run.
#[derive(Debug, Clone)]
pub struct ListenerOptions {
    pub socket_path: PathBuf,
    /// The `config` object of `driver_hello`.
    pub hello_config: Value,
    pub policy: PeerPolicy,
}

/// A running listener. Dropping it stops accepting and removes the socket.
pub struct ListenerHandle {
    pub sessions: mpsc::Receiver<HostSession>,
    active: Arc<AtomicUsize>,
    socket_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ListenerHandle {
    /// Host connections currently open (0 or 1).
    pub fn active_hosts(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Makes `path`'s directory a 0700 directory owned by this uid and removes a
/// stale socket at `path`.
pub fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("socket path has no parent directory"))?;
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(std::io::Error::other(format!(
                    "{} is not a directory",
                    dir.display()
                )));
            }
            let uid = unsafe { libc::geteuid() };
            if metadata.uid() != uid {
                return Err(std::io::Error::other(format!(
                    "{} is owned by uid {}, not {uid}",
                    dir.display(),
                    metadata.uid()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Err(error) => return Err(error),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} exists and is not a socket; not removing it",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Prepares the path, binds, and starts the listener thread. Returns after
/// the socket is bound (or binding failed).
pub fn spawn(options: ListenerOptions) -> std::io::Result<ListenerHandle> {
    prepare_socket_path(&options.socket_path)?;
    let (sessions_tx, sessions) = mpsc::channel(8);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let (bound_tx, bound_rx) = std::sync::mpsc::channel();
    let active = Arc::new(AtomicUsize::new(0));
    let thread_active = active.clone();
    let socket_path = options.socket_path.clone();
    std::thread::Builder::new()
        .name("cua-ext-bridge".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = bound_tx.send(Err(error));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match UnixListener::bind(&options.socket_path) {
                    Ok(listener) => {
                        let _ = bound_tx.send(Ok(()));
                        listener
                    }
                    Err(error) => {
                        let _ = bound_tx.send(Err(error));
                        return;
                    }
                };
                accept_loop(
                    listener,
                    Arc::new(options.policy),
                    options.hello_config,
                    sessions_tx,
                    thread_active,
                    shutdown_rx,
                )
                .await;
            });
        })?;
    bound_rx
        .recv()
        .map_err(|_| std::io::Error::other("listener thread exited before binding"))??;
    Ok(ListenerHandle {
        sessions,
        active,
        socket_path,
        shutdown: Some(shutdown),
    })
}

struct Verified {
    stream: UnixStream,
    hello: HostHello,
}

async fn accept_loop(
    listener: UnixListener,
    policy: Arc<PeerPolicy>,
    hello_config: Value,
    sessions: mpsc::Sender<HostSession>,
    active: Arc<AtomicUsize>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (verified_tx, verified_rx) = mpsc::channel::<Verified>(4);
    let registrar = tokio::spawn(register(verified_rx, sessions, active, hello_config));
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let policy = policy.clone();
                let verified_tx = verified_tx.clone();
                tokio::spawn(async move {
                    match handshake(stream, &policy).await {
                        Ok(verified) => {
                            let _ = verified_tx.send(verified).await;
                        }
                        Err(reason) => {
                            tracing::warn!(target: "cua_driver::ext_bridge", "refused host connection: {reason}");
                        }
                    }
                });
            }
        }
    }
    registrar.abort();
}

/// Reads `host_hello` and checks the peer. On error the stream is dropped
/// without writing anything.
async fn handshake(mut stream: UnixStream, policy: &PeerPolicy) -> Result<Verified, String> {
    let cred = stream
        .peer_cred()
        .map_err(|error| format!("peer credentials: {error}"))?;
    let payload = match tokio::time::timeout(
        HOST_HELLO_TIMEOUT,
        read_frame(&mut stream, MAX_HANDSHAKE_FRAME),
    )
    .await
    {
        Err(_) => return Err("no host_hello in time".into()),
        Ok(Ok(Some(payload))) => payload,
        Ok(Ok(None)) => return Err("closed before host_hello".into()),
        Ok(Err(error)) => return Err(format!("read host_hello: {error}")),
    };
    let envelope = parse_envelope(&payload).map_err(|error| format!("host_hello: {error}"))?;
    if envelope.t != FrameType::HostHello {
        return Err("first frame is not host_hello".into());
    }
    let value = envelope.value;
    let hello = (|| {
        Some(HostHello {
            instance: value.get("instance")?.as_str()?.to_string(),
            ext: value.get("ext")?.as_str()?.to_string(),
            file_access: value.get("file_access")?.as_bool()?,
            host_pid: u32::try_from(value.get("host_pid")?.as_u64()?).ok()?,
        })
    })()
    .ok_or("host_hello lacks instance, ext, file_access or host_pid")?;
    if cred.pid() != Some(hello.host_pid as i32) {
        return Err(format!(
            "host_pid {} is not the connecting process {:?}",
            hello.host_pid,
            cred.pid()
        ));
    }
    verify_peer(policy, cred.uid(), cred.pid()).map_err(|rejection| rejection.to_string())?;
    Ok(Verified { stream, hello })
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serialises activation so that at most one host connection is open.
async fn register(
    mut verified: mpsc::Receiver<Verified>,
    sessions: mpsc::Sender<HostSession>,
    active: Arc<AtomicUsize>,
    hello_config: Value,
) {
    let mut current: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)> = None;
    let mut counter: u64 = 0;
    while let Some(Verified { mut stream, hello }) = verified.recv().await {
        if let Some((close, mut pump)) = current.take() {
            let _ = close.send(());
            if tokio::time::timeout(SUPERSEDE_GRACE, &mut pump)
                .await
                .is_err()
            {
                pump.abort();
                let _ = pump.await;
            }
        }
        counter += 1;
        let session = format!("{}-{counter}", std::process::id());
        let driver_hello = json!({
            "v": 1,
            "t": "driver_hello",
            "session": session,
            "config": hello_config,
        });
        if write_frame(
            &mut stream,
            &serde_json::to_vec(&driver_hello).expect("json"),
        )
        .await
        .is_err()
        {
            continue;
        }
        let (out_tx, out_rx) = mpsc::channel::<String>(64);
        let (in_tx, in_rx) = mpsc::channel::<String>(64);
        let (close_tx, close_rx) = oneshot::channel();
        active.fetch_add(1, Ordering::SeqCst);
        let guard = ActiveGuard(active.clone());
        let pump_task = tokio::spawn(async move {
            let _guard = guard;
            pump(stream, out_rx, in_tx, close_rx).await;
        });
        let receiver: WireReceiver =
            Box::pin(futures_util::stream::unfold(in_rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            }));
        let host = HostSession {
            hello,
            session,
            wire: Wire {
                sender: Box::new(SocketSender(out_tx)),
                receiver,
            },
        };
        current = Some((close_tx, pump_task));
        if sessions.send(host).await.is_err() {
            // Nobody consumes sessions any more: close what we just opened.
            if let Some((close, pump)) = current.take() {
                let _ = close.send(());
                let _ = tokio::time::timeout(SUPERSEDE_GRACE, pump).await;
            }
            return;
        }
    }
}

/// Moves frames between one host socket and its session until either ends
/// or the connection is superseded.
async fn pump(
    stream: UnixStream,
    mut outgoing: mpsc::Receiver<String>,
    incoming: mpsc::Sender<String>,
    mut close: oneshot::Receiver<()>,
) {
    let (mut read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::spawn(async move {
        while let Ok(Some(payload)) = read_frame(&mut read_half, MAX_FROM_CHROME).await {
            let Ok(text) = String::from_utf8(payload) else {
                break;
            };
            if incoming.send(text).await.is_err() {
                break;
            }
        }
    });
    loop {
        tokio::select! {
            _ = &mut reader => break,
            closed = &mut close => {
                if closed.is_ok() {
                    let bye = json!({"v": 1, "t": "bye", "reason": "superseded"});
                    let _ = write_frame(&mut write_half, &serde_json::to_vec(&bye).expect("json")).await;
                }
                break;
            }
            next = outgoing.recv() => match next {
                Some(text) => {
                    if write_frame(&mut write_half, text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    reader.abort();
    let _ = write_half.shutdown().await;
}

struct SocketSender(mpsc::Sender<String>);

impl WireSender for SocketSender {
    fn send_text(&self, text: String) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if text.len() > MAX_TO_CHROME {
                anyhow::bail!(
                    "frame of {} bytes exceeds the {MAX_TO_CHROME}-byte extension limit",
                    text.len()
                );
            }
            self.0
                .send(text)
                .await
                .map_err(|_| anyhow::anyhow!("extension host connection is closed"))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::time::Instant;

    use futures_util::StreamExt;

    use super::super::frame::encode;
    use super::super::peer::tests::FakeProc;
    use super::super::peer::FileId;
    use super::*;

    struct Fixture {
        proc: FakeProc,
        socket_path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    impl Fixture {
        /// This test process plays the host: the fake process table says
        /// it runs the pinned host binary under the browser.
        fn new() -> Self {
            let proc = FakeProc::new();
            proc.process(
                std::process::id() as i32,
                &proc.host_bin,
                5,
                &proc.browser_bin,
                &["browser"],
            );
            let dir = tempfile::tempdir().unwrap();
            let socket_path = dir.path().join("bridge").join("driver.sock");
            Self {
                proc,
                socket_path,
                _dir: dir,
            }
        }

        fn options(&self, policy: PeerPolicy) -> ListenerOptions {
            ListenerOptions {
                socket_path: self.socket_path.clone(),
                hello_config: json!({"upload_roots": ["/tmp/up"]}),
                policy,
            }
        }

        fn spawn(&self) -> ListenerHandle {
            spawn(self.options(self.proc.policy(uid()))).unwrap()
        }
    }

    fn host_hello(pid: u32) -> Vec<u8> {
        encode(
            &serde_json::to_vec(&json!({
                "v": 1, "t": "host_hello", "instance": "I1", "ext": "0.1.0",
                "file_access": true, "host_pid": pid,
            }))
            .unwrap(),
        )
    }

    fn read_std_frame(stream: &mut StdUnixStream) -> Option<Value> {
        let mut header = [0_u8; 4];
        stream.read_exact(&mut header).ok()?;
        let mut payload = vec![0_u8; u32::from_ne_bytes(header) as usize];
        stream.read_exact(&mut payload).ok()?;
        Some(serde_json::from_slice(&payload).unwrap())
    }

    fn connect(path: &Path) -> StdUnixStream {
        let stream = StdUnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }

    /// Reads until EOF; returns the bytes received.
    fn drain(stream: &mut StdUnixStream) -> Vec<u8> {
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        rest
    }

    fn next_session(handle: &mut ListenerHandle) -> HostSession {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match handle.sessions.try_recv() {
                Ok(session) => return session,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("no session: {error:?}"),
            }
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn directory_is_private_and_stale_socket_is_replaced() {
        let fixture = Fixture::new();
        let dir = fixture.socket_path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&fixture.socket_path).unwrap());
        assert!(fixture.socket_path.exists(), "stale socket left behind");

        let handle = fixture.spawn();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let metadata = std::fs::symlink_metadata(&fixture.socket_path).unwrap();
        assert!(metadata.file_type().is_socket());
        connect(&fixture.socket_path);
        drop(handle);
        assert!(!fixture.socket_path.exists());
    }

    #[test]
    fn new_directory_is_created_private() {
        let fixture = Fixture::new();
        let _handle = fixture.spawn();
        let mode = std::fs::metadata(fixture.socket_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_non_socket_at_the_path_is_kept_and_binding_fails() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.socket_path.parent().unwrap()).unwrap();
        std::fs::write(&fixture.socket_path, b"keep me").unwrap();
        assert!(spawn(fixture.options(fixture.proc.policy(uid()))).is_err());
        assert_eq!(std::fs::read(&fixture.socket_path).unwrap(), b"keep me");
    }

    #[test]
    fn verified_host_gets_driver_hello_and_frames_flow_both_ways() {
        let fixture = Fixture::new();
        let mut handle = fixture.spawn();
        let mut host = connect(&fixture.socket_path);
        host.write_all(&host_hello(std::process::id())).unwrap();
        let driver_hello = read_std_frame(&mut host).unwrap();
        assert_eq!(driver_hello["t"], "driver_hello");
        assert_eq!(driver_hello["config"], json!({"upload_roots": ["/tmp/up"]}));

        let mut session = next_session(&mut handle);
        assert_eq!(driver_hello["session"], json!(session.session));
        assert_eq!(
            session.hello,
            HostHello {
                instance: "I1".into(),
                ext: "0.1.0".into(),
                file_access: true,
                host_pid: std::process::id(),
            }
        );
        assert_eq!(handle.active_hosts(), 1);

        let rt = runtime();
        let req = r#"{"v":1,"t":"req","id":1,"op":"ping"}"#;
        rt.block_on(session.wire.sender.send_text(req.into()))
            .unwrap();
        assert_eq!(read_std_frame(&mut host).unwrap()["op"], "ping");
        let res = r#"{"v":1,"t":"res","id":1,"ok":true}"#;
        host.write_all(&encode(res.as_bytes())).unwrap();
        assert_eq!(
            rt.block_on(session.wire.receiver.next()).as_deref(),
            Some(res)
        );

        let too_big = "x".repeat(MAX_TO_CHROME + 1);
        assert!(rt.block_on(session.wire.sender.send_text(too_big)).is_err());

        drop(host);
        assert_eq!(rt.block_on(session.wire.receiver.next()), None);
        assert!(wait_for(|| handle.active_hosts() == 0));
    }

    fn assert_refused(fixture: &Fixture, policy: PeerPolicy, pid: u32) {
        let handle = spawn(fixture.options(policy)).unwrap();
        let mut host = connect(&fixture.socket_path);
        host.write_all(&host_hello(pid)).unwrap();
        assert!(drain(&mut host).is_empty(), "a refused peer gets no frame");
        assert_eq!(handle.active_hosts(), 0);
        drop(handle);
    }

    #[test]
    fn refused_peers_are_closed_without_a_frame() {
        let fixture = Fixture::new();
        let mut wrong_exe = fixture.proc.policy(uid());
        wrong_exe.host_exe = FileId::of(&fixture.proc.other_bin).unwrap();
        assert_refused(&fixture, wrong_exe, std::process::id());

        let mut wrong_browser = fixture.proc.policy(uid());
        wrong_browser.browsers = vec![FileId::of(&fixture.proc.other_bin).unwrap()];
        assert_refused(&fixture, wrong_browser, std::process::id());

        // host_pid must be the connecting process.
        assert_refused(&fixture, fixture.proc.policy(uid()), std::process::id() + 1);

        fixture.proc.process(
            std::process::id() as i32,
            &fixture.proc.host_bin,
            5,
            &fixture.proc.browser_bin,
            &["browser", "--type=utility"],
        );
        assert_refused(&fixture, fixture.proc.policy(uid()), std::process::id());
    }

    #[test]
    fn a_new_host_supersedes_the_old_one() {
        let fixture = Fixture::new();
        let mut handle = fixture.spawn();
        let rt = runtime();

        let mut first = connect(&fixture.socket_path);
        first.write_all(&host_hello(std::process::id())).unwrap();
        assert_eq!(read_std_frame(&mut first).unwrap()["t"], "driver_hello");
        let mut first_session = next_session(&mut handle);
        assert_eq!(handle.active_hosts(), 1);

        let mut second = connect(&fixture.socket_path);
        second.write_all(&host_hello(std::process::id())).unwrap();
        let bye = read_std_frame(&mut first).unwrap();
        assert_eq!(bye, json!({"v": 1, "t": "bye", "reason": "superseded"}));
        assert!(drain(&mut first).is_empty());
        assert_eq!(rt.block_on(first_session.wire.receiver.next()), None);

        let hello = read_std_frame(&mut second).unwrap();
        assert_eq!(hello["t"], "driver_hello");
        let second_session = next_session(&mut handle);
        assert_ne!(second_session.session, first_session.session);
        assert_eq!(handle.active_hosts(), 1);
        drop(second);
    }
}
