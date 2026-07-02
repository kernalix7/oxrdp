use crate::codec::Decode;
use crate::cursor::ReadCursor;
use crate::error::DecodeResult;

pub mod license_msg {
    pub const LICENSE_REQUEST: u8 = 0x01;
    pub const PLATFORM_CHALLENGE: u8 = 0x02;
    pub const NEW_LICENSE: u8 = 0x03;
    pub const UPGRADE_LICENSE: u8 = 0x04;
    pub const LICENSE_INFO: u8 = 0x12;
    pub const NEW_LICENSE_REQUEST: u8 = 0x13;
    pub const PLATFORM_CHALLENGE_RESPONSE: u8 = 0x15;
    pub const ERROR_ALERT: u8 = 0xFF;
}

/// LICENSE_ERROR dwErrorCode meaning "no license required — proceed".
pub const STATUS_VALID_CLIENT: u32 = 0x0000_0007;

/// Parses the RDP licensing PDU (MS-RDPELE) preamble and, for error alerts,
/// extracts the error code. This is enough to detect the common
/// "proceed without a license" path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LicensePdu {
    pub msg_type: u8,
    pub flags: u8,
    /// Present only for an ERROR_ALERT message: the `dwErrorCode`.
    pub error_code: Option<u32>,
}

impl LicensePdu {
    /// True when the server signalled the client may proceed without licensing
    /// (an ERROR_ALERT carrying STATUS_VALID_CLIENT).
    pub fn is_proceed(&self) -> bool {
        self.msg_type == license_msg::ERROR_ALERT && self.error_code == Some(STATUS_VALID_CLIENT)
    }
}

impl<'de> Decode<'de> for LicensePdu {
    fn decode(cursor: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let msg_type = cursor.read_u8("license bMsgType")?;
        let flags = cursor.read_u8("license flags")?;
        let _msg_size = cursor.read_u16_le("license wMsgSize")?;

        let error_code = if msg_type == license_msg::ERROR_ALERT {
            Some(cursor.read_u32_le("license dwErrorCode")?)
        } else {
            None
        };

        Ok(LicensePdu {
            msg_type,
            flags,
            error_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode;

    #[test]
    fn valid_client_proceeds() {
        // ERROR_ALERT (0xFF), flags 0x03, wMsgSize 0x0010, dwErrorCode = STATUS_VALID_CLIENT (7),
        // then dwStateTransition + empty blob (ignored).
        let bytes = [
            0xFF, 0x03, 0x10, 0x00, 0x07, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x04, 0x00,
            0x00, 0x00,
        ];
        let pdu = decode::<LicensePdu>(&bytes).unwrap();
        assert_eq!(pdu.msg_type, license_msg::ERROR_ALERT);
        assert_eq!(pdu.error_code, Some(STATUS_VALID_CLIENT));
        assert!(pdu.is_proceed());
    }

    #[test]
    fn license_request_is_not_proceed() {
        let bytes = [0x01, 0x03, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
        let pdu = decode::<LicensePdu>(&bytes).unwrap();
        assert_eq!(pdu.msg_type, license_msg::LICENSE_REQUEST);
        assert_eq!(pdu.error_code, None);
        assert!(!pdu.is_proceed());
    }
}
