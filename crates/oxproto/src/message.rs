#![forbid(unsafe_code)]

use oxrdp_pdu::{Decode, DecodeError, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor};

pub mod msg_type {
    pub const CLIENT_HELLO: u8 = 0x01;
    pub const SERVER_HELLO: u8 = 0x02;
    pub const WINDOW_CREATED: u8 = 0x10;
    pub const WINDOW_CLOSED: u8 = 0x12;
    pub const FRAME_DATA: u8 = 0x20;
    pub const POINTER_EVENT: u8 = 0x30;
}

/// Codec identifiers (FrameData.codec / hello negotiation).
pub mod codec {
    pub const H264: u8 = 1;
    pub const RAW_BGRA: u8 = 0; // uncompressed, for bring-up
}

/// Client greeting used to negotiate protocol version, initial screen size, and supported codecs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub version: u16,
    pub screen_width: u16,
    pub screen_height: u16,
    pub codecs: u8,
}

/// Server greeting used to confirm protocol version, selected codec, and assigned session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub version: u16,
    pub codec: u8,
    pub session_id: u32,
}

/// Notification that a new application window has been created on the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCreated {
    pub window_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub flags: u32,
    pub title: String,
}

/// Pointer/mouse event sent from the client to the agent for a specific window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEvent {
    pub window_id: u32,
    pub x: i32,
    pub y: i32,
    pub buttons: u8,
    pub wheel: i16,
}

/// Encoded video frame for a specific window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameData<'a> {
    pub window_id: u32,
    pub codec: u8,
    pub keyframe: bool,
    pub timestamp: u32,
    pub data: &'a [u8],
}

/// Top-level protocol message envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    WindowCreated(WindowCreated),
    WindowClosed { window_id: u32 },
    FrameData(FrameData<'a>),
    PointerEvent(PointerEvent),
}

impl ClientHello {
    const SIZE: usize = 2 + 2 + 2 + 1;

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.version, "ClientHello.version")?;
        dst.write_u16_le(self.screen_width, "ClientHello.screen_width")?;
        dst.write_u16_le(self.screen_height, "ClientHello.screen_height")?;
        dst.write_u8(self.codecs, "ClientHello.codecs")?;
        Ok(())
    }

    fn decode_payload(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(Self {
            version: src.read_u16_le("ClientHello.version")?,
            screen_width: src.read_u16_le("ClientHello.screen_width")?,
            screen_height: src.read_u16_le("ClientHello.screen_height")?,
            codecs: src.read_u8("ClientHello.codecs")?,
        })
    }
}

impl ServerHello {
    const SIZE: usize = 2 + 1 + 4;

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.version, "ServerHello.version")?;
        dst.write_u8(self.codec, "ServerHello.codec")?;
        dst.write_u32_le(self.session_id, "ServerHello.session_id")?;
        Ok(())
    }

    fn decode_payload(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(Self {
            version: src.read_u16_le("ServerHello.version")?,
            codec: src.read_u8("ServerHello.codec")?,
            session_id: src.read_u32_le("ServerHello.session_id")?,
        })
    }
}

impl WindowCreated {
    fn payload_size(&self) -> usize {
        4 + 4 + 4 + 2 + 2 + 4 + 2 + self.title.len()
    }

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowCreated.window_id")?;
        dst.write_u32_le(self.x as u32, "WindowCreated.x")?;
        dst.write_u32_le(self.y as u32, "WindowCreated.y")?;
        dst.write_u16_le(self.width, "WindowCreated.width")?;
        dst.write_u16_le(self.height, "WindowCreated.height")?;
        dst.write_u32_le(self.flags, "WindowCreated.flags")?;
        let title_len =
            u16::try_from(self.title.len()).map_err(|_| oxrdp_pdu::EncodeError::FieldTooLarge {
                context: "WindowCreated",
                field: "title length",
            })?;
        dst.write_u16_le(title_len, "WindowCreated.title_len")?;
        dst.write_slice(self.title.as_bytes(), "WindowCreated.title")?;
        Ok(())
    }

    fn decode_payload(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let window_id = src.read_u32_le("WindowCreated.window_id")?;
        let x = src.read_u32_le("WindowCreated.x")? as i32;
        let y = src.read_u32_le("WindowCreated.y")? as i32;
        let width = src.read_u16_le("WindowCreated.width")?;
        let height = src.read_u16_le("WindowCreated.height")?;
        let flags = src.read_u32_le("WindowCreated.flags")?;
        let title_len = src.read_u16_le("WindowCreated.title_len")? as usize;
        let title_bytes = src.read_slice(title_len, "WindowCreated.title")?;
        let title =
            String::from_utf8(title_bytes.to_vec()).map_err(|_| DecodeError::InvalidField {
                context: "WindowCreated",
                field: "title",
                reason: "invalid UTF-8",
            })?;
        Ok(Self {
            window_id,
            x,
            y,
            width,
            height,
            flags,
            title,
        })
    }
}

