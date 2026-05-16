# Ask Codex Input

## Question

Produce the complete content of `docs/specs/terminal-events.md` — the canonical spec for terminal needs-attention event taxonomy + dedup keys + ring buffer overflow policy + UTF-8/ANSI boundary safety. Downstream consumer: task15 (portable-pty Tokio actor implementation), task22 (notification surface dedup arbiter), task23 (orchestrator semantic-summary notification).

# Locked context

Each Terminal Mesh tab hosts a portable-pty + Tokio actor with a **1 MB bounded ring buffer per PTY** (HARD requirement). UTF-8 multi-byte and ANSI escape sequence boundaries MUST be respected when slicing or coalescing buffer contents. Default needs-attention triggers: child exit (zero), child exit (nonzero), explicit agent marker via OSC sequence, prompt-waiting heuristic (if reliably detectable). **Stderr-burst is NOT a default trigger.** Notification dedup keyed by `(plugin_id, terminal_id, event_kind)` with ~2s window. Orchestrator's `claude` task completion gets a semantic summary (richer than "exit 0") via OSC sequence or MCP notification.

Target AC:
- **AC-8.2**: Default needs-attention trigger set; stderr-burst explicitly excluded.

(Cross-references: AC-2.4 ring buffer behavior, AC-4.1 ≥4 concurrent PTYs, AC-8.1 native notification + tray fire, AC-3.5 orchestrator semantic summary.)

# Required Output

Sections (use exact `##` headings):

## Overview

## TerminalEvent Taxonomy
- Enum: `TerminalEvent { Output(bytes), Resize(cols, rows), Exit(code), NeedsAttention(AttentionKind), Cancelled }`.
- `AttentionKind` variants:
  - `Completion { exit_code: 0 }` — child exited with zero.
  - `NonZeroExit { exit_code: i32 }` — child exited with non-zero.
  - `PromptWaiting` — shell is at prompt awaiting input (heuristic; see "Prompt-Waiting Heuristic" below).
  - `AgentMarker { summary: Option<String>, severity: AttentionSeverity }` — emitted by an agent (typically claude) via OSC sequence.
- Explicitly OUT of the default set: `StderrBurst`. The spec MUST document why (false-positive prone; some legitimate tools log heavily to stderr — e.g., cargo build).

## Event Payload Schema
- Each event carries: `terminal_id: Uuid`, `plugin_id: String` (always "terminal_mesh"), `timestamp: SystemTime`, plus variant-specific fields.
- For `NeedsAttention`: also `event_id: Uuid` for dedup correlation, `dedup_key: String` (computed as `"{plugin_id}:{terminal_id}:{kind_name}"`).
- For `AgentMarker`: `summary` is the human-readable semantic blurb (e.g., "标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐"); `severity` ∈ {Info, NeedsConfirm, Error}.

## OSC Agent Marker Sequence
- Format: `ESC ] 1337 ; AgentTask ; <severity>;<base64-summary> BEL`. (Propose a concrete OSC sequence number not conflicting with iTerm2's allocations; or use a custom sub-extension.)
- Parser MUST extract `severity` and base64-decode `summary` (max 4 KB total payload to fit in a single OSC; truncate with marker if longer).
- Emitted by claude when it completes a task in the orchestrator tab.
- Parser MUST handle truncated / partial sequences across buffer reads (state machine resumes across multiple read calls).
- Failure: malformed OSC sequence → no event emitted; sequence is treated as opaque bytes passed through to xterm.js (which will render it harmlessly or with iTerm2's standard handling).

## Prompt-Waiting Heuristic
- Detection strategy: monitor for a known shell prompt pattern in the tail of the ring buffer (configurable per-shell regex; default for bash/zsh: `\\$\\s*$` or `%\\s*$` at end of line).
- Heuristic timing: trigger ONLY when the prompt has been visible for ≥1 second without any output (debounce; avoid triggering during fast prompt-output-prompt cycles in scripts).
- Reliability caveat: the heuristic is NOT reliable for all shells/prompts (custom PS1, multi-line prompts, terminal-aware prompts like starship). When unreliable, the host SHOULD disable PromptWaiting in plugin settings.
- Disabled by default for non-default shells; enabled by default for bash/zsh.

## Ring Buffer Policy
- Capacity: 1,048,576 bytes (1 MB) per PTY (HARD).
- Storage: `VecDeque<u8>` (or `Vec<u8>` with pop_front semantics) plus a `Vec<usize>` of safe-boundary indices (kept in sync to enable O(1) safe slice retrieval).
- Overflow policy: **coalesce-on-overflow** — when ring is full and new bytes arrive:
  1. Drop oldest bytes up to the nearest safe boundary AFTER the dropped-bytes count requested.
  2. Push new bytes (which may themselves trigger another safe-boundary computation).
  3. Emit a single `BufferTruncated { bytes_dropped }` event (NOT a NeedsAttention — this is a status event).
- Snapshot reads: `read_scrollback(max_bytes)` returns up to `max_bytes` of the buffer's tail content as a contiguous `String`, sliced at safe boundaries.

## UTF-8 / ANSI Boundary Safety
- "Safe boundary" = byte index between code points AND between ANSI escape sequences (NOT in the middle of an escape).
- Scanner: maintains a `last_safe_index` as bytes arrive; advances when:
  - Current state machine is in `Ground` (not consuming an escape sequence).
  - Current byte is a UTF-8 code-point boundary (`& 0xC0 != 0x80`).
- ANSI parser state machine (minimal): `Ground`, `Escape`, `CsiEntry`, `CsiParam`, `CsiIntermediate`, `CsiFinal`, `OscEntry`, `OscString`, `OscEsc`. Resume across buffer reads.
- When overflow drops bytes, the drop count rounds UP to the next safe boundary (never leaves a half-escape or half-codepoint at the buffer head).

## Dedup Arbiter Behavior (consumed by AC-8.1/AC-8.4)
- Key: `(plugin_id, terminal_id, event_kind)` where `event_kind` is e.g. `"Completion"`, `"NonZeroExit"`, `"AgentMarker"`.
- Window: 2000 ms (configurable; tests use 2000 ms exactly).
- Behavior: within window, only the FIRST event of the same key fires; subsequent events update an internal "pending" counter but do NOT spawn additional notifications.
- Edge case: at window boundary, a new event becomes a new notification.
- Suppressed-events log: kept in plugin debug view (not user-visible by default).

## Notification Payload Shape (consumed by Notification Surface)
- For `Completion`: title `"Terminal <name> 完成"`, subtitle `"exit 0"`.
- For `NonZeroExit { exit_code }`: title `"Terminal <name> 失败"`, subtitle `"exit <code>"`.
- For `PromptWaiting`: title `"Terminal <name> 等待输入"`.
- For `AgentMarker { summary, severity }`: title `"<severity_label> · claude"`, subtitle = `summary` (truncated to ~120 chars in macOS notification; full text in tray entry).
- Tray window groups by terminal_id; sorts by timestamp descending.

## Test Hooks
- A debug command `TerminalDebug::inject_event(event)` for integration tests (only available with `cfg(test)` or feature flag).
- An OSC sequence simulator in a Rust test helper for AgentMarker parsing tests.

## DOs / DON'Ts

# Output Format

Output ONLY the markdown content in stdout. Do NOT use any filesystem write tool to save the file yourself — the caller will save your stdout to `docs/specs/terminal-events.md`. Start with `# Terminal Events Specification`. No preamble, no "Saved to..." summary. Just the spec content.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_11-36-43
- Tool: codex
