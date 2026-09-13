use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON-RPC message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent closed stdout")]
    EndOfStream,
}

/// Writes one JSON-RPC message using newline-delimited JSON framing.
///
/// # Errors
///
/// Returns an error when serialization or writing fails.
pub async fn write_jsonl<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &JsonRpcMessage,
) -> Result<(), TransportError> {
    let mut encoded = serde_json::to_vec(message)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one newline-delimited JSON-RPC message.
///
/// # Errors
///
/// Returns an error for invalid JSON, an I/O failure or a closed stream.
pub async fn read_jsonl<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<JsonRpcMessage, TransportError> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(TransportError::EndOfStream);
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn jsonl_round_trip() {
        let expected = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: Some("initialize".into()),
            params: Some(json!({"protocolVersion": 1})),
            result: None,
            error: None,
        };
        let (mut writer, reader) = tokio::io::duplex(1024);
        write_jsonl(&mut writer, &expected).await.unwrap();
        let mut reader = BufReader::new(reader);
        assert_eq!(read_jsonl(&mut reader).await.unwrap(), expected);
    }
}
