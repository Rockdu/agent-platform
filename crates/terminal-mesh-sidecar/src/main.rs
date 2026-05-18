//! `terminal-mesh-sidecar` binary entry point.
//!
//! Reads MCP JSON-RPC messages on stdin, dispatches via
//! `terminal_mesh_sidecar::handle_mcp_request`, writes responses on
//! stdout. Calls back into the host RPC bridge via the unix socket at
//! `--host-rpc-sock` for `tools/call` requests.

use std::env;
use std::process::ExitCode;

use mcp_stdio::{decode_line, encode_message, JsonRpcMessage};
use terminal_mesh_sidecar::{call_bridge_once, handle_mcp_request, parse_args};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TERMINAL_MESH_SIDECAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();

    let args: Vec<String> = env::args().skip(1).collect();
    // Privileged terminal-mesh capability handle, sourced from the
    // env (NOT argv — env keeps it out of `ps` output). Present
    // only when the orchestrator's MCP config minted the
    // privileged mount and embedded the handle.
    let terminal_mesh_capability = env::var("TERMINAL_MESH_CAPABILITY").ok();
    let parsed = match parse_args(args, terminal_mesh_capability) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "terminal-mesh-sidecar: bad args");
            return ExitCode::from(2);
        }
    };
    tracing::info!(
        client_id = %parsed.client_id,
        workspace = %parsed.workspace.display(),
        host_rpc_sock = %parsed.host_rpc_sock.display(),
        cross_tab_read_marker = parsed.cross_tab_read,
        has_terminal_mesh_capability = parsed.terminal_mesh_capability.is_some(),
        "terminal-mesh-sidecar: ready"
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "terminal-mesh-sidecar: stdin read failed");
                return ExitCode::from(1);
            }
        };
        if n == 0 {
            // EOF: orchestrator's claude has closed stdin → exit cleanly.
            return ExitCode::SUCCESS;
        }
        let msg = match decode_line(line.as_bytes()) {
            Ok(JsonRpcMessage::Request(req)) => req,
            Ok(other) => {
                tracing::warn!(?other, "terminal-mesh-sidecar: ignoring non-request message");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "terminal-mesh-sidecar: frame parse failed");
                continue;
            }
        };
        let sock = parsed.host_rpc_sock.clone();
        let response = handle_mcp_request(
            &parsed.client_id,
            parsed.terminal_mesh_capability.as_deref(),
            msg,
            |bridge_req| async move { call_bridge_once(&sock, &bridge_req).await },
        )
        .await;
        let mut bytes = match encode_message(&response) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "terminal-mesh-sidecar: encode failed");
                continue;
            }
        };
        bytes.push(b'\n');
        if let Err(e) = stdout.write_all(&bytes).await {
            tracing::error!(error = %e, "terminal-mesh-sidecar: stdout write failed");
            return ExitCode::from(1);
        }
        if let Err(e) = stdout.flush().await {
            tracing::error!(error = %e, "terminal-mesh-sidecar: stdout flush failed");
            return ExitCode::from(1);
        }
    }
}
