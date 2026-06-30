use oxrdp_pdu::connect::{ConnectionConfirm, ConnectionRequest, NegotiationConfirm};
use oxrdp_pdu::connect_initial::ConnectInitial;
use oxrdp_pdu::connect_response::ConnectResponse;
use oxrdp_pdu::frame::{mcs_payload, wrap_mcs};
use oxrdp_pdu::gcc::{ClientCoreData, ClientNetworkData, ClientSecurityData};
use oxrdp_pdu::mcs::{
    AttachUserConfirm, AttachUserRequest, ChannelJoinConfirm, ChannelJoinRequest,
    ErectDomainRequest,
};
use oxrdp_pdu::nego::{self, NegotiationRequest};
use oxrdp_pdu::tpkt::{TpktHeader, TPKT_HEADER_LEN};
use oxrdp_pdu::{decode, encode_vec, DecodeError, EncodeError};

/// Action the caller must perform on behalf of the connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreOutput {
    /// Send this complete, framed byte buffer to the server.
    SendData(Vec<u8>),
    /// Upgrade the transport to TLS and then call `resume_after_tls`.
    UpgradeToTls,
    /// The MCS channel join phase is complete.
    Connected {
        /// Protocol selected during negotiation.
        selected_protocol: u32,
        /// MCS user channel id assigned by the server.
        user_channel: u16,
        /// MCS I/O channel id.
        io_channel: u16,
    },
}

/// Current state of the RDP connection sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    /// No data has been sent yet.
    Initial,
    /// The X.224 Connection Request has been sent.
    SentConnectionRequest,
    /// TLS upgrade has been requested.
    NeedTls,
    /// The MCS Connect Initial has been sent.
    SentConnectInitial,
    /// The Erect Domain Request and Attach User Request have been sent.
    SentErectAttach,
    /// Channel join requests are in progress.
    JoiningChannels,
    /// The connection sequence is complete.
    Connected,
}

/// Configuration for the RDP connector.
#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    /// Optional routing token / cookie.
    pub cookie: Option<String>,
    /// Protocols to request during negotiation.
    pub requested_protocols: u32,
    /// GCC client core data.
    pub core: ClientCoreData,
    /// GCC client security data.
    pub security: ClientSecurityData,
    /// GCC client network data.
    pub network: ClientNetworkData,
}

