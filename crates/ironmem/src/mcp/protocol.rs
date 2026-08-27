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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
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
                data: None,
            }),
        }
    }

    /// Same as [`Self::error`], with a structured `data` payload attached —
    /// used by `-32022` to carry the requested and supported protocol
    /// versions so a client can react programmatically instead of parsing
    /// `message`.
    pub fn error_with_data(
        id: Option<serde_json::Value>,
        code: i64,
        message: &str,
        data: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: Some(data),
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

/// MCP protocol revisions this server understands, oldest first. Current as
/// of issue #275 — the current revision plus the prior four.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    "2026-07-28",
];

/// The version negotiated when a client's `initialize` omits
/// `protocolVersion` entirely. Not part of the MCP spec's negotiation
/// algorithm (a compliant client always sends one) — this only covers
/// already-permissive callers (this crate's own test/health-probe code, and
/// any pre-negotiation client still in the wild) so they keep getting a
/// successful handshake instead of a new hard failure.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2026-07-28";

/// Build the successful `initialize` result body, negotiating on
/// `requested_version`:
/// - `Some(v)` with `v` in [`SUPPORTED_PROTOCOL_VERSIONS`]: echoes `v` back.
/// - `None`: negotiates [`DEFAULT_PROTOCOL_VERSION`].
///
/// Returns `Err(v)` when `v` is set but not supported; the caller turns that
/// into a `-32022` JSON-RPC error carrying `v` and the supported list.
pub fn negotiate_initialize(requested_version: Option<&str>) -> Result<serde_json::Value, String> {
    let negotiated = match requested_version {
        None => DEFAULT_PROTOCOL_VERSION,
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        Some(v) => return Err(v.to_string()),
    };
    Ok(serde_json::json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ironmem",
            "version": env!("IRONMEM_VERSION")
        }
    }))
}

/// `server/discover` result body: supported protocol versions, capabilities,
/// and server identity, answerable without a prior `initialize` handshake.
pub fn discover_response() -> serde_json::Value {
    serde_json::json!({
        "protocolVersions": SUPPORTED_PROTOCOL_VERSIONS,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ironmem",
            "version": env!("IRONMEM_VERSION")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_initialize_echoes_each_supported_version() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let body = negotiate_initialize(Some(version)).unwrap();
            assert_eq!(body["protocolVersion"], serde_json::json!(version));
        }
    }

    #[test]
    fn negotiate_initialize_defaults_when_version_omitted() {
        let body = negotiate_initialize(None).unwrap();
        assert_eq!(
            body["protocolVersion"],
            serde_json::json!(DEFAULT_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn negotiate_initialize_rejects_unsupported_version() {
        let err = negotiate_initialize(Some("1999-01-01")).unwrap_err();
        assert_eq!(err, "1999-01-01");
    }

    #[test]
    fn discover_response_lists_all_supported_versions() {
        let body = discover_response();
        let versions: Vec<&str> = body["protocolVersions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(versions, SUPPORTED_PROTOCOL_VERSIONS);
    }

    #[test]
    fn error_with_data_carries_structured_payload() {
        let response = JsonRpcResponse::error_with_data(
            Some(serde_json::json!(1)),
            -32022,
            "Unsupported protocol version",
            serde_json::json!({"requested": "1999-01-01", "supported": SUPPORTED_PROTOCOL_VERSIONS}),
        );
        let error = response.error.expect("error_with_data must set error");
        assert_eq!(error.code, -32022);
        assert_eq!(
            error.data.expect("data must be set")["requested"],
            serde_json::json!("1999-01-01")
        );
    }
}
