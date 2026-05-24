//! Binary-level MCP integration test for the papers-plugin sidecar.
//!
//! Spawns the compiled `papers-plugin` binary as a subprocess with:
//!   - `APP_DATA_DIR` pointing at a tempdir so the sidecar opens an
//!     isolated SQLite at `${tmp}/plugins/papers/state.sqlite`.
//!   - `PAPERS_ARXIV_BASE_OVERRIDE` pointing at a `wiremock` server so
//!     the sidecar's HTTP layer hits the mock, not real arXiv.
//!
//! Sends MCP messages (`initialize`, `tools/list`, `tools/call`) on
//! stdin and reads JSON-RPC responses from stdout. Verifies:
//!   - `tools/list` returns the expected papers tool names.
//!   - `papers.fetch {query}` returns at least one paper and writes
//!     to the sidecar's SQLite (opt-in is seeded directly via SQLite
//!     because `papers.set_opt_in` is intentionally NOT an MCP tool
//!     — the WRITE lives behind the host UI prompt so the
//!     orchestrator Claude cannot enable arXiv context queries on
//!     the user's behalf).
//!   - Bypassing opt-in (calling `papers.fetch` without prior set_opt_in)
//!     returns a `not opted in` error.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SAMPLE_ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <title>Binary-Boundary Mocked Paper</title>
    <summary>End-to-end sidecar test fixture.</summary>
    <author><name>Alice</name></author>
    <link rel="alternate" href="http://arxiv.org/abs/2401.00001v1"/>
    <link href="http://arxiv.org/pdf/2401.00001v1" type="application/pdf"/>
  </entry>
</feed>"#;

fn locate_sidecar_binary() -> PathBuf {
    // Cargo runs integration tests from the crate's manifest dir; the
    // binary lives at `<workspace>/target/<profile>/papers-plugin`.
    // CARGO_BIN_EXE_<name> is automatically set by cargo for tests in
    // the same package as the binary target.
    let env_key = "CARGO_BIN_EXE_papers-plugin";
    let p = std::env::var(env_key).unwrap_or_else(|_| {
        panic!("{env_key} not set — integration test must be run via cargo")
    });
    PathBuf::from(p)
}

fn prepare_papers_db(app_data_dir: &std::path::Path) {
    let plugin_dir = app_data_dir.join("plugins").join("papers");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let db_path = plugin_dir.join("state.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE daily_recommendations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            arxiv_id TEXT NOT NULL,
            title TEXT NOT NULL,
            authors TEXT NOT NULL,
            abstract_snippet TEXT NOT NULL,
            pdf_url TEXT NOT NULL,
            abs_url TEXT NOT NULL,
            source_query TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'scheduled',
            fetched_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE user_paper_state (
            arxiv_id TEXT PRIMARY KEY,
            starred INTEGER NOT NULL DEFAULT 0,
            read_at TEXT
        );
        CREATE TABLE scheduler_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_fired_at TEXT,
            last_arxiv_call_at TEXT,
            last_error TEXT,
            opt_in_enabled INTEGER
        );
        INSERT INTO scheduler_state (id) VALUES (1);
        "#,
    )
    .unwrap();
}

/// Spawn the sidecar with `APP_DATA_DIR` + `PAPERS_ARXIV_BASE_OVERRIDE`
/// set. Send the provided MCP messages on stdin; collect responses.
/// Returns the parsed response array (in the order they were read) and
/// shuts the subprocess down by closing stdin.
fn run_sidecar_with_messages(
    app_data: &std::path::Path,
    arxiv_base: &str,
    messages: &[Value],
) -> Vec<Value> {
    let bin = locate_sidecar_binary();
    let mut child = Command::new(&bin)
        .arg("--client-id")
        .arg("claude:00000000-0000-0000-0000-000000000000:papers")
        .arg("--workspace")
        .arg(app_data) // any existing dir works for --workspace
        .env("APP_DATA_DIR", app_data)
        .env("PAPERS_ARXIV_BASE_OVERRIDE", arxiv_base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn papers-plugin");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for msg in messages {
            let serialised = serde_json::to_string(msg).expect("serialise mcp frame");
            stdin.write_all(serialised.as_bytes()).expect("write");
            stdin.write_all(b"\n").expect("newline");
        }
        stdin.flush().expect("flush stdin");
    }

    // Drop stdin to signal EOF — the sidecar then exits on the next
    // read. We do this in a separate scope so the stdin handle is
    // closed before we read all of stdout.
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .expect("sidecar wait_with_output");
    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    if body.lines().filter(|l| !l.trim().is_empty()).count() == 0 {
        eprintln!("[sidecar stderr]\n{err}");
        eprintln!("[sidecar exit] {:?}", output.status);
    }
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("response line is valid JSON"))
        .collect()
}

