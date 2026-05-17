//! `notes-plugin` MCP sidecar — dev stub.
//!
//! The `example-notes` plugin manifest declares this as its
//! `command_bin`. Until a real notes MCP sidecar is implemented
//! (post-MVP), this stub keeps the dev-diagnostics card quiet
//! ("plugin binary not ready") and lets `mcp_config` resolve the
//! binary path during config generation.
//!
//! The stub reads any input from stdin and exits cleanly on EOF.
//! It does NOT speak MCP — if the host actually tries to dispatch
//! to it, the dispatcher's `NoSidecarWired` shape applies because
//! no real MCP handshake completes.

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
