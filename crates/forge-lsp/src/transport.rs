//! Framing transport for Language Server Protocol (Content-Length headers over async I/O).

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Header format error: {0}")]
    HeaderFormat(String),
    #[error("Missing Content-Length header")]
    MissingContentLength,
    #[error("Connection closed (EOF)")]
    Closed,
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Reads a single framed LSP message from an async buffered reader.
///
/// Returns the message payload as raw bytes.
///
/// # Errors
/// Returns transport, serialization, protocol or lifecycle errors.
pub async fn read_message<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, TransportError> {
    let mut content_length: Option<usize> = None;
    let mut header_line = String::new();
    let mut header_bytes = 0;

    loop {
        header_line.clear();
        // Bound even an unterminated header before allocating its full length.
        let bytes_read = (&mut *reader)
            .take(8193)
            .read_line(&mut header_line)
            .await?;
        header_bytes += bytes_read;
        if header_bytes > 8192 {
            return Err(TransportError::HeaderFormat("Headers exceed 8 KiB".into()));
        }
        if bytes_read == 0 {
            // If we haven't read any header lines yet, it's a clean EOF
            return Err(TransportError::Closed);
        }

        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line marks the end of headers
            break;
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                let len = value.parse::<usize>().map_err(|e| {
                    TransportError::HeaderFormat(format!("Invalid Content-Length: {e}"))
                })?;
                if content_length.replace(len).is_some() || len > 16 * 1024 * 1024 {
                    return Err(TransportError::HeaderFormat(
                        "Duplicate or oversized Content-Length".into(),
                    ));
                }
            }
            // Other headers such as Content-Type are accepted and ignored according to LSP spec
        } else {
            return Err(TransportError::HeaderFormat(format!(
                "Malformed header line: {trimmed:?}"
            )));
        }
    }

    let len = content_length.ok_or(TransportError::MissingContentLength)?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Writes a single framed LSP message to an async writer with Content-Length header.
///
/// # Errors
/// Returns transport, serialization, protocol or lifecycle errors.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), TransportError> {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{BufReader, duplex};

    #[tokio::test]
    async fn test_roundtrip_message() {
        let (client, mut server) = duplex(1024);
        let mut reader = BufReader::new(client);
        let message = b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\"}";

        write_message(&mut server, message).await.unwrap();
        let received = read_message(&mut reader).await.unwrap();
        assert_eq!(received, message);
    }

    #[tokio::test]
    async fn test_multiple_messages_with_custom_headers() {
        let (client, mut server) = duplex(2048);
        let mut reader = BufReader::new(client);

        let msg1 = b"{\"id\":1}";
        let msg2 = b"{\"id\":2}";

        // Write first message with extra header and mixed case
        let raw1 = format!(
            "content-TYPE: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            msg1.len(),
            std::str::from_utf8(msg1).unwrap()
        );
        server.write_all(raw1.as_bytes()).await.unwrap();
        server.flush().await.unwrap();

        // Write second message normal
        write_message(&mut server, msg2).await.unwrap();

        let recv1 = read_message(&mut reader).await.unwrap();
        assert_eq!(recv1, msg1);

        let recv2 = read_message(&mut reader).await.unwrap();
        assert_eq!(recv2, msg2);
    }

    #[tokio::test]
    async fn test_eof_handling() {
        let (client, server) = duplex(1024);
        let mut reader = BufReader::new(client);
        drop(server);

        let res = read_message(&mut reader).await;
        assert!(matches!(res, Err(TransportError::Closed)));
    }

    #[tokio::test]
    async fn test_missing_content_length() {
        let (client, mut server) = duplex(1024);
        let mut reader = BufReader::new(client);

        server
            .write_all(b"Content-Type: text/plain\r\n\r\n")
            .await
            .unwrap();
        server.flush().await.unwrap();

        let res = read_message(&mut reader).await;
        assert!(matches!(res, Err(TransportError::MissingContentLength)));
    }
}
