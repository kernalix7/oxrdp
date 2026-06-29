use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

const CS_CORE: u16 = 0xC001;
const CS_SECURITY: u16 = 0xC002;
const CS_NET: u16 = 0xC003;

fn read_ud_header(
    src: &mut ReadCursor<'_>,
    expected_type: u16,
    ctx: &'static str,
) -> DecodeResult<u16> {
    let ty = src.read_u16_le(ctx)?;
    if ty != expected_type {
        return Err(DecodeError::InvalidField {
            context: ctx,
            field: "ud type",
            reason: "unexpected GCC block type",
        });
    }
    let len = src.read_u16_le(ctx)?;
    Ok(len)
}

/// Client Core Data (CS_CORE) block for GCC Connect-Initial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientCoreData {
    pub version: u32,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub color_depth: u16,
    pub sas_sequence: u16,
    pub keyboard_layout: u32,
    pub client_build: u32,
    pub client_name: [u8; 32],
    pub keyboard_type: u32,
    pub keyboard_subtype: u32,
    pub keyboard_function_key: u32,
    pub ime_file_name: [u8; 64],
}

impl Encode for ClientCoreData {
    fn size(&self) -> usize {
        132
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        const CTX: &str = "GCC CS_CORE";
        dst.write_u16_le(CS_CORE, CTX)?;
        dst.write_u16_le(132, CTX)?;
        dst.write_u32_le(self.version, CTX)?;
        dst.write_u16_le(self.desktop_width, CTX)?;
        dst.write_u16_le(self.desktop_height, CTX)?;
        dst.write_u16_le(self.color_depth, CTX)?;
        dst.write_u16_le(self.sas_sequence, CTX)?;
        dst.write_u32_le(self.keyboard_layout, CTX)?;
        dst.write_u32_le(self.client_build, CTX)?;
        dst.write_slice(&self.client_name, CTX)?;
        dst.write_u32_le(self.keyboard_type, CTX)?;
        dst.write_u32_le(self.keyboard_subtype, CTX)?;
        dst.write_u32_le(self.keyboard_function_key, CTX)?;
        dst.write_slice(&self.ime_file_name, CTX)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ClientCoreData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        const CTX: &str = "GCC CS_CORE";
        let len = read_ud_header(src, CS_CORE, CTX)?;
        if len != 132 {
            return Err(DecodeError::InvalidLength {
                context: CTX,
                reason: "expected CS_CORE block length of 132 bytes",
            });
        }
        let version = src.read_u32_le(CTX)?;
        let desktop_width = src.read_u16_le(CTX)?;
        let desktop_height = src.read_u16_le(CTX)?;
        let color_depth = src.read_u16_le(CTX)?;
        let sas_sequence = src.read_u16_le(CTX)?;
        let keyboard_layout = src.read_u32_le(CTX)?;
        let client_build = src.read_u32_le(CTX)?;
        let client_name = src.read_array::<32>(CTX)?;
        let keyboard_type = src.read_u32_le(CTX)?;
        let keyboard_subtype = src.read_u32_le(CTX)?;
        let keyboard_function_key = src.read_u32_le(CTX)?;
        let ime_file_name = src.read_array::<64>(CTX)?;
        Ok(Self {
            version,
            desktop_width,
            desktop_height,
            color_depth,
            sas_sequence,
            keyboard_layout,
            client_build,
            client_name,
            keyboard_type,
            keyboard_subtype,
            keyboard_function_key,
            ime_file_name,
        })
    }
}

/// Client Security Data (CS_SECURITY) block for GCC Connect-Initial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientSecurityData {
    pub encryption_methods: u32,
    pub ext_encryption_methods: u32,
}

impl Encode for ClientSecurityData {
    fn size(&self) -> usize {
        12
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        const CTX: &str = "GCC CS_SECURITY";
        dst.write_u16_le(CS_SECURITY, CTX)?;
        dst.write_u16_le(12, CTX)?;
        dst.write_u32_le(self.encryption_methods, CTX)?;
        dst.write_u32_le(self.ext_encryption_methods, CTX)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ClientSecurityData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        const CTX: &str = "GCC CS_SECURITY";
        let len = read_ud_header(src, CS_SECURITY, CTX)?;
        if len != 12 {
            return Err(DecodeError::InvalidLength {
                context: CTX,
                reason: "expected CS_SECURITY block length of 12 bytes",
            });
        }
        let encryption_methods = src.read_u32_le(CTX)?;
        let ext_encryption_methods = src.read_u32_le(CTX)?;
        Ok(Self {
            encryption_methods,
            ext_encryption_methods,
        })
    }
}

/// A single virtual channel definition inside Client Network Data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelDef {
    pub name: [u8; 8],
    pub options: u32,
}

/// Client Network Data (CS_NET) block for GCC Connect-Initial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientNetworkData {
    pub channels: Vec<ChannelDef>,
}

