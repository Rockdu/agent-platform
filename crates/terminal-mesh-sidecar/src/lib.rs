//! task21 / AC-3.3 — `terminal-mesh` MCP sidecar.
//!
//! Exposes ONE MCP tool to the orchestrator's `claude`:
//! `terminal_mesh.read_scrollback(target_tab_id: string, max_bytes: u32)`.
//! Calls back into the host RPC bridge (`${APP_DATA}/host-rpc/host.sock`)
//! via JSON-RPC 2.0 over a unix domain socket. The bridge authorizes
//! from host state (clientId from CLI args + OrchestratorState lookup);
//! the sidecar holds NO capability handle of its own.
//!
//! Frame format: newline-delimited JSON-RPC 2.0 (same as `mcp-stdio`).

use std::path::PathBuf;

use mcp_stdio::{
    decode_line, encode_message, JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest,
    JsonRpcResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// CLI arguments parsed from `argv` by the binary entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarArgs {
    pub client_id: String,
    pub workspace: PathBuf,
    pub host_rpc_sock: PathBuf,
    /// Configuration-intent marker accepted from the orchestrator's
    /// MCP config ("configuration intent only; the host bridge's
    /// clientId-based auth is authoritative"). This flag does NOT
    /// change the sidecar's behavior in any way.
    pub cross_tab_read: bool,
    /// Privileged terminal-mesh capability handle, sourced from the
    /// `TERMINAL_MESH_CAPABILITY` env var. Present only in the
    /// orchestrator's sidecar config. Forwarded to the host bridge
    /// as `terminalMeshCapability` in `listTabs` so the bridge gate
    /// can grant `All` scope (the gate requires the CALLER to
    /// present the capability, not just exist on the stored side —
    /// clientId alone is forgeable).
    pub terminal_mesh_capability: Option<String>,
}

#[derive(Debug, Error)]
pub enum SidecarArgError {
    #[error("missing required argument `{0}`")]
    MissingRequired(&'static str),
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("argument `{0}` expects a value")]
    NeedsValue(&'static str),
}

/// Parse CLI args from an iterator (skip argv[0] before calling).
/// Accepts: `--client-id <id>` `--workspace <path>` `--host-rpc-sock <path>` `[--cross-tab-read]`.
/// The optional `terminal_mesh_capability` is the env-sourced
/// capability handle; the binary entry point reads `TERMINAL_MESH_CAPABILITY`
/// and passes it through here so tests can inject deterministic values.
pub fn parse_args<I: IntoIterator<Item = String>>(
    args: I,
    terminal_mesh_capability: Option<String>,
) -> Result<SidecarArgs, SidecarArgError> {
    let mut client_id: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut host_rpc_sock: Option<PathBuf> = None;
    let mut cross_tab_read = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--client-id" => {
                client_id = Some(iter.next().ok_or(SidecarArgError::NeedsValue("--client-id"))?);
            }
            "--workspace" => {
                workspace = Some(PathBuf::from(
                    iter.next().ok_or(SidecarArgError::NeedsValue("--workspace"))?,
                ));
            }
            "--host-rpc-sock" => {
                host_rpc_sock = Some(PathBuf::from(
                    iter.next().ok_or(SidecarArgError::NeedsValue("--host-rpc-sock"))?,
                ));
            }
            "--cross-tab-read" => {
                cross_tab_read = true;
            }
            other => return Err(SidecarArgError::Unexpected(other.to_string())),
        }
    }
    Ok(SidecarArgs {
        client_id: client_id.ok_or(SidecarArgError::MissingRequired("--client-id"))?,
        workspace: workspace.ok_or(SidecarArgError::MissingRequired("--workspace"))?,
        host_rpc_sock: host_rpc_sock
            .ok_or(SidecarArgError::MissingRequired("--host-rpc-sock"))?,
        cross_tab_read,
        terminal_mesh_capability,
    })
}

