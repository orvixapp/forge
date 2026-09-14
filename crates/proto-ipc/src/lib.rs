use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 7;
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

/// Validated frame header: payload length, kind and flags.
fn parse_header(header: [u8; HEADER_BYTES]) -> Result<(usize, FrameKind, u8), ProtocolError> {
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
    Ok((payload_len, FrameKind::try_from(header[4])?, header[5]))
}

/// Reads and validates one bounded, length-prefixed frame.
///
/// Not cancellation-safe: it issues several reads, so dropping the future
/// mid-frame (for example from a `select!` arm) loses the bytes already
/// consumed and desynchronizes the stream. Use [`FrameReader`] wherever the
/// read can be cancelled.
///
/// # Errors
///
/// Returns an error for malformed headers, oversized frames, unknown frame
/// kinds, truncated input or another I/O failure.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, ProtocolError> {
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).await?;
    let (payload_len, kind, flags) = parse_header(header)?;
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload).await?;
    Ok(Frame {
        kind,
        flags,
        payload,
    })
}

/// Buffered frame decoder whose reads are cancellation-safe: bytes are
/// appended to an internal buffer with single `read` calls and a frame is
/// only taken out once it is complete, so a future dropped between polls
/// never loses stream position.
pub struct FrameReader<R> {
    reader: R,
    buffer: Vec<u8>,
    chunk: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    const CHUNK_BYTES: usize = 64 * 1024;

    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            chunk: vec![0; Self::CHUNK_BYTES].into_boxed_slice(),
        }
    }

    /// Reads the next complete frame. Safe to use inside `select!`.
    ///
    /// # Errors
    ///
    /// Same conditions as [`read_frame`]; a clean end of stream between
    /// frames is reported as an `UnexpectedEof` I/O error.
    pub async fn read_frame(&mut self) -> Result<Frame, ProtocolError> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(frame);
            }
            let count = self.reader.read(&mut self.chunk).await?;
            if count == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.buffer.extend_from_slice(&self.chunk[..count]);
        }
    }

    /// Reads the next frame and deserializes its `MessagePack` payload.
    ///
    /// # Errors
    ///
    /// Returns an error when framing, I/O or deserialization fails.
    pub async fn read_message<T: DeserializeOwned>(
        &mut self,
    ) -> Result<(FrameKind, T), ProtocolError> {
        let frame = self.read_frame().await?;
        Ok((frame.kind, rmp_serde::from_slice(&frame.payload)?))
    }

    fn take_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        if self.buffer.len() < HEADER_BYTES {
            return Ok(None);
        }
        let mut header = [0_u8; HEADER_BYTES];
        header.copy_from_slice(&self.buffer[..HEADER_BYTES]);
        let (payload_len, kind, flags) = parse_header(header)?;
        let frame_len = HEADER_BYTES + payload_len;
        if self.buffer.len() < frame_len {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_BYTES..frame_len].to_vec();
        self.buffer.drain(..frame_len);
        Ok(Some(Frame {
            kind,
            flags,
            payload,
        }))
    }
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
        /// Directory in which the shell must start.
        cwd: std::path::PathBuf,
        cols: u16,
        rows: u16,
        /// Extra environment for the program (shell integration, `TERM_PROGRAM`).
        #[serde(default)]
        env: Vec<(String, String)>,
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
    /// A key press to encode according to the terminal's current modes
    /// (cursor keys, keypad, Kitty keyboard protocol). Encoding happens in
    /// the daemon because that is where the modes live.
    Key {
        session_id: u64,
        event: KeyEvent,
    },
    /// A mouse event in cell coordinates, forwarded only while the
    /// application has mouse tracking enabled.
    Mouse {
        session_id: u64,
        event: MouseEvent,
    },
    /// Text to paste; the daemon applies bracketed paste when the mode is on.
    Paste {
        session_id: u64,
        text: String,
    },
    /// Moves the viewport over the scrollback.
    Scroll {
        session_id: u64,
        scroll: ScrollRequest,
    },
    /// Searches text represented by the daemon-owned VT scrollback.
    Search {
        session_id: u64,
        request_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
    },
    /// Sends a POSIX signal to the session's process group.
    Signal {
        session_id: u64,
        signal: ProcessSignal,
    },
    /// Moves the viewport to the previous/next OSC 133 prompt row.
    ScrollToPrompt {
        session_id: u64,
        direction: PromptDirection,
    },
    /// Sessions the daemon still holds, so a restarted client can reattach.
    ListSessions,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    /// SIGINT, what Ctrl+C sends.
    Interrupt,
    /// SIGTERM.
    Terminate,
    /// SIGKILL.
    Kill,
    /// SIGHUP.
    Hangup,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptDirection {
    /// Towards older rows.
    Previous,
    Next,
}

