//! The connection driver: ties TCP + TLS + the sans-io [`ClientConnector`] together.
//!
//! This is the impure seam that actually talks to the network, so it cannot be unit-tested
//! without a live RDP server — it is validated against a real Windows host. The pieces it
//! drives (the connector state machine, the frame codec, the TLS config) are each unit-tested
//! in isolation.

use std::io;

use oxrdp_core::{ClientConnector, ConnectorConfig, CoreError, CoreOutput};
use oxrdp_crypto::tls_client_config;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use crate::frame::{read_frame, write_frame};

/// An established RDP session: the encrypted stream plus the negotiated MCS identifiers.
///
/// All post-connection MCS Send Data PDUs flow over [`Session::stream`].
pub struct Session {
    /// The TLS stream to the server.
    pub stream: TlsStream<TcpStream>,
    /// The security protocol the server selected during negotiation.
    pub selected_protocol: u32,
    /// The MCS user channel assigned to this client.
    pub user_channel: u16,
    /// The MCS I/O channel.
    pub io_channel: u16,
}

fn core_err(e: CoreError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Connect to an RDP server and run the connection sequence through MCS channel join.
///
/// `addr` is the TCP target (e.g. `"192.168.122.10:3389"`); `server_name` is handed to the
/// TLS layer but its identity is not validated (see `oxrdp-crypto`'s `TofuVerifier`).
/// Returns the established [`Session`] once the connector reaches `Connected`.
///
/// Only the TLS security path is supported (winpodx uses `/sec:tls`); a server that selects
/// standard RDP security is rejected.
pub async fn connect(
    addr: &str,
    server_name: &str,
    config: ConnectorConfig,
) -> io::Result<Session> {
    let mut tcp = TcpStream::connect(addr).await?;
    let mut connector = ClientConnector::new(config);

    // Phase 1 — X.224 negotiation over plain TCP.
    for out in connector.start().map_err(core_err)? {
        if let CoreOutput::SendData(bytes) = out {
            write_frame(&mut tcp, &bytes).await?;
        }
    }

    let cc = read_frame(&mut tcp).await?;
    let mut upgrade = false;
    for out in connector.step(&cc).map_err(core_err)? {
        match out {
            CoreOutput::UpgradeToTls => upgrade = true,
            CoreOutput::SendData(bytes) => write_frame(&mut tcp, &bytes).await?,
            CoreOutput::Connected { .. } => {}
        }
    }
    if !upgrade {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "server did not select a TLS-based security protocol",
        ));
    }

    // Phase 2 — TLS upgrade.
    let tls_connector = TlsConnector::from(tls_client_config());
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid server name"))?;
    let mut stream = tls_connector.connect(name, tcp).await?;

    // Phase 3+ — MCS Connect-Initial through channel join, over TLS.
    let mut outputs = connector.resume_after_tls().map_err(core_err)?;
    loop {
        for out in outputs.drain(..) {
            match out {
                CoreOutput::SendData(bytes) => write_frame(&mut stream, &bytes).await?,
                CoreOutput::UpgradeToTls => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected second TLS upgrade",
                    ));
                }
                CoreOutput::Connected {
                    selected_protocol,
                    user_channel,
                    io_channel,
                } => {
                    return Ok(Session {
                        stream,
                        selected_protocol,
                        user_channel,
                        io_channel,
                    });
                }
            }
        }
        let frame = read_frame(&mut stream).await?;
        outputs = connector.step(&frame).map_err(core_err)?;
    }
}
