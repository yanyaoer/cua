// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The native-messaging host (`cua-driver ext-host`, spec §5).
//!
//! Chrome starts the host with the caller origin as the first argument and
//! talks to it over stdin/stdout. The host
//! 1. checks the origin against the bridge config — on mismatch it writes
//!    nothing and exits 2;
//! 2. waits at most [`HELLO_TIMEOUT`] for the extension's `hello`;
//! 3. only then connects to the driver socket (at most
//!    [`CONNECT_TIMEOUT`]; failure → `bye driver_unavailable`, exit 0);
//! 4. sends `host_hello`, turns the driver's `driver_hello` into `welcome`;
//! 5. forwards frames verbatim in both directions until stdin ends, the
//!    socket closes or the driver sends `bye`.
//!
//! stdout carries frames only; diagnostics go to stderr. The host never
//! touches the socket before a valid `hello` (spec invariant 7).
//!
//! Exit codes: 0 orderly end (including `driver_unavailable`), 1 protocol
//! or I/O failure, 2 refused start (usage, config, origin).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::config::{self, BridgeConfig};
use super::frame::{
    parse_envelope, read_frame, write_frame, FrameType, MAX_FROM_CHROME, MAX_TO_CHROME,
    PROTOCOL_VERSION,
};

/// How long the host waits for the extension's `hello` (spec §5 step 2).
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the host tries to connect to the driver (spec §5 step 3).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the host waits for `driver_hello` after `host_hello`.
pub const DRIVER_HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest handshake frame the host reads from the driver.
const MAX_HANDSHAKE_FRAME: usize = 64 * 1024;

pub const EXIT_OK: i32 = 0;
pub const EXIT_PROTOCOL: i32 = 1;
pub const EXIT_REFUSED: i32 = 2;

/// Subcommand name for the explicit form.
pub const SUBCOMMAND: &str = "ext-host";
const ORIGIN_SCHEME: &str = "chrome-extension://";

/// A host invocation parsed from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostArgs {
    pub config: Option<PathBuf>,
    pub origin: String,
}

/// `Some` when argv asks for host mode: either Chrome's form
/// (`<exe> chrome-extension://<id>/ [ignored...]`) or the explicit
/// `<exe> ext-host [--config <path>] <origin>`. `Some(Err)` is a malformed
/// explicit invocation.
pub fn parse_args(args: &[OsString]) -> Option<Result<HostArgs, String>> {
    let first = args.get(1)?.to_str()?;
    if first.starts_with(ORIGIN_SCHEME) {
        return Some(Ok(HostArgs {
            config: None,
            origin: first.to_string(),
        }));
    }
    if first != SUBCOMMAND {
        return None;
    }
    let usage = || format!("usage: cua-driver {SUBCOMMAND} [--config <path>] <origin>");
    let mut rest = args[2..].iter();
    let mut config = None;
    let mut origin = None;
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--config") => match rest.next() {
                Some(path) => config = Some(PathBuf::from(path)),
                None => return Some(Err(usage())),
            },
            Some(value) if origin.is_none() && !value.starts_with("--") => {
                origin = Some(value.to_string())
            }
            _ => return Some(Err(usage())),
        }
    }
    Some(
        origin
            .map(|origin| HostArgs { config, origin })
            .ok_or_else(usage),
    )
}

/// Entry hook for `main`: runs the host and returns its exit code when argv
/// asks for host mode, `None` otherwise. Called before logging or telemetry
/// is initialised so nothing but frames reaches stdout.
pub fn run_if_requested() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let parsed = parse_args(&args)?;
    Some(match parsed {
        Ok(args) => run(args),
        Err(usage) => {
            eprintln!("{usage}");
            EXIT_REFUSED
        }
    })
}

fn run(args: HostArgs) -> i32 {
    let path = match args.config {
        Some(path) => path,
        None => match config::default_config_path() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("cua-driver ext-host: locate bridge config: {error}");
                return EXIT_REFUSED;
            }
        },
    };
    let config = match config::load(&path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!(
                "cua-driver ext-host: bridge is not configured ({} missing)",
                path.display()
            );
            return EXIT_REFUSED;
        }
        Err(error) => {
            eprintln!("cua-driver ext-host: {error}");
            return EXIT_REFUSED;
        }
    };
    if args.origin != config.expected_origin() {
        eprintln!("cua-driver ext-host: caller origin is not the configured extension");
        return EXIT_REFUSED;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cua-driver ext-host: start runtime: {error}");
            return EXIT_PROTOCOL;
        }
    };
    let code = runtime.block_on(serve(config, tokio::io::stdin(), tokio::io::stdout()));
    // A blocking stdin read may still be parked on its thread; do not wait
    // for it — the caller exits the process right away.
    runtime.shutdown_background();
    code
}

