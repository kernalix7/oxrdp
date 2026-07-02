use oxproto::Message;
use oxrdp_pdu::encode_vec;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum accepted message size (guards against a hostile/garbled length). 64 MiB.
pub const MAX_MESSAGE_LEN: u32 = 64 * 1024 * 1024;

/// Write one oxproto message frame (envelope + payload) and flush.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message<'_>,
) -> io::Result<()> {
    let bytes =
        encode_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Read one whole oxproto message frame into `buf` (cleared first), returning the full frame
/// bytes (5-byte header + payload). The caller decodes with `oxproto::decode::<Message>(buf)`
/// — the frame is returned as bytes (not a decoded `Message`) because `Message` may borrow
/// from the buffer.
pub async fn read_message_bytes<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    if payload_len > MAX_MESSAGE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oxproto message too large",
        ));
    }
    buf.clear();
    buf.extend_from_slice(&header);
    buf.resize(5 + payload_len as usize, 0);
    reader.read_exact(&mut buf[5..]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::{ClientHello, FrameData, ServerHello};

    #[tokio::test]
    async fn round_trips_messages() {
        let (mut a, mut b) = tokio::io::duplex(1024);

        let hello = Message::ClientHello(ClientHello {
            version: 1,
            screen_width: 800,
            screen_height: 600,
            codecs: 1,
        });
        write_message(&mut a, &hello).await.unwrap();
        let payload = [1u8, 2, 3, 4, 5];
        let frame = Message::FrameData(FrameData {
            window_id: 9,
            codec: 0,
            keyframe: true,
            timestamp: 42,
            data: &payload,
        });
        write_message(&mut a, &frame).await.unwrap();

        let mut buf = Vec::new();
        read_message_bytes(&mut b, &mut buf).await.unwrap();
        assert_eq!(oxproto::decode::<Message>(&buf).unwrap(), hello);
        read_message_bytes(&mut b, &mut buf).await.unwrap();
        assert_eq!(oxproto::decode::<Message>(&buf).unwrap(), frame);
    }

    #[tokio::test]
    async fn server_hello_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(256);
        let m = Message::ServerHello(ServerHello {
            version: 1,
            codec: 1,
            session_id: 7,
        });
        write_message(&mut a, &m).await.unwrap();
        let mut buf = Vec::new();
        read_message_bytes(&mut b, &mut buf).await.unwrap();
        assert_eq!(oxproto::decode::<Message>(&buf).unwrap(), m);
    }
}
