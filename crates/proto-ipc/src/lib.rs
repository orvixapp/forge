use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const HEADER_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Request = 1,
    Response = 2,
    Notification = 3,
    StreamItem = 4,
    Credit = 5,
    Cancel = 6,
}

impl TryFrom<u8> for FrameKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Response),
            3 => Ok(Self::Notification),
            4 => Ok(Self::StreamItem),
            5 => Ok(Self::Credit),
            6 => Ok(Self::Cancel),
            other => Err(ProtocolError::UnknownFrameKind(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub flags: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("MessagePack error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("MessagePack error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("frame is too large: {actual} bytes (maximum {maximum})")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("unknown frame kind {0}")]
    UnknownFrameKind(u8),
    #[error("reserved header bits must be zero")]
    ReservedBits,
}

/// Writes one bounded, length-prefixed frame.
///
/// # Errors
///
/// Returns an error when the payload exceeds [`MAX_FRAME_BYTES`] or when the
/// underlying writer cannot accept the complete frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), ProtocolError> {
    if frame.payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: frame.payload.len(),
            maximum: MAX_FRAME_BYTES,
        });
    }

    let mut header = [0_u8; HEADER_BYTES];
    let payload_len =
        u32::try_from(frame.payload.len()).map_err(|_| ProtocolError::FrameTooLarge {
            actual: frame.payload.len(),
            maximum: MAX_FRAME_BYTES,
        })?;
    header[..4].copy_from_slice(&payload_len.to_le_bytes());
    header[4] = frame.kind as u8;
    header[5] = frame.flags;
    writer.write_all(&header).await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads and validates one bounded, length-prefixed frame.
///
/// # Errors
///
/// Returns an error for malformed headers, oversized frames, unknown frame
/// kinds, truncated input or another I/O failure.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, ProtocolError> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).await?;
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if payload_len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: payload_len,
            maximum: MAX_FRAME_BYTES,
        });
    }
    if header[6] != 0 || header[7] != 0 {
        return Err(ProtocolError::ReservedBits);
    }
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload).await?;
    Ok(Frame {
        kind: FrameKind::try_from(header[4])?,
        flags: header[5],
        payload,
    })
}

/// Serializes a typed `MessagePack` value and writes it as a frame.
///
/// # Errors
///
/// Returns an error when serialization fails or the frame cannot be written.
pub async fn write_message<W, T>(
    writer: &mut W,
    kind: FrameKind,
    message: &T,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = rmp_serde::to_vec_named(message)?;
    write_frame(
        writer,
        &Frame {
            kind,
            flags: 0,
            payload,
        },
    )
    .await
}

/// Reads one frame and deserializes its `MessagePack` payload.
///
/// # Errors
///
/// Returns an error when framing, I/O or deserialization fails.
pub async fn read_message<R, T>(reader: &mut R) -> Result<(FrameKind, T), ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let frame = read_frame(reader).await?;
    Ok((frame.kind, rmp_serde::from_slice(&frame.payload)?))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Initialize {
        protocol_version: u16,
        client_name: String,
    },
    CreateSession {
        request_id: u64,
        command: String,
        args: Vec<String>,
        cols: u16,
        rows: u16,
    },
    Attach {
        session_id: u64,
    },
    Detach {
        session_id: u64,
    },
    Input {
        session_id: u64,
        data: Vec<u8>,
    },
    Resize {
        session_id: u64,
        cols: u16,
        rows: u16,
    },
    ShutdownSession {
        session_id: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Initialized {
        protocol_version: u16,
    },
    SessionCreated {
        request_id: u64,
        session_id: u64,
    },
    Attached {
        session_id: u64,
        backlog: Vec<u8>,
    },
    Output {
        session_id: u64,
        data: Vec<u8>,
    },
    ScreenPatch {
        session_id: u64,
        revision: u64,
        cols: u16,
        rows: u16,
        full: bool,
        dirty_rows: Vec<ScreenRow>,
        cursor: Option<ScreenCursor>,
    },
    Exited {
        session_id: u64,
        exit_code: Option<u32>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenRow {
    pub y: u16,
    pub cells: Vec<ScreenCell>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenCell {
    /// Complete UTF-8 grapheme for this terminal cell; empty means blank.
    pub text: String,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub styled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenCursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub blinking: bool,
    pub style: CursorStyle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    Bar,
    Block,
    Underline,
    HollowBlock,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn message_round_trip() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let expected = ClientMessage::Input {
            session_id: 7,
            data: b"hello".to_vec(),
        };

        write_message(&mut writer, FrameKind::Request, &expected)
            .await
            .unwrap();
        let (kind, actual) = read_message::<_, ClientMessage>(&mut reader).await.unwrap();

        assert_eq!(kind, FrameKind::Request);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn rejects_reserved_bits() {
        let mut input: &[u8] = &[0, 0, 0, 0, 1, 0, 1, 0];
        assert!(matches!(
            read_frame(&mut input).await,
            Err(ProtocolError::ReservedBits)
        ));
    }
}
