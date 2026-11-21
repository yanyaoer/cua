// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Native-messaging frames and the bridge envelope.
//!
//! A frame is a 4-byte length in native byte order followed by that many
//! bytes of UTF-8 JSON. Chrome uses this framing on the host's stdin and
//! stdout; the driver socket uses the same framing so the host can forward
//! frames verbatim after the handshake.
//!
//! The envelope is a JSON object with `v` = 1 and a type `t`; `req` and
//! `res` carry a positive integer `id`. Envelope checks here are shape
//! checks only — what an operation means is decided by the endpoint that
//! owns it.

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame Chrome accepts from a native host (host → extension).
pub const MAX_TO_CHROME: usize = 1024 * 1024;
/// Largest frame Chrome sends to a native host (extension → host).
pub const MAX_FROM_CHROME: usize = 64 * 1024 * 1024;
/// The only envelope version this build speaks.
pub const ENVELOPE_VERSION: u64 = 1;
/// The only handshake protocol this build speaks (`hello.proto`).
pub const PROTOCOL_VERSION: u64 = 1;

/// Why a byte stream did not yield a frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The stream ended inside a frame (header or payload).
    #[error("incomplete frame: stream ended after {got} of {want} bytes")]
    Incomplete { got: usize, want: usize },
    /// The length prefix exceeds the limit for this direction.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("frame read failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Prefixes `payload` with its native-order length.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Serialises `value` compactly and frames it.
pub fn encode_json(value: &Value) -> Vec<u8> {
    encode(&serde_json::to_vec(value).expect("serialising a serde_json::Value cannot fail"))
}

/// Incremental decoder for callers that receive bytes in arbitrary chunks.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Returns the next complete payload, `Ok(None)` when more bytes are
    /// needed, or `TooLarge` as soon as the length prefix is known.
    pub fn next_frame(&mut self, max: usize) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_ne_bytes(self.buffer[..4].try_into().expect("4 bytes")) as usize;
        if len > max {
            return Err(FrameError::TooLarge { len, max });
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buffer[4..4 + len].to_vec();
        self.buffer.drain(..4 + len);
        Ok(Some(payload))
    }

    /// Call at end of stream: leftover bytes are an incomplete frame, which
    /// at EOF is a read error.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let want = if self.buffer.len() < 4 {
            4
        } else {
            4 + u32::from_ne_bytes(self.buffer[..4].try_into().expect("4 bytes")) as usize
        };
        Err(FrameError::Incomplete {
            got: self.buffer.len(),
            want,
        })
    }
}

/// Reads one frame. `Ok(None)` is a clean end of stream at a frame boundary;
/// end of stream anywhere else is `Incomplete`.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0_u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = reader.read(&mut header[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(FrameError::Incomplete { got, want: 4 });
        }
        got += n;
    }
    let len = u32::from_ne_bytes(header) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut payload = vec![0_u8; len];
    let mut got = 0;
    while got < len {
        let n = reader.read(&mut payload[got..]).await?;
        if n == 0 {
            return Err(FrameError::Incomplete {
                got: 4 + got,
                want: 4 + len,
            });
        }
        got += n;
    }
    Ok(Some(payload))
}

/// Writes one frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    writer.write_all(&encode(payload)).await?;
    writer.flush().await
}

/// Envelope types (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Hello,
    Welcome,
    Bye,
    Req,
    Res,
    Evt,
    HostHello,
    DriverHello,
}

impl FrameType {
    pub fn parse(t: &str) -> Option<Self> {
        Some(match t {
            "hello" => Self::Hello,
            "welcome" => Self::Welcome,
            "bye" => Self::Bye,
            "req" => Self::Req,
            "res" => Self::Res,
            "evt" => Self::Evt,
            "host_hello" => Self::HostHello,
            "driver_hello" => Self::DriverHello,
            _ => return None,
        })
    }
}