/// MCP `tools/list` shape — the tools this sidecar exposes. The
/// host bridge enforces auth; the sidecar is a passthrough.
pub fn tools_list_response() -> Value {
    json!({
        "tools": [
            {
                "name": "terminal_mesh.read_scrollback",
                "description": "Read the bounded scrollback buffer for a tab. Orchestrator-only for cross-tab reads; regular tabs may self-read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target_tab_id": {
                            "type": "string",
                            "description": "The tab id whose scrollback to read"
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Max bytes to return (host caps at 262144)",
                            "default": 65536
                        }
                    },
                    "required": ["target_tab_id"]
                }
            },
            {
                "name": "terminal_mesh.list_tabs",
                "description": "List visible terminal tabs. Orchestrator callers see every tab including the orchestrator slot; regular tabs see only their own.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "agent_platform.open_workspace",
                "description": "Open a workspace as a new agent tab and auto-launch Claude Code inside it. Supports local paths and remote SSH machines. Returns workspace_id and tab_id.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to open. Formats: '/local/path' for local, 'user@host:/remote/path' for SSH remote, 'user@host:22:/path' for SSH with port, 'host:/path' for SSH without user."
                        },
                        "name": {
                            "type": "string",
                            "description": "Friendly display name (defaults to the last path component)"
                        }
                    },
                    "required": ["path"]
                }
            }
        ]
    })
}

#[derive(Debug, Deserialize)]
struct ReadScrollbackToolArgs {
    target_tab_id: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: u32,
}

fn default_max_bytes() -> u32 {
    65536
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeReadScrollbackParams<'a> {
    client_id: &'a str,
    target_tab_id: &'a str,
    max_bytes: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeListTabsParams<'a> {
    client_id: &'a str,
    /// Privileged orchestrator capability handle, sourced from the
    /// `TERMINAL_MESH_CAPABILITY` env var. The host bridge gate
    /// grants `ListTabsScope::All` only when the caller PRESENTS
    /// the capability AND it matches the stored value — clientId
    /// alone is forgeable. Omitted from the wire when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_mesh_capability: Option<&'a str>,
}

/// Translate an MCP `tools/call` request into a bridge JSON-RPC call.
/// Pure function for testability — does not perform IO. Returns the
/// JSON-RPC request the caller should send over the socket. The
/// dispatch is keyed by the MCP `tool_name` so adding tools is a pure
/// arm-extension.
///
/// `terminal_mesh_capability` is the env-sourced privileged handle
/// (present only in the orchestrator's sidecar config). Forwarded
/// for `list_tabs` so the host bridge can grant `All` scope.
pub fn build_bridge_request(
    client_id: &str,
    terminal_mesh_capability: Option<&str>,
    tool_name: &str,
    tool_args: Value,
    request_id: JsonRpcId,
) -> Result<JsonRpcMessage, JsonRpcError> {
    match tool_name {
        "terminal_mesh.read_scrollback" => {
            let parsed: ReadScrollbackToolArgs =
                serde_json::from_value(tool_args).map_err(|e| JsonRpcError {
                    code: -32602,
                    message: format!("invalid tool args: {e}"),
                    data: None,
                })?;
            let params = serde_json::to_value(BridgeReadScrollbackParams {
                client_id,
                target_tab_id: &parsed.target_tab_id,
                max_bytes: parsed.max_bytes,
            })
            .expect("bridge params serialize");
            Ok(JsonRpcMessage::request(
                request_id,
                "terminalMesh.readScrollback",
                Some(params),
            ))
        }
        "terminal_mesh.list_tabs" => {
            // The list_tabs tool takes no inputSchema fields beyond
            // the implicit clientId + capability, so tool_args is
            // ignored.
            let _ = tool_args;
            let params = serde_json::to_value(BridgeListTabsParams {
                client_id,
                terminal_mesh_capability,
            })
            .expect("bridge params serialize");
            Ok(JsonRpcMessage::request(
                request_id,
                "terminalMesh.listTabs",
                Some(params),
            ))
        }
        "agent_platform.open_workspace" => {
            // Forward path + name directly; the host handler validates them.
            let params = serde_json::to_value(serde_json::json!({
                "clientId": client_id,
                "path":    tool_args.get("path").cloned().unwrap_or(Value::Null),
                "name":    tool_args.get("name").cloned(),
            }))
            .expect("bridge params serialize");
            Ok(JsonRpcMessage::request(
                request_id,
                "agentPlatform.openWorkspace",
                Some(params),
            ))
        }
        other => Err(JsonRpcError {
            code: -32601,
            message: format!("unknown tool `{other}`"),
            data: None,
        }),
    }
}

