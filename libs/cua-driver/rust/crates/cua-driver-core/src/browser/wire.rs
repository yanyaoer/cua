// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The text-frame layer under [`super::cdp_ws::CdpConnection`].
//!
//! A [`Wire`] is the minimal thing the CDP demultiplexer needs from a
//! transport: a way to send one JSON text frame, and an ordered stream of
//! received JSON text frames whose end means the connection is gone.
//! Ids, pending calls, event fan-out, dialog state and method policy stay in
//! `cdp_ws`; a wire knows nothing about CDP.
//!
//! The loopback WebSocket used by [`super::cdp_ws::CdpConnection::connect`]
//! is one implementation (`websocket`, crate-private). [`memory`] is an in-process pair
//! for tests and for drivers that bridge CDP over a non-WebSocket channel.
//!
//! Ordering contract: an implementation delivers received frames in the
//! order the peer sent them. `cdp_ws`'s guarantee that events emitted
//! before a reply are queued for subscribers by the time the call returns
//! depends on it.

use std::pin::Pin;

use futures_util::future::BoxFuture;
use futures_util::Stream;

/// Sends one JSON text frame. Implementations serialise concurrent sends.
/// An error means the frame was not delivered.
pub trait WireSender: Send + Sync {
    fn send_text(&self, text: String) -> BoxFuture<'_, anyhow::Result<()>>;
}

/// Received JSON text frames in arrival order. The stream ending means the
/// connection closed; it never resumes.
pub type WireReceiver = Pin<Box<dyn Stream<Item = String> + Send>>;

/// One connection's two halves.
pub struct Wire {
    pub sender: Box<dyn WireSender>,
    pub receiver: WireReceiver,
}

/// The loopback WebSocket implementation.
pub(crate) mod websocket {
    use futures_util::stream::{SplitSink, SplitStream};
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    use super::{Wire, WireReceiver, WireSender};

    type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

    struct WsSender(Mutex<SplitSink<WsStream, Message>>);

    impl WireSender for WsSender {
        fn send_text(
            &self,
            text: String,
        ) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async move {
                let mut writer = self.0.lock().await;
                writer.send(Message::Text(text)).await?;
                Ok(())
            })
        }
    }

    /// Text frames until the first read error or Close frame; binary,
    /// ping and pong frames are skipped.
    fn text_frames(read: SplitStream<WsStream>) -> WireReceiver {
        Box::pin(futures_util::stream::unfold(read, |mut read| async move {
            loop {
                match read.next().await {
                    Some(Ok(Message::Text(text))) => return Some((text, read)),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                    Some(Ok(_)) => continue,
                }
            }
        }))
    }

    pub(crate) fn wire(ws: WsStream) -> Wire {
        let (write, read) = ws.split();
        Wire {
            sender: Box::new(WsSender(Mutex::new(write))),
            receiver: text_frames(read),
        }
    }
}

/// An in-process wire pair.
pub mod memory {
    use tokio::sync::mpsc;

    use super::{Wire, WireReceiver, WireSender};

    struct MemorySender(mpsc::UnboundedSender<String>);

