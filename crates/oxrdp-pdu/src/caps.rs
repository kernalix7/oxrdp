use crate::codec::{encode_vec, Encode};
use crate::cursor::WriteCursor;
use crate::error::EncodeResult;

/// Capability set type constants (MS-RDPBCGR 2.2.7.1.2).
pub mod cap_type {
    pub const GENERAL: u16 = 0x0001;
    pub const BITMAP: u16 = 0x0002;
    pub const ORDER: u16 = 0x0003;
    pub const POINTER: u16 = 0x0008;
    pub const INPUT: u16 = 0x000D;
    pub const VIRTUAL_CHANNEL: u16 = 0x0014;
}

/// General capability set (TS_GENERAL_CAPABILITYSET).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneralCapabilitySet {
    pub os_major: u16,
    pub os_minor: u16,
    pub protocol_version: u16,
    pub extra_flags: u16,
}

impl Default for GeneralCapabilitySet {
    fn default() -> Self {
        Self {
            os_major: 1,
            os_minor: 3,
            protocol_version: 0x0200,
            extra_flags: 0x040D,
        }
    }
}

impl Encode for GeneralCapabilitySet {
    fn size(&self) -> usize {
        24
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(cap_type::GENERAL, "general capability set type")?;
        dst.write_u16_le(24, "general capability set length")?;
        dst.write_u16_le(self.os_major, "os major")?;
        dst.write_u16_le(self.os_minor, "os minor")?;
        dst.write_u16_le(self.protocol_version, "protocol version")?;
        dst.write_u16_le(0, "pad2octetsA")?;
        dst.write_u16_le(0, "generalCompressionTypes")?;
        dst.write_u16_le(self.extra_flags, "extra flags")?;
        dst.write_u16_le(0, "updateCapabilityFlag")?;
        dst.write_u16_le(0, "remoteUnshareFlag")?;
        dst.write_u16_le(0, "generalCompressionLevel")?;
        dst.write_u8(0, "refreshRectSupport")?;
        dst.write_u8(0, "suppressOutputSupport")?;
        Ok(())
    }
}

/// Bitmap capability set (TS_BITMAP_CAPABILITYSET).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitmapCapabilitySet {
    pub preferred_bits_per_pixel: u16,
    pub desktop_width: u16,
    pub desktop_height: u16,
}

impl Encode for BitmapCapabilitySet {
    fn size(&self) -> usize {
        28
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(cap_type::BITMAP, "bitmap capability set type")?;
        dst.write_u16_le(28, "bitmap capability set length")?;
        dst.write_u16_le(self.preferred_bits_per_pixel, "preferred bits per pixel")?;
        dst.write_u16_le(1, "receive1BitPerPixel")?;
        dst.write_u16_le(1, "receive4BitsPerPixel")?;
        dst.write_u16_le(1, "receive8BitsPerPixel")?;
        dst.write_u16_le(self.desktop_width, "desktop width")?;
        dst.write_u16_le(self.desktop_height, "desktop height")?;
        dst.write_u16_le(0, "pad")?;
        dst.write_u16_le(1, "desktopResizeFlag")?;
        dst.write_u16_le(1, "bitmapCompressionFlag")?;
        dst.write_u8(0, "highColorFlags")?;
        dst.write_u8(0, "drawingFlags")?;
        dst.write_u16_le(1, "multipleRectangleSupport")?;
        dst.write_u16_le(0, "pad2octetsB")?;
        Ok(())
    }
}

/// Input capability set (TS_INPUT_CAPABILITYSET).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputCapabilitySet {
    pub input_flags: u16,
    pub keyboard_layout: u32,
    pub keyboard_type: u32,
    pub keyboard_subtype: u32,
    pub keyboard_function_key: u32,
}

impl Default for InputCapabilitySet {
    fn default() -> Self {
        Self {
            input_flags: 0x0001 | 0x0004 | 0x0020 | 0x0080,
            keyboard_layout: 0x0409,
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
        }
    }
}

impl Encode for InputCapabilitySet {
    fn size(&self) -> usize {
        88
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(cap_type::INPUT, "input capability set type")?;
        dst.write_u16_le(88, "input capability set length")?;
        dst.write_u16_le(self.input_flags, "input flags")?;
        dst.write_u16_le(0, "pad2octetsA")?;
        dst.write_u32_le(self.keyboard_layout, "keyboard layout")?;
        dst.write_u32_le(self.keyboard_type, "keyboard type")?;
        dst.write_u32_le(self.keyboard_subtype, "keyboard subtype")?;
        dst.write_u32_le(self.keyboard_function_key, "keyboard function key")?;
        dst.write_slice(&[0u8; 64], "input imeFileName")?;
        Ok(())
    }
}

/// Encode the default set of client capabilities (General + Bitmap + Input) for a Confirm
/// Active PDU. Returns (number_of_capability_sets, concatenated_bytes).
pub fn default_client_capabilities(
    width: u16,
    height: u16,
    bpp: u16,
) -> EncodeResult<(u16, Vec<u8>)> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&encode_vec(&GeneralCapabilitySet::default())?);
    buf.extend_from_slice(&encode_vec(&BitmapCapabilitySet {
        preferred_bits_per_pixel: bpp,
        desktop_width: width,
        desktop_height: height,
    })?);
    buf.extend_from_slice(&encode_vec(&InputCapabilitySet::default())?);
    Ok((3, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_vec;

    #[test]
    fn general_is_24_bytes_with_header() {
        let b = encode_vec(&GeneralCapabilitySet::default()).unwrap();
        assert_eq!(b.len(), 24);
        assert_eq!(&b[0..2], &cap_type::GENERAL.to_le_bytes()); // type
        assert_eq!(&b[2..4], &24u16.to_le_bytes()); // length
    }

    #[test]
    fn bitmap_is_28_bytes() {
        let b = encode_vec(&BitmapCapabilitySet {
            preferred_bits_per_pixel: 32,
            desktop_width: 1024,
            desktop_height: 768,
        })
        .unwrap();
        assert_eq!(b.len(), 28);
        assert_eq!(&b[0..2], &cap_type::BITMAP.to_le_bytes());
        assert_eq!(&b[4..6], &32u16.to_le_bytes()); // preferred bpp
    }

    #[test]
    fn input_is_88_bytes() {
        let b = encode_vec(&InputCapabilitySet::default()).unwrap();
        assert_eq!(b.len(), 88);
        assert_eq!(&b[0..2], &cap_type::INPUT.to_le_bytes());
    }

    #[test]
    fn default_client_caps_bundles_three() {
        let (n, blob) = default_client_capabilities(1024, 768, 32).unwrap();
        assert_eq!(n, 3);
        assert_eq!(blob.len(), 24 + 28 + 88);
    }
}