/// Translate a bridge JSON-RPC response into the MCP `tools/call`
/// result envelope: success → `content: [{type: "text", text: <text>}]`;
/// error → `isError: true, content: [{type: "text", text: <message>}]`.
/// `read_scrollback` returns `{data: "..."}` and uses the data string
/// verbatim; other tools (e.g. `list_tabs`) return structured JSON
/// and are serialized as a single text content block so MCP clients
/// receive a parseable JSON payload.
pub fn translate_bridge_response(response: JsonRpcResponse) -> Value {
    match (response.result, response.error) {
        (Some(result), None) => {
            let text = if let Some(data) = result.get("data").and_then(|v| v.as_str()) {
                data.to_string()
            } else {
                serde_json::to_string(&result).unwrap_or_else(|_| "{}".into())
            };
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        (_, Some(err)) => json!({
            "content": [{ "type": "text", "text": format!("{} (code {})", err.message, err.code) }],
            "isError": true,
        }),
        (None, None) => json!({
            "content": [{ "type": "text", "text": "host bridge returned an empty response" }],
            "isError": true,
        }),
    }
}

/// Send one JSON-RPC request over a freshly-connected unix socket and
/// await the matching response. Each call opens a new connection; the
/// host bridge handles multiple sequential requests per connection but
/// the orchestrator's claude rate is low and the simplicity wins.
pub async fn call_bridge_once(
    sock_path: &std::path::Path,
    request: &JsonRpcMessage,
) -> std::io::Result<JsonRpcResponse> {
    let stream = UnixStream::connect(sock_path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut bytes = encode_message(request)
        .map_err(|e| std::io::Error::other(format!("encode: {e}")))?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.flush().await?;
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "host bridge closed connection before response",
        ));
    }
    let msg = decode_line(line.as_bytes()).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("decode: {e}"))
    })?;
    match msg {
        JsonRpcMessage::Response(r) => Ok(r),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected Response from host bridge, got {other:?}"),
        )),
    }
}

