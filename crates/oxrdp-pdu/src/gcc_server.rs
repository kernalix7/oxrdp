use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

const SC_CORE: u16 = 0x0C01;
const SC_NET: u16 = 0x0C03;

/// Server Core Data (TS_UD_SC_CORE, type 0x0C01).
///
/// Carried in the MCS Connect-Response as a GCC user-data block.
/// Fields after the 4-byte header are positional and length-driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerCoreData {
    /// Server RDP version (mandatory).
    pub version: u32,
    /// Optional client-requested protocols.
    pub client_requested_protocols: Option<u32>,
    /// Optional early capability flags.
    pub early_capability_flags: Option<u32>,
}

impl Encode for ServerCoreData {
    fn size(&self) -> usize {
        let mut len = 4 + 4;
        if self.client_requested_protocols.is_some() {
            len += 4;
            if self.early_capability_flags.is_some() {
                len += 4;
            }
        }
        len
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let size = self.size();
        let length = u16::try_from(size).map_err(|_| EncodeError::FieldTooLarge {
            context: "GCC SC_CORE",
            field: "length",
        })?;
        dst.write_u16_le(SC_CORE, "ud type")?;
        dst.write_u16_le(length, "ud length")?;
        dst.write_u32_le(self.version, "version")?;
        if let Some(protocols) = self.client_requested_protocols {
            dst.write_u32_le(protocols, "clientRequestedProtocols")?;
            if let Some(flags) = self.early_capability_flags {
                dst.write_u32_le(flags, "earlyCapabilityFlags")?;
            }
        }
        Ok(())
    }
}

impl<'de> Decode<'de> for ServerCoreData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let ud_type = src.read_u16_le("ud type")?;
        if ud_type != SC_CORE {
            return Err(DecodeError::InvalidField {
                context: "GCC SC_CORE",
                field: "ud type",
                reason: "unexpected GCC block type",
            });
        }
        let length = src.read_u16_le("ud length")?;
        let content = (length as usize)
            .checked_sub(4)
            .ok_or(DecodeError::InvalidLength {
                context: "GCC SC_CORE",
                reason: "length smaller than header",
            })?;

        if content < 4 {
            return Err(DecodeError::NotEnoughBytes {
                context: "GCC SC_CORE",
                needed: 4,
                remaining: content,
            });
        }
        let version = src.read_u32_le("version")?;

        let client_requested_protocols = if content >= 8 {
            Some(src.read_u32_le("clientRequestedProtocols")?)
        } else {
            None
        };

        let early_capability_flags = if content >= 12 {
            Some(src.read_u32_le("earlyCapabilityFlags")?)
        } else {
            None
        };

        Ok(Self {
            version,
            client_requested_protocols,
            early_capability_flags,
        })
    }
}

/// Server Network Data (TS_UD_SC_NET, type 0x0C03).
///
/// Carried in the MCS Connect-Response as a GCC user-data block. The channel
/// id array is followed by a 2-byte alignment pad when the channel count is
/// odd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNetworkData {
    /// I/O channel MCS id.
    pub mcs_channel_id: u16,
    /// Static virtual channel ids.
    pub channel_ids: Vec<u16>,
}

impl Encode for ServerNetworkData {
    fn size(&self) -> usize {
        let mut len = 4 + 2 + 2 + self.channel_ids.len() * 2;
        if self.channel_ids.len() % 2 == 1 {
            len += 2;
        }
        len
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let size = self.size();
        let length = u16::try_from(size).map_err(|_| EncodeError::FieldTooLarge {
            context: "GCC SC_NET",
            field: "length",
        })?;
        dst.write_u16_le(SC_NET, "ud type")?;
        dst.write_u16_le(length, "ud length")?;
        dst.write_u16_le(self.mcs_channel_id, "mcsChannelId")?;
        let count =
            u16::try_from(self.channel_ids.len()).map_err(|_| EncodeError::FieldTooLarge {
                context: "GCC SC_NET",
                field: "channelCount",
            })?;
        dst.write_u16_le(count, "channelCount")?;
        for id in &self.channel_ids {
            dst.write_u16_le(*id, "channelId")?;
        }
        if self.channel_ids.len() % 2 == 1 {
            dst.write_u16_le(0x0000, "alignmentPad")?;
        }
        Ok(())
    }
}

impl<'de> Decode<'de> for ServerNetworkData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let ud_type = src.read_u16_le("ud type")?;
        if ud_type != SC_NET {
            return Err(DecodeError::InvalidField {
                context: "GCC SC_NET",
                field: "ud type",
                reason: "unexpected GCC block type",
            });
        }
        let _length = src.read_u16_le("ud length")?;
        let mcs_channel_id = src.read_u16_le("mcsChannelId")?;
        let channel_count = src.read_u16_le("channelCount")?;

        let mut channel_ids = Vec::with_capacity(channel_count as usize);
        for _ in 0..channel_count {
            channel_ids.push(src.read_u16_le("channelId")?);
        }

        if channel_count % 2 == 1 {
            let _pad = src.read_u16_le("alignmentPad")?;
        }

        Ok(Self {
            mcs_channel_id,
            channel_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn sc_core_version_and_protocols() {
        let core = ServerCoreData {
            version: 0x0008_0004,
            client_requested_protocols: Some(1),
            early_capability_flags: None,
        };
        let bytes = encode_vec(&core).unwrap();
        assert_eq!(
            bytes,
            [0x01, 0x0C, 0x0C, 0x00, 0x04, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode::<ServerCoreData>(&bytes).unwrap(), core);
    }

    #[test]
    fn sc_core_version_only() {
        let core = ServerCoreData {
            version: 0x0008_0004,
            client_requested_protocols: None,
            early_capability_flags: None,
        };
        let bytes = encode_vec(&core).unwrap();
        assert_eq!(bytes, [0x01, 0x0C, 0x08, 0x00, 0x04, 0x00, 0x08, 0x00]);
        assert_eq!(decode::<ServerCoreData>(&bytes).unwrap(), core);
    }

    #[test]
    fn sc_net_one_channel_has_pad() {
        let net = ServerNetworkData {
            mcs_channel_id: 1003,
            channel_ids: vec![1004],
        };
        let bytes = encode_vec(&net).unwrap();
        // type 0C03, len 12, mcsChannelId 0x03EB, count 1, id 0x03EC, pad 0000
        assert_eq!(
            bytes,
            [0x03, 0x0C, 0x0C, 0x00, 0xEB, 0x03, 0x01, 0x00, 0xEC, 0x03, 0x00, 0x00]
        );
        assert_eq!(decode::<ServerNetworkData>(&bytes).unwrap(), net);
    }

    #[test]
    fn sc_net_two_channels_no_pad() {
        let net = ServerNetworkData {
            mcs_channel_id: 1003,
            channel_ids: vec![1004, 1005],
        };
        let bytes = encode_vec(&net).unwrap();
        assert_eq!(
            bytes,
            [0x03, 0x0C, 0x0C, 0x00, 0xEB, 0x03, 0x02, 0x00, 0xEC, 0x03, 0xED, 0x03]
        );
        assert_eq!(decode::<ServerNetworkData>(&bytes).unwrap(), net);
    }

    #[test]
    fn rejects_wrong_type() {
        let err = decode::<ServerCoreData>(&[0x03, 0x0C, 0x08, 0x00, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "ud type",
                ..
            }
        ));
    }
}
