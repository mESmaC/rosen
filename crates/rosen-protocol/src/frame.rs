//! Length-prefixed framing: a 4-byte big-endian body length, then a UTF-8 JSON body.
//!
//! JSON keeps frames readable in logs and lets old clients skip fields they do not
//! know. The length prefix lets a reader find frame boundaries without parsing.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame body either side accepts. A bigger length prefix means a
/// corrupt or hostile stream, and is refused before any allocation.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Encodes one message as a complete frame.
pub fn encode_frame<T: Serialize>(message: &T) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
        .ok_or_else(|| too_large(body.len() as u64))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one message as a frame and flushes it.
pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Reads one frame body. Returns `None` when the peer closed the stream cleanly
/// between frames; a stream that ends inside a frame is an error.
pub async fn read_frame_body<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(prefix);
    if len > MAX_FRAME_LEN {
        return Err(too_large(u64::from(len)));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Reads and decodes one frame. `None` means a clean end of stream.
pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match read_frame_body(reader).await? {
        Some(body) => decode_body(&body).map(Some),
        None => Ok(None),
    }
}

/// Decodes a frame body read by [`read_frame_body`].
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn too_large(len: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte limit"),
    )
}
