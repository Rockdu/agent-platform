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
    fetch_arxiv_with_retry, handle_mcp_message, make_blocking_arxiv_fetcher, parse_args,
    resolve_papers_db_path, ArxivError, PapersStore, MAX_RETRY_ATTEMPTS,
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

    // NOTE: the blocking reqwest client is constructed INSIDE each
    // spawn_blocking closure rather than once at startup. reqwest's
    // blocking client owns an internal tokio runtime that must be
    // dropped on a blocking thread, never inside the async runtime.
    // Constructing it per-request keeps the lifetime entirely within
    // the spawn_blocking task. Cost: one client init per MCP call (~ms);
    // negligible given the once-per-day scheduler cadence and the
    // manual-search granularity.

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
        let resp = tokio::task::spawn_blocking(move || {
            // Build and drop the blocking client entirely within this
            // thread; see the note at the top of `main` for why.
            let client = match make_blocking_arxiv_fetcher() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "papers-plugin: reqwest build failed in worker");
                    return None;
                }
            };
            let fetcher = |url: &str| -> Result<String, ArxivError> {
                fetch_arxiv_with_retry(&client, url, MAX_RETRY_ATTEMPTS)
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
