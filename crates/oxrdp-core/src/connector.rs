use oxrdp_pdu::connect::{ConnectionConfirm, ConnectionRequest, NegotiationConfirm};
use oxrdp_pdu::nego::{self, NegotiationRequest};
use oxrdp_pdu::tpkt::{TpktHeader, TPKT_HEADER_LEN};
use oxrdp_pdu::{decode, encode_vec, DecodeError, EncodeError};

/// What the connector needs the IO layer to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreOutput {
    /// Write these bytes to the server.
    SendData(Vec<u8>),
    /// Negotiation selected a TLS-based protocol: upgrade the transport to TLS now.
    UpgradeToTls,
    /// The negotiation phase completed.
    Connected { selected_protocol: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    Initial,
    SentConnectionRequest,
    Connected,
}

#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    /// `mstshash` cookie identifier (routing token). None = no cookie.
    pub cookie: Option<String>,
    /// Security protocols to request (bitmask of `nego::protocol::*`).
    pub requested_protocols: u32,
}

#[derive(Debug)]
pub enum CoreError {
    Decode(DecodeError),
    Encode(EncodeError),
    /// Called step()/start() in a state that does not expect it.
    UnexpectedState,
    /// Server rejected the negotiation with this failure code.
    NegotiationFailed(u32),
}

pub type CoreResult<T> = Result<T, CoreError>;

impl From<DecodeError> for CoreError {
    fn from(err: DecodeError) -> Self {
        CoreError::Decode(err)
    }
}

impl From<EncodeError> for CoreError {
    fn from(err: EncodeError) -> Self {
        CoreError::Encode(err)
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Decode(e) => write!(f, "decode error: {e}"),
            CoreError::Encode(e) => write!(f, "encode error: {e}"),
            CoreError::UnexpectedState => write!(f, "unexpected connector state"),
            CoreError::NegotiationFailed(code) => write!(f, "negotiation failed with code {code}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CoreError::Decode(e) => Some(e),
            CoreError::Encode(e) => Some(e),
            CoreError::UnexpectedState | CoreError::NegotiationFailed(_) => None,
        }
    }
}

pub struct ClientConnector {
    state: ConnectorState,
    config: ConnectorConfig,
}

impl ClientConnector {
    pub fn new(config: ConnectorConfig) -> Self {
        Self {
            state: ConnectorState::Initial,
            config,
        }
    }

    pub fn state(&self) -> ConnectorState {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        self.state == ConnectorState::Connected
    }

    /// Begin the connection sequence. Returns the X.224 Connection Request (wrapped in TPKT)
    /// as a `SendData` output, and moves to `SentConnectionRequest`.
    /// Error `UnexpectedState` if not in `Initial`.
    pub fn start(&mut self) -> CoreResult<Vec<CoreOutput>> {
        if self.state != ConnectorState::Initial {
            return Err(CoreError::UnexpectedState);
        }

        let cr = ConnectionRequest {
            cookie: self.config.cookie.clone(),
            negotiation: Some(NegotiationRequest {
                flags: 0,
                requested_protocols: self.config.requested_protocols,
            }),
        };

        let x224 = encode_vec(&cr)?;
        let tpkt = encode_vec(&TpktHeader::new((TPKT_HEADER_LEN + x224.len()) as u16))?;
        let mut frame = tpkt;
        frame.extend_from_slice(&x224);

        self.state = ConnectorState::SentConnectionRequest;
        Ok(vec![CoreOutput::SendData(frame)])
    }

    /// Feed ONE complete TPKT frame received from the server. In `SentConnectionRequest`,
    /// parses the X.224 Connection Confirm and produces the next outputs.
    /// Error `UnexpectedState` if not in `SentConnectionRequest`.
    pub fn step(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        if self.state != ConnectorState::SentConnectionRequest {
            return Err(CoreError::UnexpectedState);
        }

        let _tpkt: TpktHeader = decode(&frame[..TPKT_HEADER_LEN])?;
        let cc: ConnectionConfirm = decode(&frame[TPKT_HEADER_LEN..])?;

        match cc.negotiation {
            Some(NegotiationConfirm::Failure(f)) => {
                Err(CoreError::NegotiationFailed(f.failure_code))
            }
            Some(NegotiationConfirm::Response(r)) => {
                let proto = r.selected_protocol;
                self.state = ConnectorState::Connected;
                if proto != nego::protocol::RDP {
                    Ok(vec![
                        CoreOutput::UpgradeToTls,
                        CoreOutput::Connected {
                            selected_protocol: proto,
                        },
                    ])
                } else {
                    Ok(vec![CoreOutput::Connected {
                        selected_protocol: proto,
                    }])
                }
            }
            None => {
                self.state = ConnectorState::Connected;
                Ok(vec![CoreOutput::Connected {
                    selected_protocol: nego::protocol::RDP,
                }])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::nego::protocol;

    #[test]
    fn start_emits_tpkt_wrapped_cr() {
        let mut c = ClientConnector::new(ConnectorConfig {
            cookie: None,
            requested_protocols: protocol::SSL,
        });
        let out = c.start().unwrap();
        // TPKT(03 00 00 13) + X.224 CR(0E E0 00 00 00 00 00) + NEG_REQ SSL(01 00 08 00 01 00 00 00)
        let expected = vec![
            0x03, 0x00, 0x00, 0x13, 0x0E, 0xE0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x08, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(out, vec![CoreOutput::SendData(expected)]);
        assert_eq!(c.state(), ConnectorState::SentConnectionRequest);
    }

    #[test]
    fn step_tls_selected_upgrades_and_connects() {
        let mut c = ClientConnector::new(ConnectorConfig {
            cookie: None,
            requested_protocols: protocol::SSL,
        });
        c.start().unwrap();
        // TPKT + X.224 CC + NEG_RSP selecting SSL
        let cc = vec![
            0x03, 0x00, 0x00, 0x13, 0x0E, 0xD0, 0, 0, 0, 0, 0, 0x02, 0x00, 0x08, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        let out = c.step(&cc).unwrap();
        assert_eq!(
            out,
            vec![
                CoreOutput::UpgradeToTls,
                CoreOutput::Connected {
                    selected_protocol: protocol::SSL
                }
            ]
        );
        assert!(c.is_connected());
    }

    #[test]
    fn step_failure_errors() {
        let mut c = ClientConnector::new(ConnectorConfig {
            cookie: None,
            requested_protocols: protocol::SSL,
        });
        c.start().unwrap();
        let cc = vec![
            0x03, 0x00, 0x00, 0x13, 0x0E, 0xD0, 0, 0, 0, 0, 0, 0x03, 0x00, 0x08, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        assert!(matches!(c.step(&cc), Err(CoreError::NegotiationFailed(1))));
    }

    #[test]
    fn step_before_start_is_unexpected() {
        let mut c = ClientConnector::new(ConnectorConfig {
            cookie: None,
            requested_protocols: protocol::SSL,
        });
        assert!(matches!(
            c.step(&[0x03, 0x00, 0x00, 0x04]),
            Err(CoreError::UnexpectedState)
        ));
    }
}