/// Error type for the connector state machine.
#[derive(Debug)]
pub enum CoreError {
    /// Decoding a received PDU failed.
    Decode(DecodeError),
    /// Encoding an outbound PDU failed.
    Encode(EncodeError),
    /// The operation is not valid in the current state.
    UnexpectedState,
    /// The server refused the requested protocol.
    NegotiationFailed(u32),
    /// The MCS Connect-Response indicated failure.
    ConnectResponseFailed(u8),
    /// A channel join was rejected by the server.
    ChannelJoinFailed(u16),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Decode(e) => write!(f, "decode error: {e}"),
            CoreError::Encode(e) => write!(f, "encode error: {e}"),
            CoreError::UnexpectedState => write!(f, "unexpected connector state"),
            CoreError::NegotiationFailed(c) => write!(f, "negotiation failed: {c}"),
            CoreError::ConnectResponseFailed(r) => write!(f, "connect response failed: {r}"),
            CoreError::ChannelJoinFailed(c) => write!(f, "channel join failed: {c}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CoreError::Decode(e) => Some(e),
            CoreError::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<DecodeError> for CoreError {
    fn from(e: DecodeError) -> Self {
        CoreError::Decode(e)
    }
}

impl From<EncodeError> for CoreError {
    fn from(e: EncodeError) -> Self {
        CoreError::Encode(e)
    }
}

/// Result type for connector operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// SANS-IO RDP connector driving the connection sequence up to the end of MCS channel join.
pub struct ClientConnector {
    config: ConnectorConfig,
    state: ConnectorState,
    selected_protocol: u32,
    user_id: u16,
    io_channel: u16,
    join_queue: Vec<u16>,
    join_index: usize,
}

impl ClientConnector {
    /// Create a new connector in the initial state.
    pub fn new(config: ConnectorConfig) -> Self {
        Self {
            config,
            state: ConnectorState::Initial,
            selected_protocol: 0,
            user_id: 0,
            io_channel: 0,
            join_queue: Vec::new(),
            join_index: 0,
        }
    }

    /// Return the current connector state.
    pub fn state(&self) -> ConnectorState {
        self.state
    }

    /// Return true if the connector has reached the connected state.
    pub fn is_connected(&self) -> bool {
        self.state == ConnectorState::Connected
    }

    /// Start the connection sequence by sending the X.224 Connection Request.
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
        let mut frame = encode_vec(&TpktHeader::new((TPKT_HEADER_LEN + x224.len()) as u16))?;
        frame.extend_from_slice(&x224);

        self.state = ConnectorState::SentConnectionRequest;
        Ok(vec![CoreOutput::SendData(frame)])
    }

    /// Resume after the transport has been upgraded to TLS.
    pub fn resume_after_tls(&mut self) -> CoreResult<Vec<CoreOutput>> {
        if self.state != ConnectorState::NeedTls {
            return Err(CoreError::UnexpectedState);
        }

        self.send_connect_initial()
    }

    /// Feed a server frame into the state machine and return the next actions.
    pub fn step(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        match self.state {
            ConnectorState::SentConnectionRequest => self.handle_connection_confirm(frame),
            ConnectorState::SentConnectInitial => self.handle_connect_response(frame),
            ConnectorState::SentErectAttach => self.handle_attach_user_confirm(frame),
            ConnectorState::JoiningChannels => self.handle_channel_join_confirm(frame),
            _ => Err(CoreError::UnexpectedState),
        }
    }

    fn handle_connection_confirm(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        let _tpkt = decode::<TpktHeader>(frame)?;
        let cc = decode::<ConnectionConfirm>(&frame[TPKT_HEADER_LEN..])?;

        match cc.negotiation {
            Some(NegotiationConfirm::Failure(f)) => {
                Err(CoreError::NegotiationFailed(f.failure_code))
            }
            Some(NegotiationConfirm::Response(r)) => {
                self.selected_protocol = r.selected_protocol;
                if r.selected_protocol != nego::protocol::RDP {
                    self.state = ConnectorState::NeedTls;
                    Ok(vec![CoreOutput::UpgradeToTls])
                } else {
                    self.send_connect_initial()
                }
            }
            None => self.send_connect_initial(),
        }
    }

    fn handle_connect_response(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        let payload = mcs_payload(frame)?;
        let resp = ConnectResponse::from_bytes(payload)?;

        if resp.result != 0 {
            return Err(CoreError::ConnectResponseFailed(resp.result));
        }

        self.io_channel = resp
            .server_network
            .as_ref()
            .map(|n| n.mcs_channel_id)
            .unwrap_or(0);
        let virtual_ids: Vec<u16> = resp
            .server_network
            .as_ref()
            .map(|n| n.channel_ids.clone())
            .unwrap_or_default();
        self.join_queue = virtual_ids;

        let erect = wrap_mcs(&encode_vec(&ErectDomainRequest)?)?;
        let attach = wrap_mcs(&encode_vec(&AttachUserRequest)?)?;

        self.state = ConnectorState::SentErectAttach;
        Ok(vec![
            CoreOutput::SendData(erect),
            CoreOutput::SendData(attach),
        ])
    }

    fn handle_attach_user_confirm(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        let payload = mcs_payload(frame)?;
        let confirm = decode::<AttachUserConfirm>(payload)?;

        if confirm.result != 0 {
            return Err(CoreError::ConnectResponseFailed(confirm.result));
        }

        self.user_id = confirm.user_id;

        let mut q = vec![self.user_id, self.io_channel];
        q.extend_from_slice(&self.join_queue);
        self.join_queue = q;
        self.join_index = 0;

        self.send_channel_join()
    }

    fn handle_channel_join_confirm(&mut self, frame: &[u8]) -> CoreResult<Vec<CoreOutput>> {
        let payload = mcs_payload(frame)?;
        let confirm = decode::<ChannelJoinConfirm>(payload)?;

        if confirm.result != 0 {
            return Err(CoreError::ChannelJoinFailed(confirm.channel_id));
        }

        self.join_index += 1;

        if self.join_index < self.join_queue.len() {
            self.send_channel_join()
        } else {
            self.state = ConnectorState::Connected;
            Ok(vec![CoreOutput::Connected {
                selected_protocol: self.selected_protocol,
                user_channel: self.user_id,
                io_channel: self.io_channel,
            }])
        }
    }

    fn send_connect_initial(&mut self) -> CoreResult<Vec<CoreOutput>> {
        let ci = ConnectInitial {
            core: self.config.core,
            security: self.config.security,
            network: self.config.network.clone(),
        };
        let frame = wrap_mcs(&ci.to_bytes()?)?;
        self.state = ConnectorState::SentConnectInitial;
        Ok(vec![CoreOutput::SendData(frame)])
    }

    fn send_channel_join(&mut self) -> CoreResult<Vec<CoreOutput>> {
        let channel_id = self.join_queue[self.join_index];
        let req = ChannelJoinRequest {
            user_id: self.user_id,
            channel_id,
        };
        let frame = wrap_mcs(&encode_vec(&req)?)?;
        self.state = ConnectorState::JoiningChannels;
        Ok(vec![CoreOutput::SendData(frame)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::gcc::ChannelDef;
    use oxrdp_pdu::mcs::{AttachUserConfirm, ChannelJoinConfirm};
    use oxrdp_pdu::nego::protocol;

    fn cfg() -> ConnectorConfig {
        let mut name = [0u8; 32];
        name[..5].copy_from_slice(b"oxrdp");
        ConnectorConfig {
            cookie: None,
            requested_protocols: protocol::SSL,
            core: ClientCoreData {
                version: 0x0008_0004,
                desktop_width: 1024,
                desktop_height: 768,
                color_depth: 0xCA01,
                sas_sequence: 0xAA03,
                keyboard_layout: 0x0409,
                client_build: 2600,
                client_name: name,
                keyboard_type: 4,
                keyboard_subtype: 0,
                keyboard_function_key: 12,
                ime_file_name: [0u8; 64],
            },
            security: ClientSecurityData {
                encryption_methods: 0,
                ext_encryption_methods: 0,
            },
            network: ClientNetworkData {
                channels: vec![ChannelDef {
                    name: *b"rdpdr\0\0\0",
                    options: 0,
                }],
            },
        }
    }

    // server -> client frames
    fn cc_tls() -> Vec<u8> {
        vec![
            0x03, 0x00, 0x00, 0x13, 0x0E, 0xD0, 0, 0, 0, 0, 0, 0x02, 0x00, 0x08, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ]
    }

    fn connect_response_frame() -> Vec<u8> {
        // build a Connect-Response with SC_NET: io channel 1003, no virtual channels
        let net = oxrdp_pdu::gcc_server::ServerNetworkData {
            mcs_channel_id: 1003,
            channel_ids: vec![],
        };
        let blocks = encode_vec(&net).unwrap();
        let mut gcc = Vec::new();
        gcc.extend_from_slice(&[0x00, 0x05, 0x00, 0x14, 0x7C, 0x00, 0x01]);
        per_len(&mut gcc, blocks.len() + 14);
        gcc.extend_from_slice(&[0x2A, 0x14, 0x76, 0x0A, 0x01, 0x01, 0x00, 0x01, 0xC0, 0x00]);
        gcc.extend_from_slice(b"McDn");
        per_len(&mut gcc, blocks.len());
        gcc.extend_from_slice(&blocks);
        let mut body = Vec::new();
        body.extend_from_slice(&[0x0A, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x00, 0x04]);
        ber_len(&mut body, gcc.len());
        body.extend_from_slice(&gcc);
        let mut out = Vec::new();
        out.extend_from_slice(&[0x7F, 0x66]);
        ber_len(&mut out, body.len());
        out.extend_from_slice(&body);
        wrap_mcs(&out).unwrap()
    }

    fn attach_confirm() -> Vec<u8> {
        wrap_mcs(
            &encode_vec(&AttachUserConfirm {
                result: 0,
                user_id: 1007,
            })
            .unwrap(),
        )
        .unwrap()
    }

    fn join_confirm(ch: u16) -> Vec<u8> {
        wrap_mcs(
            &encode_vec(&ChannelJoinConfirm {
                result: 0,
                user_id: 1007,
                requested_channel_id: ch,
                channel_id: ch,
            })
            .unwrap(),
        )
        .unwrap()
    }

    fn per_len(o: &mut Vec<u8>, l: usize) {
        if l < 0x80 {
            o.push(l as u8);
        } else {
            o.push(0x80 | (l >> 8) as u8);
            o.push((l & 0xFF) as u8);
        }
    }

    fn ber_len(o: &mut Vec<u8>, l: usize) {
        if l < 0x80 {
            o.push(l as u8);
        } else {
            let mut b = Vec::new();
            let mut v = l;
            while v > 0 {
                b.insert(0, (v & 0xFF) as u8);
                v >>= 8;
            }
            o.push(0x80 | b.len() as u8);
            o.extend_from_slice(&b);
        }
    }

    #[test]
    fn full_sequence_to_connected() {
        let mut c = ClientConnector::new(cfg());
        assert!(matches!(
            c.start().unwrap().as_slice(),
            [CoreOutput::SendData(_)]
        ));
        assert_eq!(c.state(), ConnectorState::SentConnectionRequest);

        assert_eq!(c.step(&cc_tls()).unwrap(), vec![CoreOutput::UpgradeToTls]);
        assert_eq!(c.state(), ConnectorState::NeedTls);

        assert!(matches!(
            c.resume_after_tls().unwrap().as_slice(),
            [CoreOutput::SendData(_)]
        ));
        assert_eq!(c.state(), ConnectorState::SentConnectInitial);

        // Connect-Response -> sends Erect Domain + Attach User
        let out = c.step(&connect_response_frame()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(c.state(), ConnectorState::SentErectAttach);

        // Attach User Confirm -> first channel join (user channel 1007)
        assert!(matches!(
            c.step(&attach_confirm()).unwrap().as_slice(),
            [CoreOutput::SendData(_)]
        ));
        assert_eq!(c.state(), ConnectorState::JoiningChannels);

        // join confirm for user channel 1007 -> next join (io channel 1003)
        assert!(matches!(
            c.step(&join_confirm(1007)).unwrap().as_slice(),
            [CoreOutput::SendData(_)]
        ));
        // join confirm for io channel 1003 -> Connected (queue was [1007, 1003])
        let done = c.step(&join_confirm(1003)).unwrap();
        assert_eq!(
            done,
            vec![CoreOutput::Connected {
                selected_protocol: protocol::SSL,
                user_channel: 1007,
                io_channel: 1003
            }]
        );
        assert!(c.is_connected());
    }

    #[test]
    fn step_before_start_is_unexpected() {
        let mut c = ClientConnector::new(cfg());
        assert!(matches!(
            c.step(&[0x03, 0x00, 0x00, 0x04]),
            Err(CoreError::UnexpectedState)
        ));
    }
}
