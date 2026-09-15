//! JSON-RPC 2.0 message definitions and serialization for LSP.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LSP error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ResponseError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

impl Message {
    pub fn parse(raw: &[u8]) -> Result<Self, serde_json::Error> {
        let value: Value = serde_json::from_slice(raw)?;
        Self::from_value(value)
    }

    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let has_id = value.get("id").is_some();
        let has_method = value.get("method").is_some();

        if has_method && has_id {
            serde_json::from_value(value).map(Message::Request)
        } else if has_method {
            serde_json::from_value(value).map(Message::Notification)
        } else {
            serde_json::from_value(value).map(Message::Response)
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        match self {
            Message::Request(req) => serde_json::to_vec(req),
            Message::Response(res) => serde_json::to_vec(res),
            Message::Notification(notif) => serde_json::to_vec(notif),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_request() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///a.rs"},"position":{"line":0,"character":5}}}"#;
        let msg = Message::parse(raw).unwrap();
        match msg {
            Message::Request(req) => {
                assert_eq!(req.id, Id::Number(1));
                assert_eq!(req.method, "textDocument/hover");
                assert!(req.params.is_some());
            }
            _ => panic!("Expected request"),
        }
    }

    #[test]
    fn test_parse_response_success() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"result":{"contents":"fn main()"}}"#;
        let msg = Message::parse(raw).unwrap();
        match msg {
            Message::Response(res) => {
                assert_eq!(res.id, Some(Id::Number(1)));
                assert!(res.result.is_some());
                assert!(res.error.is_none());
            }
            _ => panic!("Expected response"),
        }
    }

    #[test]
    fn test_parse_response_error() {
        let raw = br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}"#;
        let msg = Message::parse(raw).unwrap();
        match msg {
            Message::Response(res) => {
                assert_eq!(res.id, Some(Id::Number(2)));
                let err = res.error.unwrap();
                assert_eq!(err.code, -32601);
                assert_eq!(err.message, "Method not found");
            }
            _ => panic!("Expected response error"),
        }
    }

    #[test]
    fn test_parse_notification() {
        let raw = br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///a.rs","diagnostics":[]}}"#;
        let msg = Message::parse(raw).unwrap();
        match msg {
            Message::Notification(notif) => {
                assert_eq!(notif.method, "textDocument/publishDiagnostics");
                assert!(notif.params.is_some());
            }
            _ => panic!("Expected notification"),
        }
    }

    #[test]
    fn test_string_id() {
        let raw = br#"{"jsonrpc":"2.0","id":"abc-123","method":"shutdown"}"#;
        let msg = Message::parse(raw).unwrap();
        match msg {
            Message::Request(req) => {
                assert_eq!(req.id, Id::String("abc-123".to_string()));
            }
            _ => panic!("Expected request"),
        }
    }
}
