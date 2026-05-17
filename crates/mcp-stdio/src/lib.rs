//! MCP stdio framing helpers.
//!
//! Pure-Rust primitives consumed by the host's `SidecarManager` (task11) and
//! by every plugin sidecar binary. Implements the framing and identity
//! contracts from `docs/specs/mcp-sidecar.md`:
//!
//!   * **Transport: MCP Stdio** — newline-delimited JSON-RPC 2.0 over UTF-8
//!     stdin/stdout. Messages MUST NOT contain embedded newlines.
//!   * **Sidecar Identity Scheme** — `host_ui:<plugin_id>` for host-owned
//!     UI sidecars, `claude:<tab_id>:<plugin_id>` for per-claude sidecars,
//!     where `tab_id` is a UUIDv4. Filenames URL-encode the `:` separator.
//!
//! No async runtime, no tokio, no tauri — so sidecar binaries can depend on
//! this crate without dragging desktop deps into their process image. The
//! host can layer its own async stream wrapper on top of `decode_stream`.

use std::io::{self, BufRead};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Filename escape set per the spec example: encode `:`, `/`, `\\`, and
/// whitespace; preserve alphanumerics + `-`, `_`, `.` so the spec examples
/// (`host_ui%3Anotes.log`, `claude%3A<uuid>%3Agmail.log`) round-trip exactly.
const FILENAME_ESCAPE: &AsciiSet = &CONTROLS
    .add(b':')
    .add(b'/')
    .add(b'\\')
    .add(b' ')
    .add(b'\t')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'|')
    .add(b'?')
    .add(b'*');

pub const JSONRPC_VERSION: &str = "2.0";

// ---------------------------------------------------------------------------
// JSON-RPC envelope types
// ---------------------------------------------------------------------------

/// JSON-RPC request/response id. `Number(i64)` and `String` are the
/// interoperable shapes; `Null` is only valid on notifications-style
/// responses (rarely used here but tolerated for compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
    Null,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// Unified message shape. Serialize uses `#[serde(untagged)]` to flatten
/// the variant — fine because we always construct the correct variant
/// before serializing. Deserialize is **manual** (see the `Deserialize`
/// impl below) and routes through the same `decode_value` structural
/// validator the framing helpers use; this prevents a downstream
/// `serde_json::from_str::<JsonRpcMessage>(...)` call from misrouting
/// a Request payload to Response (which would happen with the
/// derived untagged Deserialize because Response's fields are all
/// optional and serde tolerates unknown fields).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Response(JsonRpcResponse),
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = Value::deserialize(deserializer)?;
        decode_value(v).map_err(serde::de::Error::custom)
    }
}

impl JsonRpcMessage {
    pub fn request(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Request(JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        })
    }

    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification(JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        })
    }

    pub fn response_success(id: JsonRpcId, result: Value) -> Self {
        Self::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        })
    }

    pub fn response_error(id: JsonRpcId, error: JsonRpcError) -> Self {
        Self::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        })
    }
}

// ---------------------------------------------------------------------------
// Framing errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("MCP_FRAMING_ERROR io: {source}")]
    Io {
        #[from]
        source: io::Error,
    },

    #[error("MCP_FRAMING_ERROR not_utf8: {source}")]
    NotUtf8 {
        #[from]
        source: std::str::Utf8Error,
    },

    #[error("MCP_FRAMING_ERROR json_parse: {source}")]
    JsonParse {
        #[from]
        source: serde_json::Error,
    },

    #[error("MCP_FRAMING_ERROR embedded_newline: serialized message contains a literal newline byte; messages must be flat JSON without embedded newlines per MCP stdio framing")]
    EmbeddedNewline,

    #[error("MCP_FRAMING_ERROR empty_line: empty payload is not a valid JSON-RPC message")]
    EmptyLine,

    #[error("MCP_FRAMING_ERROR missing_jsonrpc_field: payload must include `\"jsonrpc\": \"2.0\"`")]
    MissingJsonRpcField,

    #[error("MCP_FRAMING_ERROR invalid_jsonrpc_version: expected `2.0`, got `{found}`")]
    InvalidJsonRpcVersion { found: String },

    #[error("MCP_FRAMING_ERROR invalid_message_shape: {reason}")]
    InvalidMessageShape { reason: String },
}