fn bye(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"v": 1, "t": "bye", "reason": reason})).expect("json")
}

/// The extension's `hello`, as far as the host needs it.
struct Hello {
    instance: Value,
    ext: Value,
    file_access: Value,
}

enum HelloOutcome {
    Ok(Hello),
    ProtoMismatch,
    Invalid(String),
}

fn check_hello(payload: &[u8]) -> HelloOutcome {
    let envelope = match parse_envelope(payload) {
        Ok(envelope) if envelope.t == FrameType::Hello => envelope,
        Ok(_) => return HelloOutcome::Invalid("first frame is not hello".into()),
        Err(error) => return HelloOutcome::Invalid(format!("first frame: {error}")),
    };
    let hello = envelope.value;
    if hello.get("proto").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
        return HelloOutcome::ProtoMismatch;
    }
    let (Some(instance), Some(ext), Some(file_access)) = (
        hello.get("instance").filter(|v| v.is_string()),
        hello.get("ext").filter(|v| v.is_string()),
        hello.get("file_access").filter(|v| v.is_boolean()),
    ) else {
        return HelloOutcome::Invalid("hello lacks instance, ext or file_access".into());
    };
    HelloOutcome::Ok(Hello {
        instance: instance.clone(),
        ext: ext.clone(),
        file_access: file_access.clone(),
    })
}

/// Runs the handshake and the forwarding loop over the given extension
/// streams; returns the exit code.
pub async fn serve<I, O>(config: BridgeConfig, mut stdin: I, mut stdout: O) -> i32
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    // 2. hello (nothing touches the socket before this passes).
    let hello =
        match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stdin, MAX_FROM_CHROME)).await {
            Err(_) => {
                eprintln!("cua-driver ext-host: no hello within {HELLO_TIMEOUT:?}");
                return EXIT_PROTOCOL;
            }
            Ok(Ok(Some(payload))) => payload,
            Ok(Ok(None)) => return EXIT_OK,
            Ok(Err(error)) => {
                eprintln!("cua-driver ext-host: read hello: {error}");
                return EXIT_PROTOCOL;
            }
        };
    let hello = match check_hello(&hello) {
        HelloOutcome::Ok(hello) => hello,
        HelloOutcome::ProtoMismatch => {
            let _ = write_frame(&mut stdout, &bye("proto_mismatch")).await;
            return EXIT_PROTOCOL;
        }
        HelloOutcome::Invalid(reason) => {
            eprintln!("cua-driver ext-host: {reason}");
            return EXIT_PROTOCOL;
        }
    };

    // 3. connect to the driver.
    let socket =
        match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&config.socket_path)).await
        {
            Ok(Ok(socket)) => socket,
            _ => {
                let _ = write_frame(&mut stdout, &bye("driver_unavailable")).await;
                return EXIT_OK;
            }
        };
    let (mut socket_read, mut socket_write) = socket.into_split();

    // 4. host_hello → driver_hello → welcome.
    let host_hello = json!({
        "v": 1,
        "t": "host_hello",
        "instance": hello.instance,
        "ext": hello.ext,
        "file_access": hello.file_access,
        "host_pid": std::process::id(),
    });
    if write_frame(
        &mut socket_write,
        &serde_json::to_vec(&host_hello).expect("json"),
    )
    .await
    .is_err()
    {
        let _ = write_frame(&mut stdout, &bye("driver_unavailable")).await;
        return EXIT_OK;
    }
    let driver_hello = match tokio::time::timeout(
        DRIVER_HELLO_TIMEOUT,
        read_frame(&mut socket_read, MAX_HANDSHAKE_FRAME),
    )
    .await
    {
        Ok(Ok(Some(payload))) => parse_envelope(&payload)
            .ok()
            .filter(|envelope| envelope.t == FrameType::DriverHello)
            .map(|envelope| envelope.value),
        _ => None,
    };
    let Some(driver_hello) = driver_hello.filter(|value| {
        value.get("session").is_some_and(Value::is_string)
            && value.get("config").is_some_and(Value::is_object)
    }) else {
        // The driver refused us (it closes without a frame) or answered
        // something else: to the extension both mean "driver unavailable".
        let _ = write_frame(&mut stdout, &bye("driver_unavailable")).await;
        return EXIT_OK;
    };
    let welcome = json!({
        "v": 1,
        "t": "welcome",
        "proto": PROTOCOL_VERSION,
        "host": env!("CARGO_PKG_VERSION"),
        "session": driver_hello["session"],
        "config": driver_hello["config"],
    });
    if write_frame(&mut stdout, &serde_json::to_vec(&welcome).expect("json"))
        .await
        .is_err()
    {
        return EXIT_OK;
    }

    // 5. forward until either side ends.
    forward(stdin, stdout, socket_read, socket_write).await
}