/// Destination of a clipboard write requested by the application.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardTarget {
    Clipboard,
    /// X11/Wayland primary selection.
    Primary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: u64,
    pub title: String,
    pub pwd: Option<String>,
    /// `false` once the program exited; the last screen is still readable.
    pub alive: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct KeyMods {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Physical key, named after the W3C `KeyboardEvent.code` values that
/// libghostty-vt understands. `Unidentified` with `text` still types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum TerminalKey {
    Unidentified,
    Backquote,
    Backslash,
    BracketLeft,
    BracketRight,
    Comma,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Equal,
    IntlBackslash,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Minus,
    Period,
    Quote,
    Semicolon,
    Slash,
    AltLeft,
    AltRight,
    Backspace,
    CapsLock,
    ContextMenu,
    ControlLeft,
    ControlRight,
    Enter,
    MetaLeft,
    MetaRight,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    Delete,
    End,
    Home,
    Insert,
    PageDown,
    PageUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadDecimal,
    NumpadDivide,
    NumpadEnter,
    NumpadEqual,
    NumpadMultiply,
    NumpadSubtract,
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    PrintScreen,
    ScrollLock,
    Pause,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyEvent {
    pub action: KeyAction,
    pub key: TerminalKey,
    pub mods: KeyMods,
    /// Text the key produces with the current layout, if any.
    pub text: Option<String>,
    /// Codepoint of the key without shift/altgr, for Kitty's protocol.
    pub unshifted_codepoint: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Buttons in xterm numbering; `WheelUp`/`WheelDown` are buttons 4 and 5.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// `None` for motion without a pressed button.
    pub button: Option<MouseButton>,
    pub mods: KeyMods,
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScrollRequest {
    Top,
    Bottom,
    /// Rows; negative scrolls up into the scrollback.
    Delta(i64),
    /// Absolute row from the top of the scrollback.
    Row(u64),
}

/// Position of the viewport inside the scrollable area, in rows.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Viewport {
    /// Scrollback rows plus the active screen.
    pub total: u64,
    /// First visible row.
    pub offset: u64,
    /// Visible rows.
    pub len: u64,
}

impl Viewport {
    /// Whether the viewport is scrolled away from the live screen.
    #[must_use]
    pub fn scrolled_back(&self) -> bool {
        self.offset + self.len < self.total
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Initialized {
        protocol_version: u16,
        /// Changes every time the daemon process starts. Session IDs are only
        /// meaningful inside this namespace.
        daemon_instance: u64,
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
        #[serde(default)]
        viewport: Viewport,
    },
    /// Answer to `ListSessions`.
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    /// Slow-changing session state, sent on attach and whenever it changes.
    SessionInfo {
        session_id: u64,
        /// Title set by OSC 0/2; empty when the application set none.
        title: String,
        /// Working directory reported by OSC 7 (decoded from its URI).
        pwd: Option<String>,
        /// The application wants mouse events (modes 9/1000/1002/1003).
        mouse_tracking: bool,
        alternate_screen: bool,
        /// Mode 2004: pasted newlines are wrapped, not executed.
        #[serde(default)]
        bracketed_paste: bool,
    },
    /// The application asked to write the clipboard (OSC 52, OSC 1337,
    /// OSC 5522). The client decides whether to honour it.
    ClipboardWrite {
        session_id: u64,
        target: ClipboardTarget,
        text: String,
        /// Program name when the protocol carries one; empty otherwise.
        program: String,
    },
    /// Answer to `Search`; `matches` are ordered from the oldest row to the
    /// newest. `error` reports an invalid pattern instead of a daemon error,
    /// so the search bar can show it inline.
    SearchResults {
        session_id: u64,
        request_id: u64,
        query: String,
        matches: Vec<SearchMatch>,
        #[serde(default)]
        error: Option<String>,
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
pub struct SearchMatch {
    /// Absolute row from the beginning of the retained scrollback, in the
    /// same row space as [`Viewport::offset`].
    pub row: u64,
    /// First column of the match.
    pub start: u16,
    /// Column after the last one of the match.
    pub end: u16,
    /// The matched row's text, trimmed, for result lists.
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenRow {
    pub y: u16,
    pub cells: Vec<ScreenCell>,
    /// OSC 133 mark: 0 none, 1 prompt line, 2 prompt continuation.
    #[serde(default)]
    pub prompt: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenCell {
    /// Complete UTF-8 grapheme for this terminal cell; empty means blank.
    pub text: String,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub styled: bool,
    #[serde(default)]
    pub style: CellStyle,
    /// OSC 8 hyperlink target, when the application attached one.
    #[serde(default)]
    pub hyperlink: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    /// Ghostty SGR underline: 0 none, 1 single, 2 double, 3 curly,
    /// 4 dotted, 5 dashed.
    pub underline: u8,
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
    async fn buffered_reader_survives_cancellation_mid_frame() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut frames = FrameReader::new(reader);
        let expected = ClientMessage::Input {
            session_id: 7,
            data: vec![b'x'; 300],
        };
        let mut encoded = Vec::new();
        write_message(&mut encoded, FrameKind::Notification, &expected)
            .await
            .unwrap();

        // Deliver the first half only, then cancel a read that is waiting
        // for the rest, as a `select!` arm would.
        let (head, tail) = encoded.split_at(100);
        writer.write_all(head).await.unwrap();
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            frames.read_message::<ClientMessage>(),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "read must still be waiting for the tail"
        );

        writer.write_all(tail).await.unwrap();
        // A second frame right behind exercises the buffered remainder.
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Detach { session_id: 1 },
        )
        .await
        .unwrap();

        let (kind, first) = frames.read_message::<ClientMessage>().await.unwrap();
        assert_eq!((kind, first), (FrameKind::Notification, expected));
        let (kind, second) = frames.read_message::<ClientMessage>().await.unwrap();
        assert_eq!(
            (kind, second),
            (FrameKind::Request, ClientMessage::Detach { session_id: 1 })
        );
        drop(writer);
        assert!(matches!(
            frames.read_frame().await,
            Err(ProtocolError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[tokio::test]
    async fn buffered_reader_rejects_oversized_frames_before_buffering_them() {
        let mut input: &[u8] = &[0xff, 0xff, 0xff, 0x7f, 1, 0, 0, 0];
        let mut frames = FrameReader::new(&mut input);
        assert!(matches!(
            frames.read_frame().await,
            Err(ProtocolError::FrameTooLarge { .. })
        ));
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
