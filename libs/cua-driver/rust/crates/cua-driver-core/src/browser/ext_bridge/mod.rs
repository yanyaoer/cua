// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Bridge between the driver and a browser extension over Chrome native
//! messaging.
//!
//! Topology: driver ⇄ Unix socket ⇄ native host (this binary, started by
//! Chrome) ⇄ stdin/stdout ⇄ extension. The frame format and handshake are
//! defined by the extension behaviour spec (§5) and its shared frame
//! samples, a copy of which is in `testdata/ext_bridge/frames.json`.
//!
//! - [`frame`] and [`config`] are plain code and build on every platform.
//! - [`host`], [`listener`] and [`peer`] use Unix sockets and `/proc`, and
//!   exist on Linux only. Asking for host mode on another platform is an
//!   explicit error ([`run_host_if_requested`]).
//!
//! Nothing here starts on its own: `main` routes host-mode argv to
//! [`run_host_if_requested`]; the listener is started only by a caller that
//! has a bridge config.

pub mod config;
pub mod frame;
#[cfg(target_os = "linux")]
pub mod host;
#[cfg(target_os = "linux")]
pub mod listener;
#[cfg(target_os = "linux")]
pub mod peer;

/// Runs the native-messaging host when argv asks for it and returns its
/// exit code; `None` for every other invocation.
#[cfg(target_os = "linux")]
pub fn run_host_if_requested() -> Option<i32> {
    host::run_if_requested()
}

/// Runs the native-messaging host when argv asks for it and returns its
/// exit code; `None` for every other invocation.
#[cfg(not(target_os = "linux"))]
pub fn run_host_if_requested() -> Option<i32> {
    let first = std::env::args_os().nth(1)?;
    let first = first.to_str()?;
    if first == "ext-host" || first.starts_with("chrome-extension://") {
        eprintln!("cua-driver ext-host: the browser extension bridge is only available on Linux");
        return Some(2);
    }
    None
}

#[cfg(test)]
mod tests {
    /// The fork is public: bridge sources carry no deployment values
    /// (extension ids, native host names, internal hosts). Deployments put
    /// them in the config file.
    #[test]
    fn sources_contain_no_deployment_literals() {
        let sources = [
            ("mod.rs", include_str!("mod.rs")),
            ("config.rs", include_str!("config.rs")),
            ("frame.rs", include_str!("frame.rs")),
            ("host.rs", include_str!("host.rs")),
            ("listener.rs", include_str!("listener.rs")),
            ("peer.rs", include_str!("peer.rs")),
        ];
        // Built at run time so this file does not match itself.
        let banned: Vec<String> = [
            ["xia", "omi"].concat(),
            ["mu", "se"].concat(),
            ["browser", "_host"].concat(),
            ["browser", "_bridge"].concat(),
            ["native", "messaging", "hosts"].concat(),
        ]
        .into();
        for (name, text) in sources {
            let lower = text.to_lowercase();
            for word in &banned {
                assert!(!lower.contains(word.as_str()), "{name} contains {word:?}");
            }
            // A Chrome extension id is 32 characters a-p.
            let mut run = 0;
            for byte in text.bytes() {
                run = if (b'a'..=b'p').contains(&byte) {
                    run + 1
                } else {
                    0
                };
                assert!(run < 32, "{name} contains an extension-id-like literal");
            }
        }
    }
}