// ---------------------------------------------------------------------------
// Encode / decode primitives
// ---------------------------------------------------------------------------

/// Inspect a serialized line for embedded newlines. Pure helper exposed for
/// tests; production code calls this via `encode_message`.
fn assert_no_embedded_newline(serialized: &str) -> Result<(), FramingError> {
    if serialized.contains('\n') {
        Err(FramingError::EmbeddedNewline)
    } else {
        Ok(())
    }
}

/// Per-message structural validator applied before serialization or after
/// JSON parsing. Currently enforces JSON-RPC response semantics: exactly
/// one of `result` / `error` set. Notification + Request shapes are
/// already structurally guaranteed by their field layout (no `Option`
/// fields whose presence carries semantic meaning).
fn validate_message_shape(msg: &JsonRpcMessage) -> Result<(), FramingError> {
    if let JsonRpcMessage::Response(r) = msg {
        match (r.result.is_some(), r.error.is_some()) {
            (true, true) => {
                return Err(FramingError::InvalidMessageShape {
                    reason: "response carries both result and error; JSON-RPC requires exactly one"
                        .into(),
                });
            }
            (false, false) => {
                return Err(FramingError::InvalidMessageShape {
                    reason: "response carries neither result nor error; JSON-RPC requires exactly one"
                        .into(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Serialize a JSON-RPC message and frame it for stdio: `<json>\n`.
/// Validates the response shape (exactly one of result/error), serializes,
/// rejects embedded newlines (defense against future pretty-printers or
/// hand-crafted fragments), and appends exactly one terminal `\n`.
pub fn encode_message(msg: &JsonRpcMessage) -> Result<Vec<u8>, FramingError> {
    validate_message_shape(msg)?;
    let line = serde_json::to_string(msg)?;
    assert_no_embedded_newline(&line)?;
    if line.is_empty() {
        return Err(FramingError::EmptyLine);
    }
    let mut out = line.into_bytes();
    out.push(b'\n');
    Ok(out)
}

/// Validate the `jsonrpc` field of an already-parsed JSON object.
fn check_jsonrpc_field(v: &Value) -> Result<(), FramingError> {
    match v.get("jsonrpc") {
        Some(Value::String(s)) if s == JSONRPC_VERSION => Ok(()),
        Some(Value::String(other)) => Err(FramingError::InvalidJsonRpcVersion {
            found: other.clone(),
        }),
        _ => Err(FramingError::MissingJsonRpcField),
    }
}

/// Shared structural validator. Used by both `decode_line` and the manual
/// `Deserialize` impl on `JsonRpcMessage`, so every entry point gets the
/// same protections. Disambiguates the JSON-RPC message variant by
/// inspecting the presence of `id` and `method`:
///   * `id` + `method` → Request
///   * `id` + no `method` → Response (subject to result/error shape check)
///   * no `id` + `method` → Notification
///   * neither → `InvalidMessageShape`
fn decode_value(v: Value) -> Result<JsonRpcMessage, FramingError> {
    check_jsonrpc_field(&v)?;
    let has_id = v.get("id").is_some();
    let has_method = v.get("method").is_some();
    let msg = match (has_id, has_method) {
        (true, true) => JsonRpcMessage::Request(serde_json::from_value(v)?),
        (true, false) => JsonRpcMessage::Response(serde_json::from_value(v)?),
        (false, true) => JsonRpcMessage::Notification(serde_json::from_value(v)?),
        (false, false) => {
            return Err(FramingError::InvalidMessageShape {
                reason: "JSON-RPC payload must include `id` (request/response) or `method` (notification)"
                    .into(),
            });
        }
    };
    validate_message_shape(&msg)?;
    Ok(msg)
}

/// Decode a single newline-delimited message. Accepts at most ONE terminal
/// `\n` and rejects any other embedded `\n` or `\r` as `EmbeddedNewline`
/// (per `docs/specs/mcp-sidecar.md`: "messages MUST NOT contain embedded
/// newlines"). Multiple trailing newlines surface as `EmbeddedNewline` —
/// each frame must contain exactly one message and exactly one delimiter,
/// so a second `\n` is an empty frame and stream callers must observe it.
pub fn decode_line(bytes: &[u8]) -> Result<JsonRpcMessage, FramingError> {
    let s = std::str::from_utf8(bytes)?;
    // Strip AT MOST one terminal `\n` (not `trim_end_matches` which would
    // silently swallow any number of trailing newlines and let an "empty
    // frame" through as if it were part of the previous frame).
    let body = s.strip_suffix('\n').unwrap_or(s);
    if body.trim().is_empty() {
        return Err(FramingError::EmptyLine);
    }
    // After stripping the single terminal delimiter, the body must contain
    // no `\n` or `\r` — serde would otherwise parse them as JSON whitespace
    // and obscure desync bugs.
    if body.contains('\n') || body.contains('\r') {
        return Err(FramingError::EmbeddedNewline);
    }
    let v: Value = serde_json::from_str(body)?;
    decode_value(v)
}

/// Yield typed messages from a `BufRead` stream of newline-delimited
/// JSON-RPC payloads. Each line is one message. IO errors surface as
/// `FramingError::Io`.
pub fn decode_stream<R: BufRead>(
    reader: R,
) -> impl Iterator<Item = Result<JsonRpcMessage, FramingError>> {
    reader.lines().map(|line_result| {
        let line = line_result?;
        decode_line(line.as_bytes())
    })
}

// ---------------------------------------------------------------------------
// Sidecar identity (AC-1.6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientId {
    HostUi { plugin_id: String },
    Claude { tab_id: Uuid, plugin_id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ClientIdError {
    #[error("CLIENT_ID_ERROR empty_plugin_id")]
    EmptyPluginId,

    #[error("CLIENT_ID_ERROR unknown_prefix: expected `host_ui:` or `claude:`, got `{got}`")]
    UnknownPrefix { got: String },

    #[error("CLIENT_ID_ERROR invalid_tab_id: `{got}` is not a valid UUID")]
    InvalidTabId { got: String },

    #[error("CLIENT_ID_ERROR malformed: `{got}` does not match the expected shape")]
    Malformed { got: String },
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientId::HostUi { plugin_id } => write!(f, "host_ui:{plugin_id}"),
            ClientId::Claude { tab_id, plugin_id } => write!(f, "claude:{tab_id}:{plugin_id}"),
        }
    }
}

impl ClientId {
    pub fn parse(s: &str) -> Result<Self, ClientIdError> {
        if let Some(rest) = s.strip_prefix("host_ui:") {
            if rest.is_empty() {
                return Err(ClientIdError::EmptyPluginId);
            }
            return Ok(ClientId::HostUi {
                plugin_id: rest.to_string(),
            });
        }
        if let Some(rest) = s.strip_prefix("claude:") {
            // Expect `<tab_uuid>:<plugin_id>`. Split at the first `:`.
            let dot = rest
                .find(':')
                .ok_or_else(|| ClientIdError::Malformed { got: s.to_string() })?;
            let tab_str = &rest[..dot];
            let plugin_id = &rest[dot + 1..];
            if plugin_id.is_empty() {
                return Err(ClientIdError::EmptyPluginId);
            }
            let tab_id = Uuid::parse_str(tab_str).map_err(|_| ClientIdError::InvalidTabId {
                got: tab_str.to_string(),
            })?;
            return Ok(ClientId::Claude {
                tab_id,
                plugin_id: plugin_id.to_string(),
            });
        }
        Err(ClientIdError::UnknownPrefix { got: s.to_string() })
    }

    /// Filesystem-safe filename for `${APP_DATA}/logs/<plugin_id>/<this>.log`.
    /// Per spec §"Log Channel": "`client_id` MUST be URL-encoded for filename
    /// safety. For example: `host_ui:notes` → `host_ui%3Anotes.log`."
    /// Only filesystem-unsafe chars are escaped — alphanumerics, `-`, `_`,
    /// and `.` pass through so UUIDs render with their canonical hyphens.
    pub fn log_filename(&self) -> String {
        let raw = self.to_string();
        let encoded = utf8_percent_encode(&raw, FILENAME_ESCAPE).to_string();
        format!("{encoded}.log")
    }

    pub fn plugin_id(&self) -> &str {
        match self {
            ClientId::HostUi { plugin_id } | ClientId::Claude { plugin_id, .. } => plugin_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn req(id: i64, method: &str) -> JsonRpcMessage {
        JsonRpcMessage::request(JsonRpcId::Number(id), method, Some(json!({"k": "v"})))
    }
    fn notif(method: &str) -> JsonRpcMessage {
        JsonRpcMessage::notification(method, None)
    }
    fn ok_resp(id: i64, result: Value) -> JsonRpcMessage {
        JsonRpcMessage::response_success(JsonRpcId::Number(id), result)
    }
    fn err_resp(id: i64, code: i64, message: &str) -> JsonRpcMessage {
        JsonRpcMessage::response_error(
            JsonRpcId::Number(id),
            JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        )
    }

    // ----- encode round-trips -----

    #[test]
    fn encode_request_round_trip() {
        let msg = req(1, "ping");
        let bytes = encode_message(&msg).unwrap();
        let decoded = decode_line(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_notification_round_trip() {
        let msg = notif("notifications/cancelled");
        let bytes = encode_message(&msg).unwrap();
        let decoded = decode_line(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_response_success_round_trip() {
        let msg = ok_resp(42, json!({"ok": true}));
        let bytes = encode_message(&msg).unwrap();
        let decoded = decode_line(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_response_error_round_trip() {
        let msg = err_resp(42, -32601, "method not found");
        let bytes = encode_message(&msg).unwrap();
        let decoded = decode_line(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_appends_single_newline() {
        let bytes = encode_message(&req(1, "ping")).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
    }

    #[test]
    fn encode_rejects_embedded_newline_via_helper() {
        // serde_json::to_string never emits literal \n inside string values
        // (it escapes to backslash-n), so the embedded-newline check is a
        // defense against future pretty-printers or hand-crafted fragments.
        // Exercise the helper directly with a string that contains one.
        let err = assert_no_embedded_newline("{\"jsonrpc\":\"2.0\"\n,\"method\":\"x\"}").unwrap_err();
        assert!(matches!(err, FramingError::EmbeddedNewline));
    }

    // ----- decode -----

    #[test]
    fn decode_request_happy_path() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"k":"v"}}"#;
        let msg = decode_line(line).unwrap();
        match msg {
            JsonRpcMessage::Request(r) => {
                assert_eq!(r.jsonrpc, "2.0");
                assert_eq!(r.id, JsonRpcId::Number(1));
                assert_eq!(r.method, "ping");
            }
            other => panic!("expected Request; got {other:?}"),
        }
    }

    #[test]
    fn decode_notification_happy_path() {
        let line = br#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#;
        let msg = decode_line(line).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    fn decode_response_happy_path() {
        let line = br#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#;
        let msg = decode_line(line).unwrap();
        match msg {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, JsonRpcId::Number(42));
                assert!(r.error.is_none());
            }
            other => panic!("expected Response; got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_malformed_json() {
        let err = decode_line(b"this is not json").unwrap_err();
        assert!(matches!(err, FramingError::JsonParse { .. }));
    }

    #[test]
    fn decode_rejects_missing_jsonrpc_field() {
        let line = br#"{"id":1,"method":"x"}"#;
        let err = decode_line(line).unwrap_err();
        assert!(matches!(err, FramingError::MissingJsonRpcField));
    }

    #[test]
    fn decode_rejects_wrong_jsonrpc_version() {
        let line = br#"{"jsonrpc":"1.0","id":1,"method":"x"}"#;
        match decode_line(line).unwrap_err() {
            FramingError::InvalidJsonRpcVersion { found } => assert_eq!(found, "1.0"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn decode_rejects_non_utf8() {
        let bytes = [0xFFu8, 0xFE];
        assert!(matches!(
            decode_line(&bytes).unwrap_err(),
            FramingError::NotUtf8 { .. }
        ));
    }

    #[test]
    fn decode_rejects_empty_line() {
        assert!(matches!(
            decode_line(b"").unwrap_err(),
            FramingError::EmptyLine
        ));
        assert!(matches!(
            decode_line(b"\n").unwrap_err(),
            FramingError::EmptyLine
        ));
        assert!(matches!(
            decode_line(b"   \n").unwrap_err(),
            FramingError::EmptyLine
        ));
    }

    #[test]
    fn decode_strips_trailing_newline() {
        let with_nl = br#"{"jsonrpc":"2.0","id":1,"method":"x"}
"#;
        assert!(decode_line(with_nl).is_ok());
    }

    // ----- stream -----

    #[test]
    fn decode_stream_yields_multiple_messages_from_buffer() {
        let msgs = vec![req(1, "a"), notif("b"), ok_resp(2, json!({"k": 1}))];
        let mut combined: Vec<u8> = Vec::new();
        for m in &msgs {
            combined.extend(encode_message(m).unwrap());
        }
        let cursor = Cursor::new(combined);
        let parsed: Vec<JsonRpcMessage> = decode_stream(cursor)
            .map(|r| r.expect("decode ok"))
            .collect();
        assert_eq!(parsed, msgs);
    }

    #[test]
    fn decode_stream_surfaces_per_line_error() {
        // First line ok, second line malformed: stream returns Ok then Err.
        let ok_bytes = encode_message(&req(1, "a")).unwrap();
        let mut combined = ok_bytes;
        combined.extend_from_slice(b"not json\n");
        let cursor = Cursor::new(combined);
        let mut iter = decode_stream(cursor);
        assert!(iter.next().unwrap().is_ok());
        assert!(matches!(
            iter.next().unwrap().unwrap_err(),
            FramingError::JsonParse { .. }
        ));
    }

    // ----- AC-2.1 high-volume soak -----

    #[test]
    fn encode_decode_10k_round_trip_no_framing_errors() {
        // Mixed-shape stress simulating the "60-second log-heavy run" the
        // AC's positive test calls out: 10k messages of three kinds,
        // encoded into a single byte stream, decoded back, asserted equal.
        const N: usize = 10_000;
        let mut messages = Vec::with_capacity(N);
        for i in 0..N {
            let m = match i % 3 {
                0 => req(i as i64, "method.req"),
                1 => notif("method.notif"),
                _ => ok_resp(i as i64, json!({"i": i})),
            };
            messages.push(m);
        }
        let mut bytes = Vec::with_capacity(N * 80);
        for m in &messages {
            bytes.extend(encode_message(m).expect("encode"));
        }
        let cursor = Cursor::new(bytes);
        let parsed: Vec<JsonRpcMessage> = decode_stream(cursor)
            .map(|r| r.expect("decode ok"))
            .collect();
        assert_eq!(parsed.len(), N);
        assert_eq!(parsed, messages);
    }

    // ----- ClientId (AC-1.6) -----

    fn fixed_uuid() -> Uuid {
        Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
    }

    #[test]
    fn client_id_host_ui_display() {
        let c = ClientId::HostUi {
            plugin_id: "gmail".into(),
        };
        assert_eq!(c.to_string(), "host_ui:gmail");
    }

    #[test]
    fn client_id_claude_display() {
        let c = ClientId::Claude {
            tab_id: fixed_uuid(),
            plugin_id: "gmail".into(),
        };
        assert_eq!(
            c.to_string(),
            "claude:550e8400-e29b-41d4-a716-446655440000:gmail"
        );
    }

    #[test]
    fn client_id_parse_round_trip_host_ui() {
        let s = "host_ui:papers";
        let parsed = ClientId::parse(s).unwrap();
        assert_eq!(parsed.to_string(), s);
    }

    #[test]
    fn client_id_parse_round_trip_claude() {
        let s = "claude:550e8400-e29b-41d4-a716-446655440000:papers";
        let parsed = ClientId::parse(s).unwrap();
        assert_eq!(parsed.to_string(), s);
    }

    #[test]
    fn client_id_parse_rejects_unknown_prefix() {
        match ClientId::parse("something:gmail").unwrap_err() {
            ClientIdError::UnknownPrefix { got } => assert_eq!(got, "something:gmail"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn client_id_parse_rejects_malformed_tab_id() {
        let err = ClientId::parse("claude:not-a-uuid:gmail").unwrap_err();
        assert!(matches!(err, ClientIdError::InvalidTabId { .. }));
    }

    #[test]
    fn client_id_parse_rejects_empty_plugin_id_host_ui() {
        assert!(matches!(
            ClientId::parse("host_ui:").unwrap_err(),
            ClientIdError::EmptyPluginId
        ));
    }

    #[test]
    fn client_id_parse_rejects_empty_plugin_id_claude() {
        assert!(matches!(
            ClientId::parse("claude:550e8400-e29b-41d4-a716-446655440000:").unwrap_err(),
            ClientIdError::EmptyPluginId
        ));
    }

    #[test]
    fn client_id_log_filename_host_ui_urlencodes_colon() {
        // Spec §"Log Channel" example: host_ui:notes → host_ui%3Anotes.log
        let c = ClientId::HostUi {
            plugin_id: "notes".into(),
        };
        assert_eq!(c.log_filename(), "host_ui%3Anotes.log");
    }

    #[test]
    fn client_id_log_filename_claude_urlencodes_only_colons() {
        // Spec §"Log Channel" example:
        // claude:550e8400-e29b-41d4-a716-446655440000:gmail
        //   → claude%3A550e8400-e29b-41d4-a716-446655440000%3Agmail.log
        // (Hyphens within the UUID are preserved; FILENAME_ESCAPE only
        // touches filesystem-unsafe punctuation.)
        let c = ClientId::Claude {
            tab_id: fixed_uuid(),
            plugin_id: "gmail".into(),
        };
        assert_eq!(
            c.log_filename(),
            "claude%3A550e8400-e29b-41d4-a716-446655440000%3Agmail.log"
        );
    }

    // ----- Strict parser hardening (Round 16 / Codex round-15 review) -----

    #[test]
    fn decode_rejects_embedded_newline_in_json_whitespace() {
        // Real \n between `"2.0",` and `"id"` — serde would otherwise parse
        // it as JSON whitespace. The strict framing layer must reject it.
        let payload = b"{\"jsonrpc\":\"2.0\",\n\"id\":1,\"method\":\"x\"}";
        assert!(matches!(
            decode_line(payload).unwrap_err(),
            FramingError::EmbeddedNewline
        ));
    }

    #[test]
    fn decode_rejects_embedded_carriage_return() {
        let payload = b"{\"jsonrpc\":\"2.0\",\r\"id\":1,\"method\":\"x\"}";
        assert!(matches!(
            decode_line(payload).unwrap_err(),
            FramingError::EmbeddedNewline
        ));
    }

    #[test]
    fn decode_rejects_multiple_trailing_newlines() {
        // Two trailing \n: after stripping ONE terminal delimiter, the
        // remaining \n is embedded — proves we use strip_suffix not
        // trim_end_matches.
        let payload = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}\n\n";
        assert!(matches!(
            decode_line(payload).unwrap_err(),
            FramingError::EmbeddedNewline
        ));
    }

    #[test]
    fn decode_stream_surfaces_empty_line_after_double_newline() {
        // Buffer `<line1>\n\n<line2>\n` — BufRead::lines yields:
        //   line1 -> Ok
        //   ""    -> Err(EmptyLine)
        //   line2 -> Ok
        // The empty middle frame surfaces as an error per the spec contract.
        let l1 = encode_message(&req(1, "a")).unwrap();
        let l2 = encode_message(&req(2, "b")).unwrap();
        let mut combined = Vec::new();
        combined.extend(&l1[..l1.len() - 1]); // drop terminal \n
        combined.push(b'\n'); // delimiter
        combined.push(b'\n'); // empty frame
        combined.extend(&l2); // includes its own terminal \n

        let cursor = Cursor::new(combined);
        let mut iter = decode_stream(cursor);
        let first = iter.next().unwrap();
        assert!(first.is_ok(), "first ok: {first:?}");
        let second = iter.next().unwrap();
        assert!(
            matches!(second, Err(FramingError::EmptyLine)),
            "second should be EmptyLine: {second:?}"
        );
        let third = iter.next().unwrap();
        assert!(third.is_ok(), "third ok: {third:?}");
    }

    #[test]
    fn decode_rejects_response_with_result_and_error() {
        let payload =
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#;
        match decode_line(payload).unwrap_err() {
            FramingError::InvalidMessageShape { reason } => {
                assert!(reason.contains("both"), "reason should mention both: {reason}");
            }
            other => panic!("expected InvalidMessageShape; got {other}"),
        }
    }

    #[test]
    fn decode_rejects_response_without_result_or_error() {
        let payload = br#"{"jsonrpc":"2.0","id":1}"#;
        match decode_line(payload).unwrap_err() {
            FramingError::InvalidMessageShape { reason } => {
                assert!(
                    reason.contains("neither"),
                    "reason should mention neither: {reason}"
                );
            }
            other => panic!("expected InvalidMessageShape; got {other}"),
        }
    }

    #[test]
    fn encode_rejects_response_with_result_and_error() {
        let bad = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id: JsonRpcId::Number(1),
            result: Some(json!({})),
            error: Some(JsonRpcError {
                code: -1,
                message: "x".into(),
                data: None,
            }),
        });
        match encode_message(&bad).unwrap_err() {
            FramingError::InvalidMessageShape { reason } => {
                assert!(reason.contains("both"));
            }
            other => panic!("expected InvalidMessageShape; got {other}"),
        }
    }

    #[test]
    fn encode_rejects_response_without_result_or_error() {
        let bad = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id: JsonRpcId::Number(1),
            result: None,
            error: None,
        });
        match encode_message(&bad).unwrap_err() {
            FramingError::InvalidMessageShape { reason } => {
                assert!(reason.contains("neither"));
            }
            other => panic!("expected InvalidMessageShape; got {other}"),
        }
    }

    #[test]
    fn serde_deserialize_jsonrpc_message_request_routes_to_request() {
        // The Round-15 footgun: derived #[serde(untagged)] Deserialize on
        // JsonRpcMessage was misrouting Request payloads to Response. The
        // manual Deserialize impl introduced in Round 16 must route via
        // decode_value so direct serde calls behave identically to
        // decode_line.
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"k":"v"}}"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert!(
            matches!(msg, JsonRpcMessage::Request(_)),
            "expected Request via direct serde; got {msg:?}"
        );
    }

    #[test]
    fn serde_deserialize_jsonrpc_message_response_with_both_fields_fails() {
        // Direct serde path must also surface the invalid-shape error.
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#;
        assert!(serde_json::from_str::<JsonRpcMessage>(json).is_err());
    }

    #[test]
    fn serde_deserialize_jsonrpc_message_response_without_either_field_fails() {
        let json = r#"{"jsonrpc":"2.0","id":1}"#;
        assert!(serde_json::from_str::<JsonRpcMessage>(json).is_err());
    }

    #[test]
    fn client_id_plugin_id_accessor() {
        assert_eq!(
            ClientId::HostUi {
                plugin_id: "x".into()
            }
            .plugin_id(),
            "x"
        );
        assert_eq!(
            ClientId::Claude {
                tab_id: fixed_uuid(),
                plugin_id: "y".into()
            }
            .plugin_id(),
            "y"
        );
    }
}
