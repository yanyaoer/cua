// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The browser-extension native host as Chrome runs it: the real
//! `cua-driver` binary on piped stdin/stdout, a fake driver socket in a
//! temporary directory (extension behaviour spec §5, §15).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const MIB: usize = 1024 * 1024;
/// Spec §5: the host exits within 100 ms of stdin EOF, socket close or bye.
const EXIT_LIMIT: Duration = Duration::from_millis(100);

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_ne_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

fn json_frame(value: &Value) -> Vec<u8> {
    frame(&serde_json::to_vec(value).unwrap())
}

/// A JSON object of exactly `len` bytes.
fn padded(len: usize, id: u64) -> Vec<u8> {
    let head = format!(r#"{{"v":1,"t":"req","id":{id},"op":"ping","pad":""#);
    let mut text = head.into_bytes();
    let fill = len - text.len() - 2;
    text.extend(std::iter::repeat_n(b'x', fill));
    text.extend_from_slice(b"\"}");
    assert_eq!(text.len(), len);
    text
}

fn read_frame(reader: &mut impl Read) -> Option<Vec<u8>> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).ok()?;
    let mut payload = vec![0_u8; u32::from_ne_bytes(header) as usize];
    reader.read_exact(&mut payload).ok()?;
    Some(payload)
}

fn extension_id() -> String {
    "a".repeat(32)
}

fn origin() -> String {
    format!("chrome-extension://{}/", extension_id())
}

struct Setup {
    _dir: tempfile::TempDir,
    config: PathBuf,
    socket: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bridge").join("driver.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let config = dir.path().join("cua-driver-ext-bridge.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "extension_id": extension_id(),
                "socket_path": socket,
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            _dir: dir,
            config,
            socket,
        }
    }

    fn driver(&self) -> FakeDriver {
        let listener = UnixListener::bind(&self.socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        FakeDriver { listener }
    }

    fn host(&self, origin: &str) -> Host {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cua-driver"))
            .args(["ext-host", "--config"])
            .arg(&self.config)
            .arg(origin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (frames_tx, frames) = mpsc::channel();
        let (bytes_tx, stdout_bytes) = mpsc::channel();
        std::thread::spawn(move || {
            let mut total = 0;
            while let Some(payload) = read_frame(&mut stdout) {
                total += 4 + payload.len();
                if frames_tx.send(payload).is_err() {
                    break;
                }
            }
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
            let _ = bytes_tx.send(total + rest.len());
        });
        Host {
            stdin: child.stdin.take(),
            child,
            frames,
            stdout_bytes,
        }
    }
}

struct FakeDriver {
    listener: UnixListener,
}

impl FakeDriver {
    fn accept(&self, within: Duration) -> Option<UnixStream> {
        let deadline = Instant::now() + within;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(20)))
                        .unwrap();
                    return Some(stream);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return None,
            }
        }
    }
}

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Vec<u8>>,
    stdout_bytes: mpsc::Receiver<usize>,
}

impl Host {
    fn send(&mut self, bytes: &[u8]) {
        self.stdin.as_mut().unwrap().write_all(bytes).unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn hello(&mut self, proto: u64) {
        self.send(&json_frame(&json!({
            "v": 1, "t": "hello", "proto": proto, "ext": "0.1.0",
            "instance": "AAAAAAAAAAAAAAAAAAAAAA", "file_access": true,
        })));
    }

    fn next(&self) -> Value {
        let payload = self
            .frames
            .recv_timeout(Duration::from_secs(10))
            .expect("frame from host");
        serde_json::from_slice(&payload).unwrap()
    }

    fn next_raw(&self) -> Vec<u8> {
        self.frames
            .recv_timeout(Duration::from_secs(20))
            .expect("frame from host")
    }

    fn wait(&mut self, within: Duration) -> (ExitStatus, Duration) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status, start.elapsed());
            }
            if start.elapsed() > within {
                let _ = self.child.kill();
                panic!("host still running after {within:?}");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Total stdout bytes, available once the host exited.
    fn stdout_total(&self) -> usize {
        self.stdout_bytes
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs the handshake; returns the host and the driver's side of the socket.
fn connected(setup: &Setup, driver: &FakeDriver) -> (Host, UnixStream) {
    let mut host = setup.host(&origin());
    host.hello(1);
    let mut socket = driver
        .accept(Duration::from_secs(5))
        .expect("host connects");
    let host_hello: Value = serde_json::from_slice(&read_frame(&mut socket).unwrap()).unwrap();
    assert_eq!(
        host_hello,
        json!({
            "v": 1, "t": "host_hello", "instance": "AAAAAAAAAAAAAAAAAAAAAA",
            "ext": "0.1.0", "file_access": true, "host_pid": host.child.id(),
        })
    );
    socket
        .write_all(&json_frame(&json!({
            "v": 1, "t": "driver_hello", "session": "s1",
            "config": {"upload_roots": ["/tmp/uploads"]},
        })))
        .unwrap();
    let welcome = host.next();
    assert_eq!(welcome["t"], "welcome");
    assert_eq!(welcome["proto"], 1);
    assert!(welcome["host"].is_string());
    assert_eq!(welcome["session"], "s1");
    assert_eq!(welcome["config"], json!({"upload_roots": ["/tmp/uploads"]}));
    (host, socket)
}

#[test]
fn wrong_origin_exits_2_without_output() {
    let setup = Setup::new();
    let driver = setup.driver();
    let mut host = setup.host(&format!("chrome-extension://{}/", "b".repeat(32)));
    let (status, _) = host.wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(2));
    assert_eq!(host.stdout_total(), 0);
    assert!(driver.accept(Duration::ZERO).is_none());
}

#[test]
fn no_hello_within_two_seconds_exits_without_touching_the_socket() {
    let setup = Setup::new();
    let driver = setup.driver();
    let mut host = setup.host(&origin());
    let (status, elapsed) = host.wait(Duration::from_secs(5));
    assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    assert_ne!(status.code(), Some(0));
    assert!(
        driver.accept(Duration::ZERO).is_none(),
        "host connected to the driver before hello"
    );
    assert_eq!(host.stdout_total(), 0);
}

#[test]
fn proto_mismatch_says_bye_and_exits() {
    let setup = Setup::new();
    let driver = setup.driver();
    let mut host = setup.host(&origin());
    host.hello(2);
    assert_eq!(
        host.next(),
        json!({"v": 1, "t": "bye", "reason": "proto_mismatch"})
    );
    let (status, _) = host.wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(1));
    assert!(driver.accept(Duration::ZERO).is_none());
}