impl PointerEvent {
    const SIZE: usize = 4 + 4 + 4 + 1 + 2;

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "PointerEvent.window_id")?;
        dst.write_u32_le(self.x as u32, "PointerEvent.x")?;
        dst.write_u32_le(self.y as u32, "PointerEvent.y")?;
        dst.write_u8(self.buttons, "PointerEvent.buttons")?;
        dst.write_u16_le(self.wheel as u16, "PointerEvent.wheel")?;
        Ok(())
    }

    fn decode_payload(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("PointerEvent.window_id")?,
            x: src.read_u32_le("PointerEvent.x")? as i32,
            y: src.read_u32_le("PointerEvent.y")? as i32,
            buttons: src.read_u8("PointerEvent.buttons")?,
            wheel: src.read_u16_le("PointerEvent.wheel")? as i16,
        })
    }
}

impl<'a> FrameData<'a> {
    fn payload_size(&self) -> usize {
        4 + 1 + 1 + 4 + 4 + self.data.len()
    }

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "FrameData.window_id")?;
        dst.write_u8(self.codec, "FrameData.codec")?;
        dst.write_u8(u8::from(self.keyframe), "FrameData.keyframe")?;
        dst.write_u32_le(self.timestamp, "FrameData.timestamp")?;
        let data_len =
            u32::try_from(self.data.len()).map_err(|_| oxrdp_pdu::EncodeError::FieldTooLarge {
                context: "FrameData",
                field: "data length",
            })?;
        dst.write_u32_le(data_len, "FrameData.data_len")?;
        dst.write_slice(self.data, "FrameData.data")?;
        Ok(())
    }

    fn decode_payload(src: &mut ReadCursor<'a>) -> DecodeResult<Self> {
        let window_id = src.read_u32_le("FrameData.window_id")?;
        let codec = src.read_u8("FrameData.codec")?;
        let keyframe = src.read_u8("FrameData.keyframe")? != 0;
        let timestamp = src.read_u32_le("FrameData.timestamp")?;
        let data_len = src.read_u32_le("FrameData.data_len")? as usize;
        let data = src.read_slice(data_len, "FrameData.data")?;
        Ok(Self {
            window_id,
            codec,
            keyframe,
            timestamp,
            data,
        })
    }
}

