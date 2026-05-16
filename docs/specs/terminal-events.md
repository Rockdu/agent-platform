# Terminal Events Specification

## Overview

This document defines the canonical Terminal Mesh terminal event model for portable-pty Tokio actors, notification deduplication, scrollback ring-buffer behavior, and UTF-8/ANSI-safe slicing.

Each Terminal Mesh tab hosts one portable-pty process managed by a Tokio actor. Each PTY owns a bounded 1 MB output ring buffer. The ring buffer is used for scrollback snapshots, prompt detection, agent marker parsing, and notification context.

Default needs-attention triggers are:

- Child exit with code `0`
- Child exit with non-zero code
- Explicit agent marker via OSC sequence
- Prompt-waiting heuristic, when reliably detectable

`StderrBurst` is explicitly not a default needs-attention trigger. Heavy stderr output is common for legitimate tools, including `cargo build`, compilers, linters, package managers, and progress-reporting CLIs. Treating stderr volume as attention-worthy is false-positive prone and creates notification noise.

Cross-references:

- AC-2.4: ring buffer behavior
- AC-4.1: support for at least 4 concurrent PTYs
- AC-8.1: native notification and tray fire
- AC-8.2: default needs-attention trigger set
- AC-3.5: orchestrator semantic summary

## TerminalEvent Taxonomy

The terminal actor emits events using this canonical enum:

```rust
enum TerminalEvent {
    Output(bytes),
    Resize(cols, rows),
    Exit(code),
    NeedsAttention(AttentionKind),
    Cancelled,
}
```

`Output(bytes)` is emitted for raw PTY output bytes after parser inspection. Bytes remain suitable for forwarding to xterm.js.

`Resize(cols, rows)` is emitted when the terminal size changes.

`Exit(code)` is emitted when the child process exits. The exit code is also converted into a `NeedsAttention` event when applicable.

`NeedsAttention(AttentionKind)` is emitted for events that should be considered by the notification surface and tray.

`Cancelled` is emitted when a terminal task is intentionally cancelled by user or host action.

`AttentionKind` variants:

```rust
enum AttentionKind {
    Completion { exit_code: 0 },
    NonZeroExit { exit_code: i32 },
    PromptWaiting,
    AgentMarker {
        summary: Option<String>,
        severity: AttentionSeverity,
    },
}
```

`Completion { exit_code: 0 }` means the child exited successfully.

`NonZeroExit { exit_code }` means the child exited with a non-zero status.

`PromptWaiting` means the terminal appears to be at a prompt awaiting user input. This is heuristic and must follow the debounce behavior defined below.

`AgentMarker { summary, severity }` means an agent emitted an explicit task marker, typically from `claude` in the orchestrator terminal.

Explicitly out of the default set:

```rust
// Not a default AttentionKind.
StderrBurst
```

`StderrBurst` must not be enabled by default. It may exist later as an opt-in diagnostic or plugin-specific rule, but it is excluded from AC-8.2 default behavior because stderr-heavy tools often represent normal successful work.

## Event Payload Schema

Every emitted event carries the following envelope:

```rust
struct TerminalEventEnvelope {
    terminal_id: Uuid,
    plugin_id: String,
    timestamp: SystemTime,
    event: TerminalEvent,
}
```

`plugin_id` must always be:

```text
terminal_mesh
```

Variant-specific fields are embedded in the `TerminalEvent` payload.

For `NeedsAttention`, the envelope must additionally include:

```rust
struct NeedsAttentionPayload {
    event_id: Uuid,
    dedup_key: String,
    kind: AttentionKind,
}
```

`event_id` is a unique identifier for correlation and test assertions.

`dedup_key` is computed as:

```text
"{plugin_id}:{terminal_id}:{kind_name}"
```

Examples:

```text
terminal_mesh:550e8400-e29b-41d4-a716-446655440000:Completion
terminal_mesh:550e8400-e29b-41d4-a716-446655440000:NonZeroExit
terminal_mesh:550e8400-e29b-41d4-a716-446655440000:AgentMarker
```

For `AgentMarker`, `summary` is the human-readable semantic blurb, for example:

```text
标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐
```

`severity` must be one of:

```rust
enum AttentionSeverity {
    Info,
    NeedsConfirm,
    Error,
}
```

## OSC Agent Marker Sequence

The canonical OSC agent marker format is:

```text
ESC ] 1337 ; AgentTask ; <severity>;<base64-summary> BEL
```

Byte form:

```text
\x1b]1337;AgentTask;<severity>;<base64-summary>\x07
```

`AgentTask` is the Terminal Mesh custom sub-extension. The parser must only treat OSC `1337` sequences with the exact `AgentTask` subcommand as Terminal Mesh agent markers. Other OSC `1337` sequences must pass through unchanged.

`<severity>` must be one of:

```text
Info
NeedsConfirm
Error
```

`<base64-summary>` is a base64-encoded UTF-8 string.

The parser must:

- Extract `severity`
- Base64-decode `summary`
- Validate decoded summary as UTF-8
- Enforce a maximum total OSC payload size of 4 KB
- Truncate oversized summaries before encoding when emitted
- Append a truncation marker when truncation occurs

