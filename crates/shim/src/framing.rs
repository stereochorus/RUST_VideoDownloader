//! Native-messaging framing: a 4-byte message length in native byte order (little-endian on
//! every Windows target), followed by that many bytes of UTF-8 JSON.

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Chrome refuses host → browser messages larger than 1 MB.
pub const MAX_TO_BROWSER: usize = 1024 * 1024;
/// Our own sanity cap for browser → host (Chrome allows up to 4 GB; we never need that).
const MAX_FROM_BROWSER: usize = 8 * 1024 * 1024;

/// `Ok(None)` on clean EOF before a new message (the browser closed the port).
pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_FROM_BROWSER {
        bail!("incoming message of {len} bytes exceeds limit");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &[u8]) -> Result<()> {
    if msg.len() > MAX_TO_BROWSER {
        bail!("outgoing message of {} bytes exceeds Chrome's 1 MB limit", msg.len());
    }
    w.write_all(&(msg.len() as u32).to_ne_bytes()).await?;
    w.write_all(msg).await?;
    w.flush().await?; // Chrome reads frame by frame; an unflushed frame looks like a hang
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_two_messages_then_eof() {
        let mut wire = Vec::new();
        write_msg(&mut wire, br#"{"cmd":"ping"}"#).await.unwrap();
        write_msg(&mut wire, b"{}").await.unwrap();
        assert_eq!(&wire[..4], &14u32.to_le_bytes());

        let mut r = &wire[..];
        assert_eq!(read_msg(&mut r).await.unwrap().unwrap(), br#"{"cmd":"ping"}"#);
        assert_eq!(read_msg(&mut r).await.unwrap().unwrap(), b"{}");
        assert!(read_msg(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn truncated_body_is_an_error() {
        let mut wire = 10u32.to_ne_bytes().to_vec();
        wire.extend_from_slice(b"abc");
        assert!(read_msg(&mut &wire[..]).await.is_err());
    }

    #[tokio::test]
    async fn oversized_outgoing_rejected() {
        let big = vec![b'x'; MAX_TO_BROWSER + 1];
        assert!(write_msg(&mut Vec::new(), &big).await.is_err());
    }
}