impl Message<'_> {
    fn payload_size(&self) -> usize {
        match self {
            Message::ClientHello(_) => ClientHello::SIZE,
            Message::ServerHello(_) => ServerHello::SIZE,
            Message::WindowCreated(w) => w.payload_size(),
            Message::WindowClosed { .. } => 4,
            Message::FrameData(f) => f.payload_size(),
            Message::PointerEvent(_) => PointerEvent::SIZE,
        }
    }

    fn encode_payload(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        match self {
            Message::ClientHello(h) => h.encode_payload(dst),
            Message::ServerHello(h) => h.encode_payload(dst),
            Message::WindowCreated(w) => w.encode_payload(dst),
            Message::WindowClosed { window_id } => {
                dst.write_u32_le(*window_id, "WindowClosed.window_id")
            }
            Message::FrameData(f) => f.encode_payload(dst),
            Message::PointerEvent(e) => e.encode_payload(dst),
        }
    }

    fn decode_payload<'de>(msg_type: u8, src: &mut ReadCursor<'de>) -> DecodeResult<Message<'de>> {
        match msg_type {
            msg_type::CLIENT_HELLO => Ok(Message::ClientHello(ClientHello::decode_payload(src)?)),
            msg_type::SERVER_HELLO => Ok(Message::ServerHello(ServerHello::decode_payload(src)?)),
            msg_type::WINDOW_CREATED => {
                Ok(Message::WindowCreated(WindowCreated::decode_payload(src)?))
            }
            msg_type::WINDOW_CLOSED => Ok(Message::WindowClosed {
                window_id: src.read_u32_le("WindowClosed.window_id")?,
            }),
            msg_type::FRAME_DATA => Ok(Message::FrameData(FrameData::decode_payload(src)?)),
            msg_type::POINTER_EVENT => {
                Ok(Message::PointerEvent(PointerEvent::decode_payload(src)?))
            }
            _ => Err(DecodeError::InvalidField {
                context: "oxproto message",
                field: "type",
                reason: "unknown message type",
            }),
        }
    }
}

impl Encode for Message<'_> {
    fn size(&self) -> usize {
        1 + 4 + self.payload_size()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let msg_type = match self {
            Message::ClientHello(_) => msg_type::CLIENT_HELLO,
            Message::ServerHello(_) => msg_type::SERVER_HELLO,
            Message::WindowCreated(_) => msg_type::WINDOW_CREATED,
            Message::WindowClosed { .. } => msg_type::WINDOW_CLOSED,
            Message::FrameData(_) => msg_type::FRAME_DATA,
            Message::PointerEvent(_) => msg_type::POINTER_EVENT,
        };
        dst.write_u8(msg_type, "Message.type")?;
        dst.write_u32_le(self.payload_size() as u32, "Message.payload_len")?;
        self.encode_payload(dst)
    }
}

impl<'de> Decode<'de> for Message<'de> {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let msg_type = src.read_u8("Message.type")?;
        let _payload_len = src.read_u32_le("Message.payload_len")?;
        Self::decode_payload(msg_type, src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::{decode, encode_vec};

    #[test]
    fn client_hello_round_trip() {
        let m = Message::ClientHello(ClientHello {
            version: 1,
            screen_width: 1920,
            screen_height: 1080,
            codecs: 1 << codec::H264,
        });
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes[0], msg_type::CLIENT_HELLO);
        assert_eq!(decode::<Message>(&bytes).unwrap(), m);
    }

    #[test]
    fn server_hello_round_trip() {
        let m = Message::ServerHello(ServerHello {
            version: 1,
            codec: codec::H264,
            session_id: 0xDEAD_BEEF,
        });
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(decode::<Message>(&bytes).unwrap(), m);
    }

    #[test]
    fn window_created_round_trip() {
        let m = Message::WindowCreated(WindowCreated {
            window_id: 7,
            x: -100,
            y: 50,
            width: 800,
            height: 600,
            flags: 0,
            title: "Notepad".into(),
        });
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(decode::<Message>(&bytes).unwrap(), m);
    }

    #[test]
    fn window_closed_round_trip() {
        let m = Message::WindowClosed { window_id: 7 };
        assert_eq!(decode::<Message>(&encode_vec(&m).unwrap()).unwrap(), m);
    }

    #[test]
    fn frame_data_round_trip() {
        let payload = [0xAAu8, 0xBB, 0xCC];
        let m = Message::FrameData(FrameData {
            window_id: 7,
            codec: codec::H264,
            keyframe: true,
            timestamp: 12345,
            data: &payload,
        });
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(decode::<Message>(&bytes).unwrap(), m);
    }

    #[test]
    fn pointer_event_round_trip() {
        let m = Message::PointerEvent(PointerEvent {
            window_id: 7,
            x: 10,
            y: -20,
            buttons: 0x01,
            wheel: -3,
        });
        assert_eq!(decode::<Message>(&encode_vec(&m).unwrap()).unwrap(), m);
    }

    #[test]
    fn rejects_unknown_type() {
        let err = decode::<Message>(&[0xFF, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField { field: "type", .. }
        ));
    }
}
