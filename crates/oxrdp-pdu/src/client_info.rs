use crate::error::EncodeResult;

/// UTF-16LE bytes of `s` (no null terminator).
fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Flag constants for the `flags` field of [`ClientInfo`].
pub mod info_flag {
    pub const MOUSE: u32 = 0x0000_0001;
    pub const DISABLE_CTRL_ALT_DEL: u32 = 0x0000_0002;
    pub const UNICODE: u32 = 0x0000_0010;
    pub const MAXIMIZE_SHELL: u32 = 0x0000_0020;
    pub const LOGON_NOTIFY: u32 = 0x0000_0040;
    pub const ENABLE_WINDOWS_KEY: u32 = 0x0000_0100;
}

/// Extended client information (TS_EXTENDED_INFO_PACKET).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedInfo {
    /// Client network address, e.g. `"0.0.0.0"`.
    pub client_address: String,
    /// Client system directory path.
    pub client_dir: String,
    /// Time zone information (TS_TIME_ZONE_INFORMATION). Zeroed is acceptable.
    pub time_zone: [u8; 172],
    /// Session identifier.
    pub session_id: u32,
    /// Performance flags.
    pub performance_flags: u32,
}

impl Default for ExtendedInfo {
    fn default() -> Self {
        Self {
            client_address: "0.0.0.0".into(),
            client_dir: String::new(),
            time_zone: [0u8; 172],
            session_id: 0,
            performance_flags: 0,
        }
    }
}

/// RDP Client Info PDU payload (TS_INFO_PACKET).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Client code page.
    pub code_page: u32,
    /// Information flags; see [`info_flag`].
    pub flags: u32,
    /// Logon domain.
    pub domain: String,
    /// Logon username.
    pub username: String,
    /// Logon password.
    pub password: String,
    /// Alternate shell.
    pub alternate_shell: String,
    /// Working directory.
    pub working_dir: String,
    /// Extended client information.
    pub extended: ExtendedInfo,
}

impl ClientInfo {
    /// Encode this packet into its wire representation.
    pub fn to_bytes(&self) -> EncodeResult<Vec<u8>> {
        let mut out = Vec::new();

        // 1. Header fields.
        out.extend_from_slice(&self.code_page.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());

        // 3. String byte counts (without null terminator).
        let domain_bytes = utf16le(&self.domain);
        let username_bytes = utf16le(&self.username);
        let password_bytes = utf16le(&self.password);
        let alternate_shell_bytes = utf16le(&self.alternate_shell);
        let working_dir_bytes = utf16le(&self.working_dir);

        out.extend_from_slice(&(domain_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(username_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(password_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(alternate_shell_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(working_dir_bytes.len() as u16).to_le_bytes());

        // 4. String fields with null terminators.
        out.extend_from_slice(&domain_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&username_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&password_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&alternate_shell_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&working_dir_bytes);
        out.extend_from_slice(&[0x00, 0x00]);

        // 5. Extended info.
        let client_address_bytes = utf16le(&self.extended.client_address);
        let client_dir_bytes = utf16le(&self.extended.client_dir);

        out.extend_from_slice(&0x0002u16.to_le_bytes()); // clientAddressFamily = AF_INET
        out.extend_from_slice(&((client_address_bytes.len() + 2) as u16).to_le_bytes());
        out.extend_from_slice(&client_address_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&((client_dir_bytes.len() + 2) as u16).to_le_bytes());
        out.extend_from_slice(&client_dir_bytes);
        out.extend_from_slice(&[0x00, 0x00]);
        out.extend_from_slice(&self.extended.time_zone);
        out.extend_from_slice(&self.extended.session_id.to_le_bytes());
        out.extend_from_slice(&self.extended.performance_flags.to_le_bytes());

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ClientInfo {
        ClientInfo {
            code_page: 0,
            flags: info_flag::UNICODE | info_flag::MOUSE,
            domain: String::new(),
            username: "u".into(),
            password: "pw".into(),
            alternate_shell: String::new(),
            working_dir: String::new(),
            extended: ExtendedInfo::default(),
        }
    }

    #[test]
    fn header_fields() {
        let b = sample().to_bytes().unwrap();
        assert_eq!(&b[0..4], &0u32.to_le_bytes()); // code_page
        assert_eq!(
            &b[4..8],
            &(info_flag::UNICODE | info_flag::MOUSE).to_le_bytes()
        ); // flags
        assert_eq!(&b[8..10], &0u16.to_le_bytes()); // cbDomain (empty)
        assert_eq!(&b[10..12], &2u16.to_le_bytes()); // cbUserName ("u" = 1 code unit = 2 bytes)
        assert_eq!(&b[12..14], &4u16.to_le_bytes()); // cbPassword ("pw" = 2 code units = 4 bytes)
        assert_eq!(&b[14..16], &0u16.to_le_bytes()); // cbAlternateShell
        assert_eq!(&b[16..18], &0u16.to_le_bytes()); // cbWorkingDir
    }

    #[test]
    fn strings_are_utf16_with_null() {
        let b = sample().to_bytes().unwrap();
        // after the 18-byte fixed header: domain (just null), then username "u" + null.
        // domain empty -> [0,0]; username -> 0x75 0x00 0x00 0x00
        assert_eq!(&b[18..20], &[0x00, 0x00]); // domain null
        assert_eq!(&b[20..24], &[0x75, 0x00, 0x00, 0x00]); // 'u' + null
    }

    #[test]
    fn ends_with_extended_info() {
        let b = sample().to_bytes().unwrap();
        // last 8 bytes = session_id (0) + performance_flags (0)
        assert_eq!(&b[b.len() - 8..], &[0u8; 8]);
        // the 172-byte timezone + those 8 bytes are present near the end
        assert!(b.len() > 172 + 8);
    }
}