/// A frame whose envelope passed the shape checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub t: FrameType,
    /// Present exactly for `req` and `res`.
    pub id: Option<u64>,
    pub value: Value,
}

/// Every envelope rejection maps to the `bad_request` error code (spec §12).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("frame is not a JSON object")]
    NotObject,
    #[error("envelope version is not 1")]
    WrongVersion,
    #[error("unknown frame type")]
    UnknownType,
    #[error("req/res frame lacks a positive integer id")]
    MissingId,
}

impl EnvelopeError {
    /// The spec §12 error code for this rejection.
    pub fn code(&self) -> &'static str {
        "bad_request"
    }
}

/// Parses and shape-checks one payload.
pub fn parse_envelope(payload: &[u8]) -> Result<Envelope, EnvelopeError> {
    let value: Value = serde_json::from_slice(payload).map_err(|_| EnvelopeError::NotObject)?;
    let object = value.as_object().ok_or(EnvelopeError::NotObject)?;
    if object.get("v").and_then(Value::as_u64) != Some(ENVELOPE_VERSION) {
        return Err(EnvelopeError::WrongVersion);
    }
    let t = object
        .get("t")
        .and_then(Value::as_str)
        .and_then(FrameType::parse)
        .ok_or(EnvelopeError::UnknownType)?;
    let id = match t {
        FrameType::Req | FrameType::Res => {
            let id = object
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
                .ok_or(EnvelopeError::MissingId)?;
            Some(id)
        }
        _ => None,
    };
    Ok(Envelope { t, id, value })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// sha256 of `browser-extension/test/fixtures/frames.json` in the
    /// spec's repository (spec appendix A). Both sides test against the
    /// same file; a different digest means the copies drifted.
    pub(crate) const FIXTURE_SHA256: &str =
        "489ae5a9aeb41461496b1664806bd4dcd8e7849bc14174e1c16b02ad17778428";
    pub(crate) const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/ext_bridge/frames.json"
    ));

    fn fixture() -> Value {
        serde_json::from_slice(FIXTURE).unwrap()
    }

    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    fn invalid(name: &str) -> Value {
        fixture()["invalid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("fixture case {name}"))
            .clone()
    }

    #[test]
    fn fixture_matches_the_spec_copy() {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(FIXTURE);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, FIXTURE_SHA256);
        assert_eq!(fixture()["version"], 1);
    }

    #[test]
    fn valid_fixtures_decode_and_round_trip() {
        let fixture = fixture();
        let valid = fixture["valid"].as_array().unwrap();
        assert_eq!(valid.len(), 12);
        for case in valid {
            let name = case["name"].as_str().unwrap();
            let bytes = unhex(case["hex"].as_str().unwrap());
            let mut decoder = FrameDecoder::default();
            decoder.push(&bytes);
            let payload = decoder.next_frame(MAX_FROM_CHROME).unwrap().expect(name);
            decoder.finish().unwrap();
            let decoded: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(decoded, case["json"], "{name}: decode");
            let envelope = parse_envelope(&payload).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(envelope.value, case["json"], "{name}");

            let encoded = encode_json(&case["json"]);
            let length = u32::from_ne_bytes(encoded[..4].try_into().unwrap()) as u64;
            assert_eq!(
                Some(length),
                case["length"].as_u64(),
                "{name}: length prefix"
            );
            let mut decoder = FrameDecoder::default();
            decoder.push(&encoded);
            let again: Value =
                serde_json::from_slice(&decoder.next_frame(MAX_TO_CHROME).unwrap().unwrap())
                    .unwrap();
            assert_eq!(again, case["json"], "{name}: encode round trip");
        }
    }

    #[test]
    fn invalid_envelopes_are_bad_request() {
        for (name, want) in [
            ("req_missing_id", EnvelopeError::MissingId),
            ("unknown_type", EnvelopeError::UnknownType),
            ("wrong_envelope_version", EnvelopeError::WrongVersion),
        ] {
            let case = invalid(name);
            let payload = serde_json::to_vec(&case["json"]).unwrap();
            let error = parse_envelope(&payload).unwrap_err();
            assert_eq!(error, want, "{name}");
            assert_eq!(error.code(), "bad_request", "{name}");
        }
        assert_eq!(
            parse_envelope(b"[1]").unwrap_err(),
            EnvelopeError::NotObject
        );
        assert_eq!(
            parse_envelope(br#"{"v":1,"t":"res","id":0}"#).unwrap_err(),
            EnvelopeError::MissingId
        );
    }

    #[test]
    fn proto_mismatch_hello_is_a_well_formed_envelope() {
        // The envelope is fine; rejecting proto 2 is the host's handshake
        // decision (bye proto_mismatch), covered by the host tests.
        let case = invalid("proto_mismatch");
        let envelope = parse_envelope(&serde_json::to_vec(&case["json"]).unwrap()).unwrap();
        assert_eq!(envelope.t, FrameType::Hello);
        assert_eq!(envelope.value["proto"], 2);
    }

    #[test]
    fn truncated_frame_waits_then_is_a_read_error_at_eof() {
        let case = invalid("truncated");
        let bytes = unhex(case["hex"].as_str().unwrap());
        let mut decoder = FrameDecoder::default();
        decoder.push(&bytes);
        assert!(decoder.next_frame(MAX_FROM_CHROME).unwrap().is_none());
        assert!(matches!(
            decoder.finish(),
            Err(FrameError::Incomplete { got: 10, want: 14 })
        ));
    }

    #[tokio::test]
    async fn async_reader_reports_truncation_and_clean_eof() {
        let bytes = unhex(invalid("truncated")["hex"].as_str().unwrap());
        let mut reader = &bytes[..];
        assert!(matches!(
            read_frame(&mut reader, MAX_FROM_CHROME).await,
            Err(FrameError::Incomplete { got: 10, want: 14 })
        ));
        let mut empty: &[u8] = &[];
        assert!(read_frame(&mut empty, MAX_FROM_CHROME)
            .await
            .unwrap()
            .is_none());
        let mut half_header: &[u8] = &[1, 0];
        assert!(matches!(
            read_frame(&mut half_header, MAX_FROM_CHROME).await,
            Err(FrameError::Incomplete { got: 2, want: 4 })
        ));
    }

    #[tokio::test]
    async fn length_limits_are_inclusive() {
        let oversize = invalid("oversize_to_chrome")["length"].as_u64().unwrap() as usize;
        assert_eq!(oversize, MAX_TO_CHROME + 1);
        for (len, max, ok) in [
            (0, MAX_TO_CHROME, true),
            (MAX_TO_CHROME, MAX_TO_CHROME, true),
            (oversize, MAX_TO_CHROME, false),
            (MAX_FROM_CHROME, MAX_FROM_CHROME, true),
            (MAX_FROM_CHROME + 1, MAX_FROM_CHROME, false),
        ] {
            // Only the header matters for the limit check.
            let header = (len as u32).to_ne_bytes();
            let mut decoder = FrameDecoder::default();
            decoder.push(&header);
            let result = decoder.next_frame(max);
            assert_eq!(result.is_ok(), ok, "len {len} max {max}");
            if ok && len > 0 {
                let mut reader = &header[..];
                assert!(matches!(
                    read_frame(&mut reader, max).await,
                    Err(FrameError::Incomplete { .. })
                ));
            }
        }
        let mut zero: &[u8] = &[0, 0, 0, 0];
        assert_eq!(
            read_frame(&mut zero, MAX_TO_CHROME).await.unwrap(),
            Some(vec![])
        );
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let value = json!({"v": 1, "t": "evt", "ev": "tab.closed", "tab": 3});
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();
        let mut reader = &buffer[..];
        let payload = read_frame(&mut reader, MAX_TO_CHROME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&payload).unwrap(), value);
    }
}