Recommended truncation marker:

```text
… [truncated]
```

The orchestrator should emit this OSC sequence when `claude` completes a task and has a semantic summary richer than a raw process exit code.

Example decoded event:

```rust
AttentionKind::AgentMarker {
    summary: Some("标记 3 封邮件为已读 + 新增 2 条 arXiv 推荐".to_string()),
    severity: AttentionSeverity::Info,
}
```

The parser must handle truncated or partial sequences across PTY reads. OSC parsing is stateful and must resume across multiple read calls.

Malformed OSC sequence behavior:

- No `NeedsAttention` event is emitted
- Parser state returns to normal after the malformed sequence terminates or exceeds the maximum payload
- Original bytes are treated as opaque terminal output and passed through to xterm.js
- xterm.js may ignore, render harmlessly, or apply standard OSC handling

## Prompt-Waiting Heuristic

Prompt waiting detection monitors the tail of the ring buffer for known shell prompt patterns.

Default bash/zsh prompt regexes:

```regex
\$\s*$
%\s*$
```

The regex is evaluated at the end of the current visible line in the ring buffer tail. The host may configure per-shell regexes.

Timing rule:

- Trigger only when a prompt pattern has been visible for at least 1 second
- No output may arrive during that 1 second debounce window
- If output arrives, the debounce timer resets
- Fast prompt-output-prompt cycles in scripts must not trigger notifications

Reliability caveat:

Prompt detection is not reliable for all shells or prompt systems. Custom `PS1`, multi-line prompts, terminal-aware prompts, async prompts, and prompt frameworks such as starship may defeat simple tail regex detection.

Default enablement:

- Enabled by default for detected bash and zsh using default prompt rules
- Disabled by default for non-default shells
- Host settings should allow disabling `PromptWaiting` per terminal, shell, or plugin

When unreliable, the host should disable `PromptWaiting` in plugin settings.

## Ring Buffer Policy

Each PTY owns exactly one bounded output ring buffer.

Capacity:

```text
1,048,576 bytes
```

This 1 MB capacity is a hard requirement per PTY.

Storage should use:

```rust
VecDeque<u8>
```

or:

```rust
Vec<u8>
```

with equivalent pop-front semantics.

The implementation must also maintain a synchronized safe-boundary index structure:

```rust
Vec<usize>
```

Safe-boundary indices should be tracked as logical byte offsets, not unstable physical positions, so overflow and wraparound do not invalidate the ability to find the nearest safe boundary efficiently.

Overflow policy is coalesce-on-overflow.

When the ring is full and new bytes arrive:

1. Compute the required dropped-byte count.
2. Drop oldest bytes up to the nearest safe boundary after the required dropped-byte count.
3. Push the new bytes.
4. Update UTF-8 and ANSI parser state.
5. Emit one `BufferTruncated { bytes_dropped }` status event for the coalesced overflow operation.

`BufferTruncated` is not a `NeedsAttention` event. It is a status event for diagnostics, debug views, and tests.

Recommended status shape:

```rust
struct BufferTruncated {
    terminal_id: Uuid,
    plugin_id: String,
    timestamp: SystemTime,
    bytes_dropped: usize,
}
```

Multiple byte drops caused by one incoming output chunk should coalesce into a single `BufferTruncated` event.

Snapshot reads:

```rust
fn read_scrollback(max_bytes: usize) -> String
```

`read_scrollback(max_bytes)` returns up to `max_bytes` from the tail of the ring buffer as a contiguous `String`.

Snapshot slicing rules:

- Start position rounds forward to the nearest safe boundary
- End position rounds backward to the nearest safe boundary
- Returned bytes must be valid UTF-8
- Returned content must not begin or end in the middle of an ANSI escape sequence
- If no valid safe slice exists, return an empty string

## UTF-8 / ANSI Boundary Safety

A safe boundary is a byte index that is both:

- Between UTF-8 code points
- Outside ANSI escape sequence parsing

The implementation must never slice, drop, or coalesce in the middle of:

- A UTF-8 multi-byte code point
- An ANSI escape sequence
- An OSC string
- A CSI sequence

The scanner maintains `last_safe_index` as bytes arrive.

`last_safe_index` may advance only when:

- ANSI parser state is `Ground`
- Current byte position is a UTF-8 code-point boundary

UTF-8 boundary check:

```rust
byte & 0xC0 != 0x80
```

A continuation byte has the form:

```rust
byte & 0xC0 == 0x80
```

Minimal ANSI parser states:

```rust
enum AnsiState {
    Ground,
    Escape,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiFinal,
    OscEntry,
    OscString,
    OscEsc,
}
```

State behavior:

