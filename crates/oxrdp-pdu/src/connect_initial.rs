use crate::codec::encode_vec;
use crate::domain_params::DomainParameters;
use crate::error::EncodeResult;
use crate::gcc::{ClientCoreData, ClientNetworkData, ClientSecurityData};

/// PER length used inside the GCC structure.
fn per_append_length(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        out.push(0x80 | (len >> 8) as u8);
        out.push((len & 0xFF) as u8);
    }
}

/// BER definite length used in the MCS Connect-Initial envelope.
fn ber_append_length(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let mut bytes = Vec::new();
        let mut v = len;
        while v > 0 {
            bytes.insert(0, (v & 0xFF) as u8);
            v >>= 8;
        }
        out.push(0x80 | bytes.len() as u8);
        out.extend_from_slice(&bytes);
    }
}

/// MCS Connect-Initial PDU wrapping a GCC Conference Create Request.
#[derive(Debug, Clone)]
pub struct ConnectInitial {
    /// Client core data block.
    pub core: ClientCoreData,
    /// Client security data block.
    pub security: ClientSecurityData,
    /// Client network data block.
    pub network: ClientNetworkData,
}

impl ConnectInitial {
    /// Encode the Connect-Initial PDU to bytes.
    pub fn to_bytes(&self) -> EncodeResult<Vec<u8>> {
        // 1. user data = the three client blocks concatenated
        let mut user_data = Vec::new();
        user_data.extend_from_slice(&encode_vec(&self.core)?);
        user_data.extend_from_slice(&encode_vec(&self.security)?);
        user_data.extend_from_slice(&encode_vec(&self.network)?);
        let udlen = user_data.len();

        // 2. GCC Conference Create Request (PER)
        let mut gcc = Vec::new();
        gcc.extend_from_slice(&[0x00, 0x05, 0x00, 0x14, 0x7C, 0x00, 0x01]);
        per_append_length(&mut gcc, udlen + 14);
        gcc.extend_from_slice(&[0x00, 0x08, 0x00, 0x10, 0x00, 0x01, 0xC0, 0x00]);
        gcc.extend_from_slice(b"Duca");
        per_append_length(&mut gcc, udlen);
        gcc.extend_from_slice(&user_data);

        // 3. MCS Connect-Initial body (BER fields)
        let mut body = Vec::new();
        body.extend_from_slice(&[0x04, 0x01, 0x01]);
        body.extend_from_slice(&[0x04, 0x01, 0x01]);
        body.extend_from_slice(&[0x01, 0x01, 0xFF]);
        body.extend_from_slice(&encode_vec(&DomainParameters::target())?);
        body.extend_from_slice(&encode_vec(&DomainParameters::minimum())?);
        body.extend_from_slice(&encode_vec(&DomainParameters::maximum())?);
        body.push(0x04);
        ber_append_length(&mut body, gcc.len());
        body.extend_from_slice(&gcc);

        // 4. wrap in the Connect-Initial application tag [APPLICATION 101]
        let mut out = Vec::new();
        out.extend_from_slice(&[0x7F, 0x65]);
        ber_append_length(&mut out, body.len());
        out.extend_from_slice(&body);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcc::ChannelDef;

    fn sample() -> ConnectInitial {
        let mut name = [0u8; 32];
        name[..5].copy_from_slice(b"oxrdp");
        ConnectInitial {
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
                    options: 0x8080_0000,
                }],
            },
        }
    }

    #[test]
    fn starts_with_connect_initial_tag() {
        let b = sample().to_bytes().unwrap();
        assert_eq!(&b[..2], &[0x7F, 0x65]);
    }

    #[test]
    fn contains_t124_oid_and_duca() {
        let b = sample().to_bytes().unwrap();
        assert!(
            b.windows(7)
                .any(|w| w == [0x00, 0x05, 0x00, 0x14, 0x7C, 0x00, 0x01]),
            "t124 OID"
        );
        assert!(b.windows(4).any(|w| w == *b"Duca"), "h221 Duca key");
    }

    #[test]
    fn ends_with_the_client_blocks() {
        let ci = sample();
        let b = ci.to_bytes().unwrap();
        let mut blocks = Vec::new();
        blocks.extend_from_slice(&encode_vec(&ci.core).unwrap());
        blocks.extend_from_slice(&encode_vec(&ci.security).unwrap());
        blocks.extend_from_slice(&encode_vec(&ci.network).unwrap());
        assert_eq!(&b[b.len() - blocks.len()..], &blocks[..]);
    }

    #[test]
    fn outer_ber_length_is_consistent() {
        let b = sample().to_bytes().unwrap();
        assert_eq!(b[2] & 0x80, 0x80, "long-form length");
        let n = (b[2] & 0x7F) as usize;
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | b[3 + i] as usize;
        }
        assert_eq!(
            3 + n + len,
            b.len(),
            "outer length covers the rest of the PDU"
        );
    }
}