#[tokio::test]
async fn sidecar_binary_initialize_tools_list_fetch_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_ATOM))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    prepare_papers_db(tmp.path());
    let arxiv_base = format!("{}/api/query", server.uri());

    // We can't await inside the spawn_blocking because wiremock is async.
    // Instead, capture the URL and drive the sidecar from a blocking task.
    let arxiv_base_owned = arxiv_base.clone();
    let tmp_path = tmp.path().to_path_buf();
    // Seed opt-in directly via SQLite — `papers.set_opt_in` is NOT
    // an MCP tool (it must stay user-confirmed via the host UI),
    // and the binary boundary test still needs to exercise the
    // opt-in-required code paths.
    {
        let conn = Connection::open(tmp.path().join("plugins/papers/state.sqlite")).unwrap();
        conn.execute(
            "UPDATE scheduler_state SET opt_in_enabled = 1 WHERE id = 1",
            [],
        )
        .unwrap();
    }
    let responses = tokio::task::spawn_blocking(move || {
        let messages = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "papers.fetch",
                    "arguments": { "query": "transformer attention" }
                }
            }),
        ];
        run_sidecar_with_messages(&tmp_path, &arxiv_base_owned, &messages)
    })
    .await
    .unwrap();

    assert_eq!(responses.len(), 3, "expected one response per request");

    // initialize: result includes serverInfo.name = "papers-plugin"
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"], "papers-plugin",
        "initialize response: {}",
        responses[0]
    );

    // tools/list: orchestrator-visible tools only. `papers.fetch` is
    // intentionally absent so an ad-hoc orchestrator request cannot
    // advance scheduler state (the host scheduler still calls it via
    // unlisted `tools/call`). `papers.set_opt_in` is intentionally
    // absent so the model cannot flip the opt-in flag.
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "papers.list_recent",
        "papers.search",
        "papers.toggle_star",
        "papers.mark_read",
        "papers.get_opt_in",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    for forbidden in ["papers.fetch", "papers.set_opt_in"] {
        assert!(
            !names.contains(&forbidden),
            "{forbidden} must NOT be advertised in tools/list; got: {names:?}"
        );
    }

    // fetch succeeded with the mocked arXiv response (papers.fetch
    // still routes via `tools/call` — only the discovery path is
    // hidden from the orchestrator).
    assert_eq!(responses[2]["result"]["new_count"], 1);
    let papers = responses[2]["result"]["papers"].as_array().unwrap();
    assert_eq!(papers[0]["arxivId"], "2401.00001v1");

    // SQLite state reflects the insertion
    let conn = Connection::open(tmp.path().join("plugins/papers/state.sqlite")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_recommendations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
    let opt_in: i64 = conn
        .query_row(
            "SELECT opt_in_enabled FROM scheduler_state WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(opt_in, 1);
}

#[tokio::test]
async fn sidecar_binary_rejects_fetch_when_opt_in_not_set() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_ATOM))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    prepare_papers_db(tmp.path());
    let arxiv_base = format!("{}/api/query", server.uri());

    let tmp_path = tmp.path().to_path_buf();
    let arxiv_base_owned = arxiv_base.clone();
    let responses = tokio::task::spawn_blocking(move || {
        let messages = vec![json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "papers.fetch",
                "arguments": { "query": "transformer" }
            }
        })];
        run_sidecar_with_messages(&tmp_path, &arxiv_base_owned, &messages)
    })
    .await
    .unwrap();

    assert_eq!(responses.len(), 1);
    let err = responses[0]["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        err.contains("not opted in"),
        "expected not-opted-in error, got: {err}"
    );

    // arXiv must not have been contacted at all.
    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "opt-in gate must block ALL network IO; received {} requests",
        received.len()
    );

    // SQLite has no recommendations.
    let conn = Connection::open(tmp.path().join("plugins/papers/state.sqlite")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_recommendations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);

    let _ = Duration::from_secs(0); // touch unused import warning suppressor
}
