//! The agent side of the oxproto handshake (`docs/design/OXPROTO.md` §7).
//!
//! Deliberately platform-independent so it is unit-tested on the Linux host, where CI runs —
//! the Windows-only capture code cannot be. It is also the security-critical seam: **no other
//! message type is processed until authentication passes**, and a rejected peer costs one
//! `Error` + `Close` and no per-session state.
//!
//! Token comparison is injected rather than hardcoded so this module stays free of crypto
//! dependencies; `serve` supplies the constant-time comparison.

use std::io;

use oxproto::envelope::{channel, Reassembler};
use oxproto::message::{Close, DisplayLayout, Error as ProtoError, Message, ServerHello};
use oxproto::{codec, error_code, feature, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION};
use oxtransport::{read_message, write_message};
use tokio::io::{AsyncRead, AsyncWrite};

/// What the two peers agreed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// Protocol version in use.
    pub version: u16,
    /// Features both sides advertised.
    pub features: u64,
    /// Codec chosen for this session.
    pub codec: u8,
    /// Session id the agent assigned.
    pub session_id: u64,
    /// Name the client reported, for logs.
    pub client_name: String,
    /// The client's output topology.
    pub display: DisplayLayout,
}

/// Why a handshake did not complete.
#[derive(Debug)]
pub enum HandshakeError {
    /// The transport failed.
    Io(io::Error),
    /// The peer was rejected; the agent already sent `Error` + `Close`.
    Rejected {
        /// The `error_code` sent to the peer.
        code: u16,
        /// Human-readable reason, for the agent's own log.
        reason: &'static str,
    },
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Io(e) => write!(f, "handshake transport error: {e}"),
            HandshakeError::Rejected { code, reason } => {
                write!(f, "handshake rejected (code {code}): {reason}")
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

/// Codecs this agent can actually produce, in descending preference.
///
/// `RAW_BGRA` only, until the Media Foundation encoder lands.
pub const SUPPORTED_CODECS: &[u8] = &[codec::RAW_BGRA];

/// Run the agent side of the handshake.
///
/// `verify_token` receives the token the client presented and must compare it against the
/// agent's expected value in constant time. `session_id` is assigned by the caller.
///
/// On rejection the peer is told why (`Error` + `Close`) and the connection should be dropped.
pub async fn negotiate<S, F>(
    stream: &mut S,
    reassembler: &mut Reassembler,
    session_id: u64,
    verify_token: F,
) -> Result<Negotiated, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(&str) -> bool,
{
    // Exactly one message is accepted before authentication.
    let hello = match read_message(stream, reassembler).await? {
        Some(Message::ClientHello(h)) => h,
        // An unknown type here is not forward compatibility, it is a peer skipping the
        // handshake.
        _ => {
            return Err(reject(
                stream,
                error_code::PROTOCOL,
                "expected ClientHello",
                "first message was not a ClientHello",
            )
            .await)
        }
    };

    if !verify_token(&hello.auth_token) {
        return Err(reject(
            stream,
            error_code::AUTH_FAILED,
            "authentication failed",
            "token mismatch",
        )
        .await);
    }

    // Highest version both sides support.
    let version = PROTOCOL_VERSION.min(hello.version_max);
    if version < MIN_SUPPORTED_VERSION || version < hello.version_min {
        return Err(reject(
            stream,
            error_code::VERSION_MISMATCH,
            "no mutually supported protocol version",
            "version ranges do not overlap",
        )
        .await);
    }

    // First codec the client offered that the agent can produce; the client's list is in
    // descending preference, so this honours its preference rather than ours.
    let Some(&chosen) = hello.codecs.iter().find(|c| SUPPORTED_CODECS.contains(c)) else {
        return Err(reject(
            stream,
            error_code::UNSUPPORTED_CODEC,
            "no mutually supported codec",
            "client offered no codec this agent can produce",
        )
        .await);
    };

    let features = hello.features & feature::SUPPORTED;

    write_message(
        stream,
        &Message::ServerHello(ServerHello {
            version,
            features,
            session_id,
            codec: chosen,
        }),
        channel::CONTROL,
    )
    .await?;

    Ok(Negotiated {
        version,
        features,
        codec: chosen,
        session_id,
        client_name: hello.client_name,
        display: hello.display,
    })
}

/// Tell the peer why it was refused, then report the rejection.
///
/// Errors while sending the rejection are deliberately ignored: the peer may already be gone,
/// and the local outcome is the same either way.
async fn reject<S>(stream: &mut S, code: u16, message: &str, reason: &'static str) -> HandshakeError
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = write_message(
        stream,
        &Message::Error(ProtoError {
            code,
            message: message.to_string(),
        }),
        channel::CONTROL,
    )
    .await;
    let _ = write_message(
        stream,
        &Message::Close(Close { reason: 3 }),
        channel::CONTROL,
    )
    .await;
    HandshakeError::Rejected { code, reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::message::{ClientHello, Output};

    fn hello(token: &str, codecs: Vec<u8>, vmin: u16, vmax: u16) -> Message {
        Message::ClientHello(ClientHello {
            version_min: vmin,
            version_max: vmax,
            features: feature::CURSOR_STREAM | feature::FRAME_ACK | feature::AUDIO,
            auth_token: token.to_string(),
            client_name: "test-client".into(),
            codecs,
            display: DisplayLayout {
                outputs: vec![Output {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 720,
                    scale_num: 1,
                    scale_den: 1,
                    refresh_mhz: 60_000,
                }],
            },
        })
    }

    /// Drive the agent side against a scripted client, returning the agent's result and every
    /// message the client received.
    async fn run(
        client_hello: Message,
        expected: &'static str,
    ) -> (Result<Negotiated, HandshakeError>, Vec<Message>) {
        let (mut client_io, mut agent_io) = tokio::io::duplex(64 * 1024);

        let client = tokio::spawn(async move {
            let mut r = Reassembler::new();
            write_message(&mut client_io, &client_hello, channel::CONTROL)
                .await
                .unwrap();
            let mut seen = Vec::new();
            // Collect whatever the agent says until it stops talking.
            while let Ok(Some(msg)) = read_message(&mut client_io, &mut r).await {
                seen.push(msg);
            }
            seen
        });

        let mut r = Reassembler::new();
        let result = negotiate(&mut agent_io, &mut r, 7, |t| t == expected).await;
        drop(agent_io);
        (result, client.await.unwrap())
    }

    #[tokio::test]
    async fn accepts_a_valid_client() {
        let (result, seen) = run(hello("secret", vec![codec::RAW_BGRA], 1, 1), "secret").await;
        let negotiated = result.expect("handshake should succeed");

        assert_eq!(negotiated.version, PROTOCOL_VERSION);
        assert_eq!(negotiated.codec, codec::RAW_BGRA);
        assert_eq!(negotiated.session_id, 7);
        assert_eq!(negotiated.client_name, "test-client");
        assert_eq!(negotiated.display.outputs.len(), 1);
        // Features are intersected: the client asked for AUDIO, which this build does not have.
        assert!(negotiated.features & feature::FRAME_ACK != 0);
        assert!(negotiated.features & feature::AUDIO == 0);

        assert!(matches!(seen.as_slice(), [Message::ServerHello(_)]));
    }

    #[tokio::test]
    async fn rejects_a_bad_token_and_says_why() {
        let (result, seen) = run(hello("wrong", vec![codec::RAW_BGRA], 1, 1), "secret").await;
        assert!(matches!(
            result,
            Err(HandshakeError::Rejected {
                code: error_code::AUTH_FAILED,
                ..
            })
        ));
        // The peer is told, then closed — and nothing else is sent.
        assert!(matches!(
            seen.as_slice(),
            [Message::Error(e), Message::Close(_)] if e.code == error_code::AUTH_FAILED
        ));
    }

    #[tokio::test]
    async fn rejects_an_unsupported_codec() {
        let (result, _) = run(hello("secret", vec![codec::AV1], 1, 1), "secret").await;
        assert!(matches!(
            result,
            Err(HandshakeError::Rejected {
                code: error_code::UNSUPPORTED_CODEC,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejects_a_version_range_that_does_not_overlap() {
        let (result, _) = run(hello("secret", vec![codec::RAW_BGRA], 99, 100), "secret").await;
        assert!(matches!(
            result,
            Err(HandshakeError::Rejected {
                code: error_code::VERSION_MISMATCH,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejects_a_peer_that_skips_the_handshake() {
        let ping = Message::Ping(oxproto::message::Ping { seq: 1, sent_us: 0 });
        let (result, _) = run(ping, "secret").await;
        assert!(matches!(
            result,
            Err(HandshakeError::Rejected {
                code: error_code::PROTOCOL,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn honours_the_client_codec_preference_order() {
        // The client prefers AV1, which the agent cannot produce, then RAW_BGRA which it can.
        let (result, _) = run(
            hello("secret", vec![codec::AV1, codec::RAW_BGRA], 1, 1),
            "secret",
        )
        .await;
        assert_eq!(result.unwrap().codec, codec::RAW_BGRA);
    }
}