- `Ground`: normal printable/control byte handling
- `Escape`: entered after `ESC`
- `CsiEntry`: entered after `ESC [`
- `CsiParam`: consuming CSI parameter bytes
- `CsiIntermediate`: consuming CSI intermediate bytes
- `CsiFinal`: CSI final byte observed; next state is `Ground`
- `OscEntry`: entered after `ESC ]`
- `OscString`: consuming OSC payload until `BEL` or `ESC \`
- `OscEsc`: observed `ESC` inside OSC, waiting for string terminator or continuation

The ANSI parser state machine must resume across PTY reads.

Overflow drop behavior:

- Required drop count rounds up to the next safe boundary
- The buffer head must never be left at a UTF-8 continuation byte
- The buffer head must never be left inside an ANSI escape sequence
- If the scanner cannot find a safe boundary within the buffered content, it may drop the full buffer and report the full dropped byte count

## Dedup Arbiter Behavior (consumed by AC-8.1/AC-8.4)

The notification dedup arbiter consumes `NeedsAttention` events.

Dedup key:

```text
(plugin_id, terminal_id, event_kind)
```

`event_kind` examples:

```text
Completion
NonZeroExit
PromptWaiting
AgentMarker
```

The canonical `dedup_key` string is:

```text
"{plugin_id}:{terminal_id}:{kind_name}"
```

Window:

```text
2000 ms
```

The 2000 ms window is configurable in production, but tests must use exactly 2000 ms.

Behavior:

- The first event for a key within a window fires a notification
- Subsequent events for the same key inside the window do not spawn notifications
- Suppressed events increment an internal pending counter
- At the window boundary, a new event starts a new notification window
- Different `terminal_id` values do not dedup against each other
- Different `event_kind` values do not dedup against each other

Window boundary rule:

```text
elapsed < 2000 ms   => suppress
elapsed >= 2000 ms  => fire new notification
```

Suppressed-events log:

- Kept in the plugin debug view
- Not user-visible by default
- Should include timestamp, dedup key, suppressed count, and last suppressed event id

## Notification Payload Shape (consumed by Notification Surface)

For `Completion`:

```text
title: "Terminal <name> 完成"
subtitle: "exit 0"
```

For `NonZeroExit { exit_code }`:

```text
title: "Terminal <name> 失败"
subtitle: "exit <code>"
```

For `PromptWaiting`:

```text
title: "Terminal <name> 等待输入"
```

For `AgentMarker { summary, severity }`:

```text
title: "<severity_label> · claude"
subtitle: summary
```

Severity labels:

```text
Info         => "Info"
NeedsConfirm => "Needs confirmation"
Error        => "Error"
```

For macOS native notifications, `summary` should be truncated to approximately 120 characters.

The tray entry must retain the full available summary.

Tray behavior:

- Group entries by `terminal_id`
- Sort groups and entries by timestamp descending
- Show suppressed counts only in debug or expanded diagnostic views
- Use the newest event timestamp for group ordering

## Test Hooks

Integration tests may use:

```rust
TerminalDebug::inject_event(event)
```

`TerminalDebug::inject_event(event)` must be available only under:

```rust
cfg(test)
```

or an explicit test/debug feature flag.

The debug injection path should pass through the same dedup arbiter and notification payload builder as real events.

Agent marker tests should use a Rust OSC sequence simulator helper.

The helper should support:

- Single-read complete OSC sequence
- OSC sequence split across multiple reads
- Malformed severity
- Malformed base64
- Oversized payload
- OSC terminated by `BEL`
- OSC terminated by `ESC \`
- Adjacent normal terminal output before and after the OSC sequence

Required test assertions:

- `StderrBurst` is not emitted by default
- Exit `0` emits `Completion`
- Non-zero exit emits `NonZeroExit`
- Prompt debounce requires at least 1 second without output
- Dedup suppresses repeated same-key events inside 2000 ms
- Dedup fires again at `>= 2000 ms`
- Ring overflow emits `BufferTruncated` but not `NeedsAttention`
- Scrollback snapshots never split UTF-8 code points
- Scrollback snapshots never split ANSI or OSC sequences
- Partial OSC agent marker resumes across reads

## DOs / DON'Ts

DO keep the PTY ring buffer capped at exactly 1,048,576 bytes per PTY.

DO preserve UTF-8 and ANSI boundaries when dropping, slicing, or coalescing output.

DO emit `Completion` for child exit code `0`.

DO emit `NonZeroExit` for child exit codes other than `0`.

DO emit `AgentMarker` only from a valid OSC agent marker sequence.

DO treat malformed OSC agent markers as normal opaque output.

DO debounce `PromptWaiting` for at least 1 second without output.

DO disable prompt detection when shell or prompt configuration is unreliable.

DO dedup notifications by `(plugin_id, terminal_id, event_kind)`.

DO keep suppressed notification details in the plugin debug view.

DON'T make `StderrBurst` a default needs-attention trigger.

DON'T emit user-visible notifications for `BufferTruncated`.

DON'T slice scrollback in the middle of UTF-8 code points.

DON'T slice scrollback in the middle of ANSI escape sequences.

DON'T assume OSC sequences arrive in a single PTY read.

DON'T let one terminal's events dedup against another terminal's events.

DON'T replace semantic `AgentMarker` summaries with generic `exit 0` text when a richer summary is available.
