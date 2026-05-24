//! `papers-plugin` binary entry point.
//!
//! Reads MCP JSON-RPC messages on stdin, dispatches via
//! `papers_plugin::handle_mcp_message`, writes responses on stdout.
//! Performs HTTP requests to arXiv via `reqwest::blocking` in a
//! `spawn_blocking` task so the tokio reactor is not stalled.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use mcp_stdio::{decode_line, encode_message, JsonRpcMessage};
use papers_plugin::{
    fetch_arxiv_with_retry, handle_mcp_message, parse_args, resolve_papers_db_path, ArxivError,
    PapersStore, MAX_RETRY_ATTEMPTS,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PAPERS_PLUGIN_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();

    let args: Vec<String> = env::args().skip(1).collect();
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "papers-plugin: bad args");
            return ExitCode::from(2);
        }
    };
    tracing::info!(
        client_id = %parsed.client_id,
        workspace = %parsed.workspace.display(),
        has_host_rpc_sock = parsed.host_rpc_sock.is_some(),
        cross_tab_read_marker = parsed.cross_tab_read,
        "papers-plugin: ready"
    );

    let db_path = match resolve_papers_db_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "papers-plugin: could not resolve db path");
            return ExitCode::from(3);
        }
    };
    let store = match PapersStore::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "papers-plugin: sqlite open failed");
            return ExitCode::from(4);
        }
    };

    let http_client = match reqwest::blocking::Client::builder()
        .user_agent(concat!("papers-plugin/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "papers-plugin: reqwest build failed");
            return ExitCode::from(5);
        }
    };

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "papers-plugin: stdin read failed");
                return ExitCode::from(1);
            }
        };
        if n == 0 {
            return ExitCode::SUCCESS;
        }
        let msg = match decode_line(line.as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "papers-plugin: frame parse failed");
                continue;
            }
        };
        let store_clone = store.clone();
        let http_clone = http_client.clone();
        let resp = tokio::task::spawn_blocking(move || {
            let fetcher = |url: &str| -> Result<String, ArxivError> {
                fetch_arxiv_with_retry(&http_clone, url, MAX_RETRY_ATTEMPTS)
            };
            handle_mcp_message(msg, store_clone.as_ref(), &fetcher)
        })
        .await
        .unwrap_or(None);

        let Some(response) = resp else {
            continue;
        };
        let envelope = JsonRpcMessage::Response(response);
        let mut bytes = match encode_message(&envelope) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "papers-plugin: encode failed");
                continue;
            }
        };
        bytes.push(b'\n');
        if let Err(e) = stdout.write_all(&bytes).await {
            tracing::error!(error = %e, "papers-plugin: stdout write failed");
            return ExitCode::from(1);
        }
        if let Err(e) = stdout.flush().await {
            tracing::error!(error = %e, "papers-plugin: stdout flush failed");
            return ExitCode::from(1);
        }
    }
}