impl Encode for ClientNetworkData {
    fn size(&self) -> usize {
        4 + 4 + self.channels.len() * 12
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        const CTX: &str = "GCC CS_NET";
        let total_len = self.size();
        if total_len > u16::MAX as usize {
            return Err(EncodeError::FieldTooLarge {
                context: CTX,
                field: "length",
            });
        }
        dst.write_u16_le(CS_NET, CTX)?;
        dst.write_u16_le(total_len as u16, CTX)?;
        if self.channels.len() > u32::MAX as usize {
            return Err(EncodeError::FieldTooLarge {
                context: CTX,
                field: "channelCount",
            });
        }
        dst.write_u32_le(self.channels.len() as u32, CTX)?;
        for ch in &self.channels {
            dst.write_slice(&ch.name, CTX)?;
            dst.write_u32_le(ch.options, CTX)?;
        }
        Ok(())
    }
}

impl<'de> Decode<'de> for ClientNetworkData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        const CTX: &str = "GCC CS_NET";
        let len = read_ud_header(src, CS_NET, CTX)?;
        let channel_count = src.read_u32_le(CTX)?;
        if channel_count > u32::MAX / 12 {
            return Err(DecodeError::InvalidField {
                context: CTX,
                field: "channelCount",
                reason: "channel count too large",
            });
        }
        // TS_UD length covers the 4-byte header + 4-byte channelCount + channels.
        let channel_bytes = channel_count
            .checked_mul(12)
            .ok_or(DecodeError::InvalidField {
                context: CTX,
                field: "channelCount",
                reason: "overflow calculating total length",
            })?;
        let expected_len = channel_bytes
            .checked_add(8)
            .ok_or(DecodeError::InvalidField {
                context: CTX,
                field: "channelCount",
                reason: "overflow calculating total length",
            })?;
        if len != expected_len as u16 {
            return Err(DecodeError::InvalidLength {
                context: CTX,
                reason: "CS_NET length does not match channel count",
            });
        }
        let mut channels = Vec::with_capacity(channel_count as usize);
        for _ in 0..channel_count {
            let name = src.read_array::<8>(CTX)?;
            let options = src.read_u32_le(CTX)?;
            channels.push(ChannelDef { name, options });
        }
        Ok(Self { channels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    fn name32(s: &str) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[..s.len()].copy_from_slice(s.as_bytes());
        a
    }

    #[test]
    fn cs_core_round_trip() {
        let core = ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1024,
            desktop_height: 768,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0000_0409,
            client_build: 2600,
            client_name: name32("oxrdp"),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: [0u8; 64],
        };
        let bytes = encode_vec(&core).unwrap();
        let mut e = Vec::new();
        e.extend_from_slice(&[0x01, 0xC0]); // CS_CORE
        e.extend_from_slice(&132u16.to_le_bytes());
        e.extend_from_slice(&0x0008_0004u32.to_le_bytes());
        e.extend_from_slice(&1024u16.to_le_bytes());
        e.extend_from_slice(&768u16.to_le_bytes());
        e.extend_from_slice(&0xCA01u16.to_le_bytes());
        e.extend_from_slice(&0xAA03u16.to_le_bytes());
        e.extend_from_slice(&0x0000_0409u32.to_le_bytes());
        e.extend_from_slice(&2600u32.to_le_bytes());
        e.extend_from_slice(&name32("oxrdp"));
        e.extend_from_slice(&4u32.to_le_bytes());
        e.extend_from_slice(&0u32.to_le_bytes());
        e.extend_from_slice(&12u32.to_le_bytes());
        e.extend_from_slice(&[0u8; 64]);
        assert_eq!(bytes, e);
        assert_eq!(bytes.len(), 132);
        assert_eq!(decode::<ClientCoreData>(&bytes).unwrap(), core);
    }

    #[test]
    fn cs_security_round_trip() {
        let sec = ClientSecurityData {
            encryption_methods: 0x0000_0003,
            ext_encryption_methods: 0,
        };
        let bytes = encode_vec(&sec).unwrap();
        assert_eq!(
            bytes,
            [0x02, 0xC0, 0x0C, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode::<ClientSecurityData>(&bytes).unwrap(), sec);
    }

    #[test]
    fn cs_net_one_channel() {
        let net = ClientNetworkData {
            channels: vec![ChannelDef {
                name: *b"rdpdr\0\0\0",
                options: 0x8080_0000,
            }],
        };
        let bytes = encode_vec(&net).unwrap();
        let mut e = Vec::new();
        e.extend_from_slice(&[0x03, 0xC0]); // CS_NET
        e.extend_from_slice(&20u16.to_le_bytes()); // 4 header + 4 count + 12
        e.extend_from_slice(&1u32.to_le_bytes());
        e.extend_from_slice(b"rdpdr\0\0\0");
        e.extend_from_slice(&0x8080_0000u32.to_le_bytes());
        assert_eq!(bytes, e);
        assert_eq!(decode::<ClientNetworkData>(&bytes).unwrap(), net);
    }

    #[test]
    fn rejects_wrong_block_type() {
        let err = decode::<ClientSecurityData>(&[0x01, 0xC0, 0x0C, 0x00, 0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "ud type",
                ..
            }
        ));
    }
}