/// Handle one MCP request on stdio. Returns the response message.
/// Pure-ish: the only IO is the optional bridge call, which is
/// injected via the `bridge` async closure so tests can provide a
/// mock.
pub async fn handle_mcp_request<F, Fut>(
    client_id: &str,
    terminal_mesh_capability: Option<&str>,
    req: JsonRpcRequest,
    bridge: F,
) -> JsonRpcMessage
where
    F: FnOnce(JsonRpcMessage) -> Fut,
    Fut: std::future::Future<Output = Result<JsonRpcResponse, std::io::Error>>,
{
    match req.method.as_str() {
        "tools/list" => JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(tools_list_response()),
            error: None,
        }),
        "tools/call" => {
            let params = req.params.unwrap_or(Value::Null);
            let tool_name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let bridge_req = match build_bridge_request(
                client_id,
                terminal_mesh_capability,
                &tool_name,
                arguments,
                JsonRpcId::Number(1),
            ) {
                    Ok(r) => r,
                    Err(err) => {
                        return JsonRpcMessage::Response(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(err),
                        });
                    }
                };
            match bridge(bridge_req).await {
                Ok(resp) => JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: req.id,
                    result: Some(translate_bridge_response(resp)),
                    error: None,
                }),
                Err(e) => JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: req.id,
                    result: Some(json!({
                        "content": [{ "type": "text", "text": format!("host bridge unreachable: {e}") }],
                        "isError": true,
                    })),
                    error: None,
                }),
            }
        }
        "initialize" => JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "terminal-mesh-sidecar",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            })),
            error: None,
        }),
        other => JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("unknown method `{other}`"),
                data: None,
            }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_round_trip() {
        let args = vec![
            "--client-id".into(),
            "claude:abc:terminal-mesh".into(),
            "--workspace".into(),
            "/tmp/ws".into(),
            "--host-rpc-sock".into(),
            "/tmp/host.sock".into(),
        ];
        let parsed = parse_args(args, None).expect("parse ok");
        assert_eq!(parsed.client_id, "claude:abc:terminal-mesh");
        assert_eq!(parsed.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(parsed.host_rpc_sock, PathBuf::from("/tmp/host.sock"));
        assert!(!parsed.cross_tab_read);
    }

    #[test]
    fn parse_args_accepts_cross_tab_read_marker() {
        let args = vec![
            "--client-id".into(),
            "claude:abc:terminal-mesh".into(),
            "--workspace".into(),
            "/tmp/ws".into(),
            "--host-rpc-sock".into(),
            "/tmp/h.sock".into(),
            "--cross-tab-read".into(),
        ];
        let parsed = parse_args(args, None).expect("parse ok");
        assert!(parsed.cross_tab_read, "marker preserved");
    }

    #[test]
    fn parse_args_rejects_missing_required() {
        let err = parse_args(vec!["--client-id".into(), "x".into()], None).unwrap_err();
        assert!(matches!(err, SidecarArgError::MissingRequired("--workspace")));
    }

    #[test]
    fn tools_list_returns_both_terminal_mesh_tools() {
        let v = tools_list_response();
        let tools = v.get("tools").and_then(|t| t.as_array()).expect("tools array");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"terminal_mesh.read_scrollback"));
        assert!(names.contains(&"terminal_mesh.list_tabs"));

        // read_scrollback schema must mention target_tab_id.
        let read_tool = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("terminal_mesh.read_scrollback"))
            .expect("read_scrollback present");
        let read_props = read_tool
            .get("inputSchema")
            .and_then(|s| s.get("properties"))
            .expect("read inputSchema.properties");
        assert!(read_props.get("target_tab_id").is_some());

        // list_tabs schema is an empty-properties object.
        let list_tool = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("terminal_mesh.list_tabs"))
            .expect("list_tabs present");
        assert!(list_tool.get("inputSchema").is_some());
    }

    #[test]
    fn tools_list_response_contains_no_privileged_flag_terminology() {
        // The sidecar wire shape MUST NOT leak host-side privileged-
        // flag terminology. Privileged scope info stays inside the
        // host bridge (cross_tab_read flag, capability handle, etc.).
        let v = tools_list_response();
        let s = serde_json::to_string(&v).expect("serialize");
        assert!(!s.contains("cross_tab_read"), "wire leaks privileged flag");
        assert!(!s.contains("privileged"), "wire leaks privileged keyword");
        assert!(!s.contains("capability"), "wire leaks capability keyword");
    }

    #[test]
    fn build_bridge_request_for_read_scrollback_forwards_client_id_and_args() {
        let tool_args = json!({
            "target_tab_id": "abc-tab",
            "max_bytes": 1234,
        });
        let req = build_bridge_request(
            "claude:T:terminal-mesh",
            None,
            "terminal_mesh.read_scrollback",
            tool_args,
            JsonRpcId::Number(7),
        )
        .expect("build ok");
        let JsonRpcMessage::Request(r) = req else {
            panic!("expected Request");
        };
        assert_eq!(r.method, "terminalMesh.readScrollback");
        let params = r.params.expect("params present");
        assert_eq!(params["clientId"], "claude:T:terminal-mesh");
        assert_eq!(params["targetTabId"], "abc-tab");
        assert_eq!(params["maxBytes"], 1234);
    }

    #[test]
    fn build_bridge_request_for_read_scrollback_defaults_max_bytes_when_omitted() {
        let tool_args = json!({ "target_tab_id": "abc" });
        let req = build_bridge_request(
            "claude:T:terminal-mesh",
            None,
            "terminal_mesh.read_scrollback",
            tool_args,
            JsonRpcId::Number(7),
        )
        .expect("build ok");
        let JsonRpcMessage::Request(r) = req else {
            panic!("expected Request");
        };
        let params = r.params.expect("params");
        assert_eq!(params["maxBytes"], 65536);
    }

    #[test]
    fn build_bridge_request_for_list_tabs_omits_capability_when_none() {
        // Regular workspace clients (no capability) MUST NOT
        // include the `terminalMeshCapability` field on the wire —
        // the host bridge's `#[serde(default)]` would otherwise see
        // `Some("")` etc. and conflate it with a real handle.
        let req = build_bridge_request(
            "claude:T:terminal-mesh",
            None,
            "terminal_mesh.list_tabs",
            Value::Null,
            JsonRpcId::Number(9),
        )
        .expect("build ok");
        let JsonRpcMessage::Request(r) = req else {
            panic!("expected Request");
        };
        assert_eq!(r.method, "terminalMesh.listTabs");
        let params = r.params.expect("params present");
        assert_eq!(params["clientId"], "claude:T:terminal-mesh");
        assert!(
            params.get("terminalMeshCapability").is_none(),
            "capability MUST be omitted from the wire when None"
        );
        assert!(params.get("targetTabId").is_none());
        assert!(params.get("maxBytes").is_none());
    }

    #[test]
    fn build_bridge_request_for_list_tabs_forwards_capability_when_present() {
        // Orchestrator's sidecar receives the capability via the
        // `TERMINAL_MESH_CAPABILITY` env var; the bridge request
        // MUST forward it as `terminalMeshCapability` so the host
        // gate can grant `All` scope (the gate requires the
        // CALLER to PRESENT the token — clientId alone is
        // forgeable).
        let req = build_bridge_request(
            "claude:T:terminal-mesh",
            Some("orch-handle-xyz"),
            "terminal_mesh.list_tabs",
            Value::Null,
            JsonRpcId::Number(11),
        )
        .expect("build ok");
        let JsonRpcMessage::Request(r) = req else {
            panic!("expected Request");
        };
        let params = r.params.expect("params present");
        assert_eq!(params["clientId"], "claude:T:terminal-mesh");
        assert_eq!(
            params["terminalMeshCapability"], "orch-handle-xyz",
            "capability forwarded byte-for-byte"
        );
    }

    #[test]
    fn build_bridge_request_rejects_unknown_tool_name() {
        let err = build_bridge_request(
            "claude:T:terminal-mesh",
            None,
            "terminal_mesh.not_a_real_tool",
            Value::Null,
            JsonRpcId::Number(1),
        )
        .expect_err("unknown tool must error");
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn parse_args_round_trip_carries_terminal_mesh_capability() {
        let args = vec![
            "--client-id".into(),
            "claude:abc:terminal-mesh".into(),
            "--workspace".into(),
            "/tmp/ws".into(),
            "--host-rpc-sock".into(),
            "/tmp/host.sock".into(),
        ];
        let parsed =
            parse_args(args, Some("orch-handle-abc".into())).expect("parse ok");
        assert_eq!(
            parsed.terminal_mesh_capability.as_deref(),
            Some("orch-handle-abc")
        );
    }

    #[test]
    fn translate_bridge_response_success_wraps_data_as_text_content() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            result: Some(json!({ "data": "hello world", "maxBytes": 262144 })),
            error: None,
        };
        let v = translate_bridge_response(resp);
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello world");
    }

    #[test]
    fn translate_bridge_response_error_surfaces_as_is_error_content() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            result: None,
            error: Some(JsonRpcError {
                code: -32001,
                message: "regular tab may not cross-tab read".to_string(),
                data: None,
            }),
        };
        let v = translate_bridge_response(resp);
        assert_eq!(v["isError"], true);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("regular tab may not"));
        assert!(text.contains("-32001"));
    }

    #[tokio::test]
    async fn handle_mcp_request_tools_list_skips_bridge() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            method: "tools/list".to_string(),
            params: None,
        };
        let resp = handle_mcp_request("claude:T:terminal-mesh", None, req, |_| async {
            panic!("tools/list must not call the bridge");
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: JsonRpcId::Number(99),
                result: None,
                error: None,
            })
        })
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        assert!(r.result.is_some());
        let result = r.result.unwrap();
        let tool_names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(tool_names.contains(&"terminal_mesh.read_scrollback"));
        assert!(tool_names.contains(&"terminal_mesh.list_tabs"));
    }

    #[tokio::test]
    async fn handle_mcp_request_tools_call_list_tabs_forwards_to_bridge_and_returns_json() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(10),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "terminal_mesh.list_tabs",
                "arguments": {}
            })),
        };
        let resp = handle_mcp_request("claude:T:terminal-mesh", None, req, |bridge_req| async move {
            let JsonRpcMessage::Request(r) = bridge_req else {
                panic!("expected Request");
            };
            assert_eq!(r.method, "terminalMesh.listTabs");
            assert_eq!(r.params.as_ref().unwrap()["clientId"], "claude:T:terminal-mesh");
            assert!(r.params.as_ref().unwrap().get("targetTabId").is_none());
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: JsonRpcId::Number(1),
                result: Some(json!({
                    "tabs": [
                        { "tabId": "abc", "workspaceName": "demo", "status": "Running" }
                    ]
                })),
                error: None,
            })
        })
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        let result = r.result.unwrap();
        assert_eq!(result["isError"], false);
        // List-tabs result is a structured JSON payload serialized as
        // a single text content block (the bridge response has no
        // "data" string field).
        let text = result["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("\"tabs\""));
        assert!(text.contains("demo"));
        assert!(text.contains("abc"));
    }

    /// When the sidecar holds the orchestrator's privileged
    /// capability, `handle_mcp_request` MUST forward it on the
    /// bridge call for `list_tabs` so the host gate can grant
    /// `All` scope.
    #[tokio::test]
    async fn handle_mcp_request_tools_call_list_tabs_forwards_capability_to_bridge() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(12),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "terminal_mesh.list_tabs",
                "arguments": {}
            })),
        };
        let resp = handle_mcp_request(
            "claude:T:terminal-mesh",
            Some("orch-handle-12345"),
            req,
            |bridge_req| async move {
                let JsonRpcMessage::Request(r) = bridge_req else {
                    panic!("expected Request");
                };
                assert_eq!(r.method, "terminalMesh.listTabs");
                let params = r.params.expect("params");
                assert_eq!(params["clientId"], "claude:T:terminal-mesh");
                assert_eq!(
                    params["terminalMeshCapability"], "orch-handle-12345",
                    "capability forwarded from sidecar"
                );
                Ok(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: JsonRpcId::Number(1),
                    result: Some(json!({ "tabs": [] })),
                    error: None,
                })
            },
        )
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        assert!(r.result.is_some());
    }

    #[tokio::test]
    async fn handle_mcp_request_tools_call_forwards_to_bridge_and_returns_text() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(2),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "terminal_mesh.read_scrollback",
                "arguments": { "target_tab_id": "tab-x", "max_bytes": 4096 }
            })),
        };
        let resp = handle_mcp_request("claude:T:terminal-mesh", None, req, |bridge_req| async move {
            // Assert the bridge request shape.
            let JsonRpcMessage::Request(r) = bridge_req else {
                panic!("expected Request");
            };
            assert_eq!(r.method, "terminalMesh.readScrollback");
            assert_eq!(r.params.as_ref().unwrap()["targetTabId"], "tab-x");
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: JsonRpcId::Number(1),
                result: Some(json!({ "data": "mock-scrollback", "maxBytes": 262144 })),
                error: None,
            })
        })
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        assert!(r.result.is_some());
        let result = r.result.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "mock-scrollback");
    }

    #[tokio::test]
    async fn handle_mcp_request_tools_call_bridge_error_surfaces_as_is_error() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(3),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "terminal_mesh.read_scrollback",
                "arguments": { "target_tab_id": "tab-z" }
            })),
        };
        let resp = handle_mcp_request("claude:T:terminal-mesh", None, req, |_| async {
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: JsonRpcId::Number(1),
                result: None,
                error: Some(JsonRpcError {
                    code: -32001,
                    message: "denied".into(),
                    data: None,
                }),
            })
        })
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        let result = r.result.unwrap();
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("denied"));
    }

    #[tokio::test]
    async fn handle_mcp_request_unknown_tool_returns_method_not_found() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(4),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "no.such.tool",
                "arguments": {}
            })),
        };
        let resp = handle_mcp_request("claude:T:terminal-mesh", None, req, |_| async {
            panic!("must not call bridge for unknown tool");
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: JsonRpcId::Number(1),
                result: None,
                error: None,
            })
        })
        .await;
        let JsonRpcMessage::Response(r) = resp else {
            panic!("expected Response");
        };
        let err = r.error.expect("error present");
        assert_eq!(err.code, -32601);
    }
}