async fn forward<I, O, R, W>(
    mut stdin: I,
    mut stdout: O,
    mut socket_read: R,
    mut socket_write: W,
) -> i32
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // One writer owns the socket: frames from the extension and host-local
    // error events both go through it, so they never interleave.
    let (to_driver, mut to_driver_rx) = mpsc::channel::<Vec<u8>>(16);

    let mut socket_writer = tokio::spawn(async move {
        while let Some(payload) = to_driver_rx.recv().await {
            if write_frame(&mut socket_write, &payload).await.is_err() {
                return EXIT_OK;
            }
        }
        let _ = socket_write.shutdown().await;
        EXIT_OK
    });

    let from_extension = to_driver.clone();
    let mut extension_reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut stdin, MAX_FROM_CHROME).await {
                Ok(Some(payload)) => {
                    if from_extension.send(payload).await.is_err() {
                        return EXIT_OK;
                    }
                }
                Ok(None) => return EXIT_OK,
                Err(error) => {
                    eprintln!("cua-driver ext-host: read from extension: {error}");
                    return EXIT_PROTOCOL;
                }
            }
        }
    });

    let mut driver_reader = tokio::spawn(async move {
        loop {
            let payload = match read_frame(&mut socket_read, MAX_FROM_CHROME).await {
                Ok(Some(payload)) => payload,
                Ok(None) => return EXIT_OK,
                Err(error) => {
                    eprintln!("cua-driver ext-host: read from driver: {error}");
                    return EXIT_PROTOCOL;
                }
            };
            if payload.len() > MAX_TO_CHROME {
                let id = serde_json::from_slice::<Value>(&payload)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_u64));
                let mut error = json!({
                    "v": 1,
                    "t": "evt",
                    "ev": "host.error",
                    "params": {"code": "frame_too_large"},
                });
                if let Some(id) = id {
                    error["params"]["id"] = json!(id);
                }
                if to_driver
                    .send(serde_json::to_vec(&error).expect("json"))
                    .await
                    .is_err()
                {
                    return EXIT_OK;
                }
                continue;
            }
            let is_bye = payload.len() <= frame_peek_limit()
                && parse_envelope(&payload).is_ok_and(|envelope| envelope.t == FrameType::Bye);
            if write_frame(&mut stdout, &payload).await.is_err() {
                return EXIT_OK;
            }
            if is_bye {
                return EXIT_OK;
            }
        }
    });

    let code = tokio::select! {
        code = &mut extension_reader => code.unwrap_or(EXIT_PROTOCOL),
        code = &mut driver_reader => code.unwrap_or(EXIT_PROTOCOL),
        code = &mut socket_writer => code.unwrap_or(EXIT_PROTOCOL),
    };
    extension_reader.abort();
    driver_reader.abort();
    socket_writer.abort();
    code
}

/// `bye` frames are tiny; larger frames are not parsed just to look for one.
const fn frame_peek_limit() -> usize {
    4 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn chrome_form_and_explicit_form_parse() {
        let origin = format!("chrome-extension://{}/", "b".repeat(32));
        assert_eq!(
            parse_args(&os(&["cua-driver", &origin])),
            Some(Ok(HostArgs {
                config: None,
                origin: origin.clone()
            }))
        );
        // Chrome may append arguments (e.g. a parent window on other
        // platforms); they are ignored.
        assert_eq!(
            parse_args(&os(&["cua-driver", &origin, "--parent-window=0"])),
            Some(Ok(HostArgs {
                config: None,
                origin: origin.clone()
            }))
        );
        assert_eq!(
            parse_args(&os(&[
                "cua-driver",
                "ext-host",
                "--config",
                "/x.json",
                &origin
            ])),
            Some(Ok(HostArgs {
                config: Some(PathBuf::from("/x.json")),
                origin: origin.clone()
            }))
        );
        assert!(matches!(
            parse_args(&os(&["cua-driver", "ext-host"])),
            Some(Err(_))
        ));
        assert!(matches!(
            parse_args(&os(&["cua-driver", "ext-host", "--config"])),
            Some(Err(_))
        ));
        assert_eq!(parse_args(&os(&["cua-driver", "serve"])), None);
        assert_eq!(parse_args(&os(&["cua-driver"])), None);
    }
}
