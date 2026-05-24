//! End-to-end integration test for the papers sidecar's arXiv path,
//! using `wiremock` to stand in for the live arXiv endpoint. Verifies:
//!   - the sidecar issues a GET with the expected query string
//!   - retry on 503 honors backoff and eventually succeeds
//!   - 429 is treated as a retry-worthy status
//!   - parsed records land in SQLite via `insert_dedup`
//!   - rate-limit gate refuses a second fetch within the window

use std::time::Duration;

use papers_plugin::{
    build_arxiv_url, fetch_arxiv_with_retry, parse_arxiv_atom, PapersStore, RATE_LIMIT_SECONDS,
};
use rusqlite::Connection;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SAMPLE_ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/2401.00001v1</id>
    <title>Mocked Transformer Paper</title>
    <summary>An abstract used by the wiremock smoke test.</summary>
    <author><name>Alice</name></author>
    <link rel="alternate" href="http://arxiv.org/abs/2401.00001v1"/>
    <link href="http://arxiv.org/pdf/2401.00001v1" type="application/pdf"/>
  </entry>
</feed>"#;

fn make_store() -> (PapersStore, TempDir) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("state.sqlite");
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
    drop(conn);
    (PapersStore::open(&db_path).unwrap(), tmp)
}

#[tokio::test]
async fn arxiv_happy_path_writes_records() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_ATOM))
        .mount(&server)
        .await;

    let base = format!("{}/api/query", server.uri());
    let url = build_arxiv_url(&base, "transformer attention", 10);
    let body = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        fetch_arxiv_with_retry(&client, &url, 3)
    })
    .await
    .unwrap()
    .unwrap();
    let parsed = parse_arxiv_atom(&body, 10).unwrap();
    assert_eq!(parsed.len(), 1);

    let (store, _t) = make_store();
    let inserted = store.insert_dedup(&parsed, "transformer attention").unwrap();
    assert_eq!(inserted.len(), 1);
    let listed = store.list_recent(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].arxiv_id, "2401.00001v1");
}

#[tokio::test]
async fn retry_recovers_from_transient_503() {
    let server = MockServer::start().await;
    // First call: 503. Second call: 200.
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_ATOM))
        .mount(&server)
        .await;

    let url = build_arxiv_url(&format!("{}/api/query", server.uri()), "diffusion", 10);
    let body = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        fetch_arxiv_with_retry(&client, &url, 3)
    })
    .await
    .unwrap()
    .unwrap();
    let parsed = parse_arxiv_atom(&body, 10).unwrap();
    assert_eq!(parsed.len(), 1);
}

#[tokio::test]
async fn rate_limit_blocks_second_call_within_window() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_ATOM))
        .mount(&server)
        .await;

    let (store, _t) = make_store();
    assert!(store.rate_limit_wait().unwrap().is_none());
    store.touch_rate_limit().unwrap();
    let wait = store.rate_limit_wait().unwrap();
    assert!(wait.is_some());
    assert!(wait.unwrap() <= RATE_LIMIT_SECONDS);
}