    impl WireSender for MemorySender {
        fn send_text(
            &self,
            text: String,
        ) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
            let sent = self.0.send(text);
            Box::pin(async move { sent.map_err(|_| anyhow::anyhow!("memory wire peer is gone")) })
        }
    }

    /// The far end of a memory wire.
    ///
    /// - `sent` yields what the connection sent, in order. Dropping it makes
    ///   every later send fail.
    /// - `inject` queues a frame for the connection to receive. Frames are
    ///   received in injection order. Dropping it (with all its clones) ends
    ///   the connection's receive stream, i.e. closes the connection.
    pub struct MemoryPeer {
        pub sent: mpsc::UnboundedReceiver<String>,
        pub inject: mpsc::UnboundedSender<String>,
    }

    /// A connected pair: the [`Wire`] for the connection and its peer.
    pub fn pair() -> (Wire, MemoryPeer) {
        let (to_peer, sent) = mpsc::unbounded_channel();
        let (inject, from_peer) = mpsc::unbounded_channel::<String>();
        let receiver: WireReceiver = Box::pin(futures_util::stream::unfold(
            from_peer,
            |mut rx| async move { rx.recv().await.map(|frame| (frame, rx)) },
        ));
        (
            Wire {
                sender: Box::new(MemorySender(to_peer)),
                receiver,
            },
            MemoryPeer { sent, inject },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};

    use super::memory::{self, MemoryPeer};
    use crate::browser::cdp_ws::CdpConnection;

    /// Reads the next frame the connection sent and returns its id and method.
    async fn next_request(peer: &mut MemoryPeer) -> (u64, String, Value) {
        let text = tokio::time::timeout(Duration::from_secs(5), peer.sent.recv())
            .await
            .expect("connection sent a frame")
            .expect("sender alive");
        let v: Value = serde_json::from_str(&text).unwrap();
        (
            v["id"].as_u64().unwrap(),
            v["method"].as_str().unwrap().to_owned(),
            v,
        )
    }

    #[tokio::test]
    async fn call_resolves_by_id() {
        let (wire, mut peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        let server = tokio::spawn(async move {
            let (id1, m1, _) = next_request(&mut peer).await;
            let (id2, m2, v2) = next_request(&mut peer).await;
            assert_eq!(v2["sessionId"], "S1");
            // Answer out of order: the second call first.
            peer.inject
                .send(json!({"id": id2, "result": {"m": m2}}).to_string())
                .unwrap();
            peer.inject
                .send(json!({"id": id1, "result": {"m": m1}}).to_string())
                .unwrap();
            peer
        });
        let (a, b) = tokio::join!(conn.call(None, "Target.getTargets", json!({})), async {
            // Make sure the first call is on the wire before the second.
            tokio::time::sleep(Duration::from_millis(20)).await;
            conn.call(Some("S1"), "Runtime.evaluate", json!({"expression": "1"}))
                .await
        });
        assert_eq!(a.unwrap()["m"], "Target.getTargets");
        assert_eq!(b.unwrap()["m"], "Runtime.evaluate");
        let _peer = server.await.unwrap();
        assert_eq!(conn.pending_len(), 0);
    }

    #[tokio::test]
    async fn cdp_error_keeps_code() {
        let (wire, mut peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        let server = tokio::spawn(async move {
            let (id, _, _) = next_request(&mut peer).await;
            peer.inject
                .send(json!({"id": id, "error": {"code": -32601, "message": "nope"}}).to_string())
                .unwrap();
            peer
        });
        let err = conn
            .call(None, "Target.setAutoAttach", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("(-32601)"), "{err}");
        let _peer = server.await.unwrap();
    }

    #[tokio::test]
    async fn events_before_reply_are_queued_when_call_returns() {
        let (wire, mut peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        let mut events = conn.subscribe();
        let server = tokio::spawn(async move {
            let (id, _, _) = next_request(&mut peer).await;
            for n in 0..3 {
                peer.inject
                    .send(
                        json!({
                            "method": "Target.attachedToTarget",
                            "sessionId": "P",
                            "params": {"n": n}
                        })
                        .to_string(),
                    )
                    .unwrap();
            }
            peer.inject
                .send(json!({"id": id, "result": {}}).to_string())
                .unwrap();
            peer
        });
        conn.call(Some("P"), "Target.setAutoAttach", json!({}))
            .await
            .unwrap();
        // No await between the call returning and draining: every event the
        // peer sent before the reply must already be queued, in order.
        for n in 0..3 {
            let event = events.try_recv().expect("event queued before reply");
            assert_eq!(event.method, "Target.attachedToTarget");
            assert_eq!(event.session_id.as_deref(), Some("P"));
            assert_eq!(event.params["n"], n);
        }
        assert!(events.try_recv().is_err());
        let _peer = server.await.unwrap();
    }

    #[tokio::test]
    async fn receive_end_closes_connection_and_fails_pending_calls() {
        let (wire, mut peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        assert!(!conn.is_closed());
        let server = tokio::spawn(async move {
            let _ = next_request(&mut peer).await;
            // Drop the peer (and its inject sender) without answering.
            drop(peer);
        });
        let err = conn
            .call(None, "Browser.getVersion", json!({}))
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();
        assert!(err.contains("socket closed"), "{err}");
        assert!(conn.is_closed());
        assert_eq!(conn.pending_len(), 0);
        let err = conn
            .call(None, "Browser.getVersion", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("socket closed"), "{err}");
    }

    #[tokio::test]
    async fn send_failure_clears_the_waiter() {
        let (wire, peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        let MemoryPeer { sent, inject } = peer;
        drop(sent);
        let err = conn
            .call(None, "Browser.getVersion", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("send failed"), "{err}");
        assert_eq!(conn.pending_len(), 0);
        // The receive side is still open: a failed send is not a close.
        assert!(!conn.is_closed());
        drop(inject);
    }

    #[tokio::test]
    async fn non_json_frames_are_skipped() {
        let (wire, mut peer) = memory::pair();
        let conn = CdpConnection::from_wire(wire);
        let server = tokio::spawn(async move {
            let (id, _, _) = next_request(&mut peer).await;
            peer.inject.send("not json".to_owned()).unwrap();
            peer.inject
                .send(json!({"id": id, "result": {"ok": true}}).to_string())
                .unwrap();
            peer
        });
        let v = conn
            .call(None, "Browser.getVersion", json!({}))
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        let _peer = server.await.unwrap();
    }
}