#[test]
fn missing_socket_is_driver_unavailable_exit_0() {
    let setup = Setup::new();
    let mut host = setup.host(&origin());
    host.hello(1);
    assert_eq!(
        host.next(),
        json!({"v": 1, "t": "bye", "reason": "driver_unavailable"})
    );
    let (status, _) = host.wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(0));
}

#[test]
fn handshake_then_transparent_forwarding() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, mut socket) = connected(&setup, &driver);

    let req = br#"{"v":1,"t":"req","id":7,"op":"cdp","tab":12}"#;
    socket.write_all(&frame(req)).unwrap();
    assert_eq!(host.next_raw(), req);

    let res = br#"{"v":1,"t":"res","id":7,"ok":true,"result":{}}"#;
    host.send(&frame(res));
    assert_eq!(read_frame(&mut socket).unwrap(), res);

    // Length 0 is forwarded as-is: the host does not interpret frames
    // after the handshake.
    host.send(&frame(b""));
    assert_eq!(read_frame(&mut socket).unwrap(), b"");
}

#[test]
fn exits_quickly_when_stdin_ends() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, _socket) = connected(&setup, &driver);
    drop(host.stdin.take());
    let (status, elapsed) = host.wait(Duration::from_secs(5));
    eprintln!("stdin EOF → exit in {elapsed:?}");
    assert_eq!(status.code(), Some(0));
    assert!(elapsed <= EXIT_LIMIT, "{elapsed:?}");
}

#[test]
fn exits_quickly_when_the_socket_closes() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, socket) = connected(&setup, &driver);
    drop(socket);
    let (status, elapsed) = host.wait(Duration::from_secs(5));
    eprintln!("socket close → exit in {elapsed:?}");
    assert_eq!(status.code(), Some(0));
    assert!(elapsed <= EXIT_LIMIT, "{elapsed:?}");
}

#[test]
fn forwards_driver_bye_then_exits_quickly() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, mut socket) = connected(&setup, &driver);
    socket
        .write_all(&json_frame(
            &json!({"v": 1, "t": "bye", "reason": "superseded"}),
        ))
        .unwrap();
    let (status, elapsed) = host.wait(Duration::from_secs(5));
    eprintln!("bye → exit in {elapsed:?}");
    assert_eq!(status.code(), Some(0));
    assert!(elapsed <= EXIT_LIMIT, "{elapsed:?}");
    assert_eq!(
        host.next(),
        json!({"v": 1, "t": "bye", "reason": "superseded"})
    );
}

#[test]
fn frames_to_chrome_are_capped_at_one_mib() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (host, mut socket) = connected(&setup, &driver);

    let at_limit = padded(MIB, 1);
    socket.write_all(&frame(&at_limit)).unwrap();
    assert_eq!(host.next_raw(), at_limit);

    let over = padded(MIB + 1, 2);
    socket.write_all(&frame(&over)).unwrap();
    let error: Value = serde_json::from_slice(&read_frame(&mut socket).unwrap()).unwrap();
    assert_eq!(
        error,
        json!({"v": 1, "t": "evt", "ev": "host.error",
               "params": {"code": "frame_too_large", "id": 2}})
    );

    // The stream stays in step: the next frame is forwarded, the oversize
    // one never reached stdout.
    let small = br#"{"v":1,"t":"req","id":3,"op":"ping"}"#;
    socket.write_all(&frame(small)).unwrap();
    assert_eq!(host.next_raw(), small);
}

#[test]
fn frames_from_chrome_up_to_64_mib_are_forwarded() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, socket) = connected(&setup, &driver);
    let mut reader = socket.try_clone().unwrap();
    let received = std::thread::spawn(move || read_frame(&mut reader).map(|p| p.len()));
    let big = padded(64 * MIB, 4);
    host.send(&frame(&big));
    assert_eq!(received.join().unwrap(), Some(64 * MIB));
    drop(socket);
}

#[test]
fn a_frame_over_64_mib_from_chrome_is_a_read_error() {
    let setup = Setup::new();
    let driver = setup.driver();
    let (mut host, _socket) = connected(&setup, &driver);
    host.send(&((64 * MIB + 1) as u32).to_ne_bytes());
    let (status, _) = host.wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(1));
}

#[test]
fn config_file_missing_refuses_to_start() {
    let setup = Setup::new();
    std::fs::remove_file(&setup.config).unwrap();
    let mut host = setup.host(&origin());
    let (status, _) = host.wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(2));
    assert_eq!(host.stdout_total(), 0);
}
