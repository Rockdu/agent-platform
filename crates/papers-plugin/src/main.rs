//! `papers-plugin` MCP sidecar — scaffold stub.
//!
//! The `papers` plugin manifest declares this as its `command_bin`.
//! arXiv HTTP, Atom/XML parsing, SQLite writes, rate limiting, and
//! retry logic land in later rounds. The stub keeps the dev
//! diagnostics card quiet ("plugin binary not ready") and lets
//! `mcp_config` resolve the binary path during config generation.
//!
//! Behaviour matches `crates/notes-plugin`: reads any input from
//! stdin and exits cleanly on EOF. Does not speak MCP yet — if the
//! host tries to dispatch to it, the dispatcher's `NoSidecarWired`
//! shape applies because no real MCP handshake completes.

use std::io::{self, Read};

fn main() {
    let mut buf = [0u8; 4096];
    let mut stdin = io::stdin();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return, // EOF
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}
