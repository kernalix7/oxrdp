//! `oxrdp-cli` — thin binary that runs the RDP connection sequence.
//!
//! Pre-alpha: this drives the transport through the MCS channel-join phase and reports the
//! negotiated channels. The post-connection phases (security/licensing/capabilities,
//! graphics, RAIL) are not implemented yet. winpodx will later spawn this binary and drive
//! it over a control channel; for now it is a manual connectivity probe.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use oxrdp_core::ConnectorConfig;
use oxrdp_io::connect;
use oxrdp_pdu::gcc::{ChannelDef, ClientCoreData, ClientNetworkData, ClientSecurityData};
use oxrdp_pdu::nego::protocol;

/// Encode a client name as UTF-16LE, null-padded to the fixed 32-byte field.
fn client_name(name: &str) -> [u8; 32] {
    let mut buf = [0u8; 32];
    for (i, unit) in name.encode_utf16().take(15).enumerate() {
        let bytes = unit.to_le_bytes();
        buf[i * 2] = bytes[0];
        buf[i * 2 + 1] = bytes[1];
    }
    buf
}

/// A sensible default client configuration for the connection sequence.
fn default_config(username: &str) -> ConnectorConfig {
    ConnectorConfig {
        cookie: (!username.is_empty()).then(|| username.to_string()),
        requested_protocols: protocol::SSL,
        core: ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1024,
            desktop_height: 768,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0000_0409,
            client_build: 2600,
            client_name: client_name("oxrdp"),
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

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("oxrdp");
    if args.len() < 2 {
        eprintln!("usage: {prog} <host[:port]> [username]");
        eprintln!("  runs the RDP connection sequence through MCS channel join (pre-alpha: no graphics yet).");
        return ExitCode::from(2);
    }

    let host = &args[1];
    let addr = if host.contains(':') {
        host.clone()
    } else {
        format!("{host}:3389")
    };
    let username = args.get(2).map(String::as_str).unwrap_or("");
    let server_name = host.split(':').next().unwrap_or(host);

    eprintln!(
        "oxrdp {}: connecting to {addr} ...",
        env!("CARGO_PKG_VERSION")
    );
    match connect(&addr, server_name, default_config(username)).await {
        Ok(session) => {
            println!(
                "connected: protocol=0x{:08x} user_channel={} io_channel={}",
                session.selected_protocol, session.user_channel, session.io_channel
            );
            eprintln!(
                "(MCS channel join complete — post-connection phases are not implemented yet.)"
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("connection failed: {err}");
            ExitCode::FAILURE
        }
    }
}
