//! Handshake and control-channel messages (`docs/design/OXPROTO.md` §7, §10, §15).

use oxrdp_pdu::{Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor};

use crate::wire::{
    read_i32, read_string, read_u64, string_size, write_i32, write_string, write_u64,
};

/// One client display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Output identifier.
    pub id: u8,
    /// X coordinate in the virtual desktop.
    pub x: i32,
    /// Y coordinate in the virtual desktop.
    pub y: i32,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Scale numerator.
    pub scale_num: u16,
    /// Scale denominator.
    pub scale_den: u16,
    /// Refresh rate in millihertz.
    pub refresh_mhz: u32,
}

impl Encode for Output {
    fn size(&self) -> usize {
        1 + 4 + 4 + 2 + 2 + 2 + 2 + 4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(self.id, "Output.id")?;
        write_i32(dst, self.x, "Output.x")?;
        write_i32(dst, self.y, "Output.y")?;
        dst.write_u16_le(self.width, "Output.width")?;
        dst.write_u16_le(self.height, "Output.height")?;
        dst.write_u16_le(self.scale_num, "Output.scale_num")?;
        dst.write_u16_le(self.scale_den, "Output.scale_den")?;
        dst.write_u32_le(self.refresh_mhz, "Output.refresh_mhz")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for Output {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            id: src.read_u8("Output.id")?,
            x: read_i32(src, "Output.x")?,
            y: read_i32(src, "Output.y")?,
            width: src.read_u16_le("Output.width")?,
            height: src.read_u16_le("Output.height")?,
            scale_num: src.read_u16_le("Output.scale_num")?,
            scale_den: src.read_u16_le("Output.scale_den")?,
            refresh_mhz: src.read_u32_le("Output.refresh_mhz")?,
        })
    }
}

/// The client's output topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayLayout {
    /// Client outputs that make up the virtual desktop.
    pub outputs: Vec<Output>,
}

impl Encode for DisplayLayout {
    fn size(&self) -> usize {
        1 + self.outputs.len() * 21
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(
            u8::try_from(self.outputs.len()).unwrap_or(u8::MAX),
            "DisplayLayout.outputs_count",
        )?;
        for output in &self.outputs {
            output.encode(dst)?;
        }
        Ok(())
    }
}

impl<'de> Decode<'de> for DisplayLayout {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let count = src.read_u8("DisplayLayout.outputs_count")?;
        let mut outputs = Vec::with_capacity(usize::from(count));
        for _ in 0..usize::from(count) {
            outputs.push(Output::decode(src)?);
        }
        Ok(Self { outputs })
    }
}

/// Client handshake message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    /// Minimum supported protocol version.
    pub version_min: u16,
    /// Maximum supported protocol version.
    pub version_max: u16,
    /// Feature bitmask.
    pub features: u64,
    /// Authentication token.
    pub auth_token: String,
    /// Human-readable client name.
    pub client_name: String,
    /// Supported codec identifiers.
    pub codecs: Vec<u8>,
    /// Client display layout.
    pub display: DisplayLayout,
}

impl Encode for ClientHello {
    fn size(&self) -> usize {
        2 + 2
            + 8
            + string_size(&self.auth_token)
            + string_size(&self.client_name)
            + 1
            + self.codecs.len()
            + self.display.size()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.version_min, "ClientHello.version_min")?;
        dst.write_u16_le(self.version_max, "ClientHello.version_max")?;
        write_u64(dst, self.features, "ClientHello.features")?;
        write_string(dst, &self.auth_token, "ClientHello.auth_token")?;
        write_string(dst, &self.client_name, "ClientHello.client_name")?;
        dst.write_u8(
            u8::try_from(self.codecs.len()).unwrap_or(u8::MAX),
            "ClientHello.codecs_count",
        )?;
        for codec in &self.codecs {
            dst.write_u8(*codec, "ClientHello.codecs")?;
        }
        self.display.encode(dst)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ClientHello {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            version_min: src.read_u16_le("ClientHello.version_min")?,
            version_max: src.read_u16_le("ClientHello.version_max")?,
            features: read_u64(src, "ClientHello.features")?,
            auth_token: read_string(src, "ClientHello.auth_token")?,
            client_name: read_string(src, "ClientHello.client_name")?,
            codecs: {
                let count = src.read_u8("ClientHello.codecs_count")?;
                let mut codecs = Vec::with_capacity(usize::from(count));
                for _ in 0..usize::from(count) {
                    codecs.push(src.read_u8("ClientHello.codecs")?);
                }
                codecs
            },
            display: DisplayLayout::decode(src)?,
        })
    }
}

/// Server handshake reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// Negotiated protocol version.
    pub version: u16,
    /// Enabled feature bitmask.
    pub features: u64,
    /// Session identifier.
    pub session_id: u64,
    /// Selected codec identifier.
    pub codec: u8,
}

impl Encode for ServerHello {
    fn size(&self) -> usize {
        2 + 8 + 8 + 1
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.version, "ServerHello.version")?;
        write_u64(dst, self.features, "ServerHello.features")?;
        write_u64(dst, self.session_id, "ServerHello.session_id")?;
        dst.write_u8(self.codec, "ServerHello.codec")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ServerHello {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            version: src.read_u16_le("ServerHello.version")?,
            features: read_u64(src, "ServerHello.features")?,
            session_id: read_u64(src, "ServerHello.session_id")?,
            codec: src.read_u8("ServerHello.codec")?,
        })
    }
}

/// Control-channel error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Error code.
    pub code: u16,
    /// Human-readable error message.
    pub message: String,
}

impl Encode for Error {
    fn size(&self) -> usize {
        2 + string_size(&self.message)
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.code, "Error.code")?;
        write_string(dst, &self.message, "Error.message")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for Error {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            code: src.read_u16_le("Error.code")?,
            message: read_string(src, "Error.message")?,
        })
    }
}

