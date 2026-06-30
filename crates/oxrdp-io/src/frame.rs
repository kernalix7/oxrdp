use oxrdp_pdu::decode;
use oxrdp_pdu::tpkt::{TpktHeader, TPKT_HEADER_LEN};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one complete TPKT-framed message: read the 4-byte TPKT header, parse its length,
/// then read the remaining bytes. Returns the FULL frame (all `length` bytes, header included).
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; TPKT_HEADER_LEN];
    reader.read_exact(&mut header).await?;
    let tpkt = decode::<TpktHeader>(&header)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let total = tpkt.length as usize;
    if total < TPKT_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TPKT length smaller than header",
        ));
    }
    let mut frame = vec![0u8; total];
    frame[..TPKT_HEADER_LEN].copy_from_slice(&header);
    reader.read_exact(&mut frame[TPKT_HEADER_LEN..]).await?;
    Ok(frame)
}

/// Write a complete frame (already including its TPKT header) and flush.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
    writer.write_all(frame).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_a_frame() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let frame = vec![0x03, 0x00, 0x00, 0x09, 0x02, 0xF0, 0x80, 0xAA, 0xBB];
        write_frame(&mut a, &frame).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, frame);
    }

    #[tokio::test]
    async fn reads_exact_length() {
        // two frames back-to-back; read_frame must return only the first.
        let (mut a, mut b) = tokio::io::duplex(64);
        let f1 = vec![0x03, 0x00, 0x00, 0x07, 0x02, 0xF0, 0x80];
        let f2 = vec![0x03, 0x00, 0x00, 0x08, 0x02, 0xF0, 0x80, 0x11];
        write_frame(&mut a, &f1).await.unwrap();
        write_frame(&mut a, &f2).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), f1);
        assert_eq!(read_frame(&mut b).await.unwrap(), f2);
    }

    #[tokio::test]
    async fn rejects_bad_tpkt() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, &[0x09, 0x00, 0x00, 0x04])
            .await
            .unwrap(); // version != 3
        assert!(read_frame(&mut b).await.is_err());
    }
}
