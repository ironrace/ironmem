//! MCP protocol types — JSON-RPC 2.0 over stdio.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

/// Generic outgoing JSON-RPC 2.0 request envelope for internal clients.
///
/// Deliberately separate from [`JsonRpcRequest`]: that type is the
/// server-side, `Deserialize`-only, untyped-`params` shape used to parse an
/// incoming request. This one is `Serialize`-only and generic over `P`, so a
/// latency-sensitive outgoing client (e.g. the prompt-hook's daemon client)
/// gets typed, allocation-free params without going through
/// `serde_json::Value` — while still sharing the `jsonrpc`/`id`/`method`
/// scaffolding instead of redefining it per client.
#[derive(Serialize)]
pub struct JsonRpcCall<P> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: P,
}

impl JsonRpcResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<serde_json::Value>, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// MCP capabilities response for tools/list.
pub fn capabilities_response() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ironmem",
            "version": env!("IRONMEM_VERSION")
        }
    })
}