/// Session close message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Close {
    /// Close reason code.
    pub reason: u16,
}

impl Encode for Close {
    fn size(&self) -> usize {
        2
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.reason, "Close.reason")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for Close {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            reason: src.read_u16_le("Close.reason")?,
        })
    }
}

/// Liveness probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    /// Sequence number echoed by the reply.
    pub seq: u32,
    /// Sender's clock in microseconds.
    pub sent_us: u64,
}

impl Encode for Ping {
    fn size(&self) -> usize {
        4 + 8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.seq, "Ping.seq")?;
        write_u64(dst, self.sent_us, "Ping.sent_us")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for Ping {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            seq: src.read_u32_le("Ping.seq")?,
            sent_us: read_u64(src, "Ping.sent_us")?,
        })
    }
}

/// Liveness reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    /// Echoed sequence number.
    pub seq: u32,
    /// Original sender clock in microseconds.
    pub sent_us: u64,
    /// Agent clock in microseconds.
    pub agent_us: u64,
}

impl Encode for Pong {
    fn size(&self) -> usize {
        4 + 8 + 8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.seq, "Pong.seq")?;
        write_u64(dst, self.sent_us, "Pong.sent_us")?;
        write_u64(dst, self.agent_us, "Pong.agent_us")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for Pong {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            seq: src.read_u32_le("Pong.seq")?,
            sent_us: read_u64(src, "Pong.sent_us")?,
            agent_us: read_u64(src, "Pong.agent_us")?,
        })
    }
}

/// Quality-of-service hint for a window or the whole session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityHint {
    /// Target window (0 means all windows).
    pub window_id: u32,
    /// Target frames per second.
    pub target_fps: u16,
    /// Maximum bitrate in kilobits per second.
    pub max_bitrate_kbps: u32,
    /// Hint flags.
    pub flags: u8,
}

impl Encode for QualityHint {
    fn size(&self) -> usize {
        4 + 2 + 4 + 1
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "QualityHint.window_id")?;
        dst.write_u16_le(self.target_fps, "QualityHint.target_fps")?;
        dst.write_u32_le(self.max_bitrate_kbps, "QualityHint.max_bitrate_kbps")?;
        dst.write_u8(self.flags, "QualityHint.flags")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for QualityHint {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("QualityHint.window_id")?,
            target_fps: src.read_u16_le("QualityHint.target_fps")?,
            max_bitrate_kbps: src.read_u32_le("QualityHint.max_bitrate_kbps")?,
            flags: src.read_u8("QualityHint.flags")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::{decode, encode_vec};

    fn layout() -> DisplayLayout {
        DisplayLayout {
            outputs: vec![
                Output {
                    id: 0,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale_num: 1,
                    scale_den: 1,
                    refresh_mhz: 60_000,
                },
                Output {
                    id: 1,
                    x: 1920,
                    y: -120,
                    width: 2560,
                    height: 1440,
                    scale_num: 3,
                    scale_den: 2,
                    refresh_mhz: 143_980,
                },
            ],
        }
    }

    #[test]
    fn output_and_layout_round_trip() {
        let l = layout();
        let bytes = encode_vec(&l).unwrap();
        assert_eq!(bytes.len(), l.size());
        assert_eq!(bytes.len(), 1 + 2 * 21);
        assert_eq!(decode::<DisplayLayout>(&bytes).unwrap(), l);
    }

    #[test]
    fn client_hello_round_trip() {
        let m = ClientHello {
            version_min: 1,
            version_max: 1,
            features: 0b1011,
            auth_token: "s3cret".into(),
            client_name: "oxclient".into(),
            codecs: vec![2, 1],
            display: layout(),
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), m.size());
        assert_eq!(decode::<ClientHello>(&bytes).unwrap(), m);
    }

    #[test]
    fn server_hello_round_trip() {
        let m = ServerHello {
            version: 1,
            features: 0b11,
            session_id: 0xDEAD_BEEF_CAFE_0001,
            codec: 1,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), 19);
        assert_eq!(decode::<ServerHello>(&bytes).unwrap(), m);
    }

    #[test]
    fn small_control_messages_round_trip() {
        let e = Error {
            code: 2,
            message: "auth failed".into(),
        };
        assert_eq!(decode::<Error>(&encode_vec(&e).unwrap()).unwrap(), e);

        let c = Close { reason: 1 };
        assert_eq!(decode::<Close>(&encode_vec(&c).unwrap()).unwrap(), c);

        let p = Ping {
            seq: 7,
            sent_us: 1_234_567_890,
        };
        assert_eq!(encode_vec(&p).unwrap().len(), 12);
        assert_eq!(decode::<Ping>(&encode_vec(&p).unwrap()).unwrap(), p);

        let q = Pong {
            seq: 7,
            sent_us: 1_234_567_890,
            agent_us: 9_876_543_210,
        };
        assert_eq!(encode_vec(&q).unwrap().len(), 20);
        assert_eq!(decode::<Pong>(&encode_vec(&q).unwrap()).unwrap(), q);

        let h = QualityHint {
            window_id: 0,
            target_fps: 60,
            max_bitrate_kbps: 20_000,
            flags: 1,
        };
        assert_eq!(encode_vec(&h).unwrap().len(), 11);
        assert_eq!(decode::<QualityHint>(&encode_vec(&h).unwrap()).unwrap(), h);
    }

    #[test]
    fn u64_is_little_endian() {
        let p = Ping {
            seq: 0,
            sent_us: 0x0102_0304_0506_0708,
        };
        let bytes = encode_vec(&p).unwrap();
        assert_eq!(
            &bytes[4..12],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
    }
}
