//! Context extraction for the daily papers digest.
//!
//! Synthesises a short keyword list from the user's recent local
//! activity so the papers sidecar can query arXiv for relevant
//! recommendations. Strictly bounded — every input source has a byte
//! cap, every subprocess has a timeout, and the output is a list of
//! short tokens (≤ `MAX_TOKENS_PER_QUERY`), never raw conversation
//! content or file paths.
//!
//! Privacy contract enforced here:
//!   - Reads only the *tail* of `.claude/*.jsonl` files (last N bytes).
//!   - Extracts only ASCII identifier-shaped tokens (length 4..=24).
//!   - Filters a small built-in stop-word list (common code/English words).
//!   - Drops anything that looks like an email address, URL, path, or
//!     base64-ish secret.
//!   - Caps the final list at `MAX_TOKENS_PER_QUERY` tokens.
//!
//! Opt-in gating lives one layer above (the scheduler refuses to call
//! into this module unless `opt_in_enabled = 1` in the papers SQLite).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub const CONTEXT_BUDGET_BYTES_TOTAL: usize = 256 * 1024;
pub const MAX_TOKENS_PER_QUERY: usize = 30;
pub const GIT_LOG_TIMEOUT_MS: u64 = 1500;
pub const MAX_GIT_COMMITS: usize = 20;
/// Workspaces whose `.claude/` (or, if absent, workspace dir) has had
/// no filesystem activity within this window are skipped entirely —
/// no `.claude` read, no git mine. The plan's privacy contract is to
/// only surface recent work; stale workspace tokens must not enter
/// the arXiv query.
pub const STALE_WORKSPACE_THRESHOLD_SECS: u64 = 30 * 86400;
/// Wall-clock deadline for the entire `extract_workspace_keywords()` run
/// across all workspaces. Chosen comfortably below the plan's "<2 seconds
/// total" context-extraction contract so a slow git or filesystem read
/// on workspace N cannot push the aggregate past 2 s.
pub const CONTEXT_TOTAL_TIMEOUT_MS: u64 = 1800;
/// Below this many remaining ms it is not worth spawning a `git log`
/// subprocess — the spawn + drain overhead alone can exceed the budget.
const GIT_SPAWN_FLOOR_MS: u64 = 150;

/// Returns true when the workspace has had no recent Claude or
/// filesystem activity within `threshold_secs`. Pure function so the
/// stale-filter behavior is unit-testable without time-mocking.
///
/// Probe order:
///   1. Newest regular file mtime inside `<ws>/.claude` (capped at
///      64 entries to bound IO). This catches the case where Claude
///      appends to an existing `*.jsonl` — the file mtime bumps even
///      though the directory entry list does not change, so the dir
///      mtime alone would falsely report a long-lived active
///      workspace as stale.
///   2. `<ws>/.claude` directory mtime — fallback when the directory
///      exists but cannot be scanned, or is empty.
///   3. Workspace directory mtime — fallback when `.claude/` doesn't
///      exist (a freshly-created workspace still gets a chance to
///      contribute git history).
///   4. If none can be stat'd → return `true` (skip).
pub fn workspace_is_stale(ws: &Path, now: SystemTime, threshold_secs: u64) -> bool {
    let mtime = newest_claude_file_mtime(ws)
        .or_else(|| {
            std::fs::metadata(ws.join(".claude"))
                .and_then(|m| m.modified())
                .ok()
        })
        .or_else(|| std::fs::metadata(ws).and_then(|m| m.modified()).ok());
    let Some(mtime) = mtime else {
        return true;
    };
    match now.duration_since(mtime) {
        Ok(age) => age.as_secs() > threshold_secs,
        // mtime is in the future (clock skew). Treat as fresh — better
        // a false negative than over-aggressive redaction here.
        Err(_) => false,
    }
}

/// Newest mtime among regular files inside `<ws>/.claude` AND
/// `~/.claude/projects/<flattened-workspace-path>/`, capped at 64
/// entries per directory (mirroring the `count` cap in
/// `extract_keywords_from_claude_dir`). Returns `None` when neither
/// dir exists, can be read, or contains readable files.
///
/// Including the global path is critical: Claude Code stores per-
/// project conversation logs there (not inside the workspace), so a
/// workspace whose local `.claude/` is absent or stale can still
/// reflect very recent activity through the global jsonl mtime.
fn newest_claude_file_mtime(ws: &Path) -> Option<SystemTime> {
    let mut probe_dirs: Vec<PathBuf> = vec![ws.join(".claude")];
    if let Some(global) = claude_projects_dir_for(ws) {
        probe_dirs.push(global);
    }
    let mut newest: Option<SystemTime> = None;
    for dir in &probe_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut count = 0usize;
        for entry in entries.flatten() {
            count += 1;
            if count > 64 {
                break;
            }
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                newest = Some(match newest {
                    Some(cur) if cur >= mtime => cur,
                    _ => mtime,
                });
            }
        }
    }
    newest
}

/// Decides how to schedule the next `mine_git_commits` call given the
/// wall-clock ms remaining before the global extraction deadline.
/// Returns `None` when the remaining budget is too small to be worth
/// spawning a git subprocess, or `Some(timeout_ms)` otherwise. The
/// returned timeout is the smaller of the remaining budget and the
/// per-call `GIT_LOG_TIMEOUT_MS` ceiling.
pub fn git_timeout_for_remaining(remaining_ms: u64) -> Option<u64> {
    if remaining_ms < GIT_SPAWN_FLOOR_MS {
        None
    } else {
        Some(remaining_ms.min(GIT_LOG_TIMEOUT_MS))
    }
}

/// Stop-word allow-list. Tokens that match are dropped before the
/// keyword set is returned. Kept short and lowercase; intentionally
/// biased toward generic code/English words so domain identifiers
/// survive.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "this", "that", "have", "been", "will", "your",
    "code", "test", "true", "false", "null", "none", "some", "self", "type", "data", "file",
    "value", "string", "result", "error", "should", "would", "could", "about", "after", "before",
    "every", "first", "their", "there", "where", "while", "which", "want", "need", "make", "made",
    "tests", "files", "main", "name", "user", "host", "client", "server", "config", "options",
    "import", "export", "module", "function", "return", "const", "static", "public", "private",
    "claude", "tool", "tools", "task", "tasks", "args", "params", "json", "yaml", "rust", "tsx",
    "typescript", "javascript",
];

/// Returns the synthesized keyword query string for an iterable of
/// workspace paths. Pure orchestration: this function performs IO but
/// does not touch the network. The output is suitable to pass directly
/// as the arXiv `query` argument.
///
/// `now_for_age_filter` is used to skip workspaces whose `.claude/`
/// directory has not been touched in the past 30 days. Pass
/// `Instant::now()` from production; tests inject deterministic values.
pub fn extract_workspace_keywords<P: AsRef<Path>>(
    workspaces: &[P],
    total_budget_bytes: usize,
) -> Vec<String> {
    extract_workspace_keywords_with_git(workspaces, total_budget_bytes, "git")
}

/// Injectable variant that lets tests substitute the git binary
/// (typically a tempfile shell script) so the subprocess-timeout
/// regression test can run without touching the process-wide PATH
/// — global PATH mutation races other parallel tests in this crate.
pub fn extract_workspace_keywords_with_git<P: AsRef<Path>>(
    workspaces: &[P],
    total_budget_bytes: usize,
    git_bin: &str,
) -> Vec<String> {
    if workspaces.is_empty() {
        return Vec::new();
    }
    // Per-workspace budget: floor at 4KB so a single workspace still reads
    // something useful, but cap the TOTAL across all workspaces at
    // `total_budget_bytes` by tracking the remaining budget as we go.
    let per_workspace_floor: usize = 4096;
    let per_workspace_target = total_budget_bytes / workspaces.len().max(1);
    let mut remaining_budget = total_budget_bytes;

    // Wall-clock deadline for the entire run. Even if each
    // `mine_git_commits` call respects `GIT_LOG_TIMEOUT_MS` per
    // subprocess, N slow workspaces × 1.5 s would blow past the
    // plan's "<2 seconds total" contract. We enforce a single
    // deadline at the top of every iteration and again before
    // every git spawn; the git timeout passed in is the smaller
    // of the per-call cap and the remaining wall-clock budget.
    let deadline = Instant::now() + Duration::from_millis(CONTEXT_TOTAL_TIMEOUT_MS);

    let mut all_tokens: HashSet<String> = HashSet::new();
    let now_for_age = SystemTime::now();
    for ws in workspaces {
        let ws = ws.as_ref();
        if remaining_budget == 0 {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        // Stale-workspace filter: skip both `.claude` extraction AND
        // git mining when the workspace has no recent activity
        // signal. This honours the plan's privacy contract and
        // matches the doc-comment promise on this function.
        if workspace_is_stale(ws, now_for_age, STALE_WORKSPACE_THRESHOLD_SECS) {
            continue;
        }
        let take = per_workspace_target.min(remaining_budget).max(
            per_workspace_floor.min(remaining_budget),
        );
        let claude_tokens = extract_keywords_from_claude_dir(ws, take);
        remaining_budget = remaining_budget.saturating_sub(take);
        for t in claude_tokens {
            all_tokens.insert(t);
        }
        if all_tokens.len() >= MAX_TOKENS_PER_QUERY * 2 {
            break;
        }
        // Compute remaining wall-clock budget before spawning git.
        // Skip git entirely if there is not enough headroom for the
        // spawn + drain cycle.
        let remaining_ms = deadline
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let Some(git_timeout) = git_timeout_for_remaining(remaining_ms) else {
            break;
        };
        if let Ok(subjects) =
            mine_git_commits_with_git(git_bin, ws, MAX_GIT_COMMITS, git_timeout)
        {
            for s in subjects {
                let redacted = redact_sensitive_spans(&s);
                for t in tokenize(&redacted) {
                    if is_useful_token(&t) {
                        all_tokens.insert(t);
                    }
                }
            }
        }
        if all_tokens.len() >= MAX_TOKENS_PER_QUERY * 2 {
            break;
        }
    }

    let mut ranked: Vec<String> = all_tokens.into_iter().collect();
    ranked.sort();
    ranked.truncate(MAX_TOKENS_PER_QUERY);
    ranked
}

/// Read up to `byte_budget` bytes from the largest-mtime `*.jsonl` file
/// under `<workspace>/.claude/` AND `~/.claude/projects/<flattened-workspace-path>/`,
/// tokenize, and return useful tokens.
///
/// Claude Code stores per-project conversation logs at the GLOBAL
/// path (a flattened directory under the user's home `.claude` tree),
/// not inside the workspace itself. Older workspaces that pre-date
/// the per-project-claude convention will still have a local
/// `<ws>/.claude/` dir — both paths are probed and the newest jsonl
/// across both wins. Returns an empty Vec if neither dir has any
/// readable jsonl file.
pub fn extract_keywords_from_claude_dir(workspace: &Path, byte_budget: usize) -> Vec<String> {
    let mut probe_dirs: Vec<PathBuf> = Vec::new();
    let local = workspace.join(".claude");
    if local.is_dir() {
        probe_dirs.push(local);
    }
    if let Some(global) = claude_projects_dir_for(workspace) {
        if global.is_dir() {
            probe_dirs.push(global);
        }
    }
    if probe_dirs.is_empty() {
        return Vec::new();
    }
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for dir in &probe_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut count = 0usize;
        for entry in entries.flatten() {
            count += 1;
            if count > 64 {
                break;
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = entry.metadata().and_then(|m| m.modified()).ok();
            if let Some(mt) = mtime {
                match &newest {
                    Some((_, cur)) if *cur >= mt => {}
                    _ => newest = Some((path, mt)),
                }
            }
        }
    }
    let Some((path, _)) = newest else {
        return Vec::new();
    };
    let Some(content) = read_tail(&path, byte_budget) else {
        return Vec::new();
    };
    let redacted = redact_sensitive_spans(&content);
    let mut out: HashSet<String> = HashSet::new();
    for tok in tokenize(&redacted) {
        if is_useful_token(&tok) {
            out.insert(tok);
        }
    }
    out.into_iter().collect()
}

/// Map a workspace path to Claude Code's global per-project jsonl
/// directory: `~/.claude/projects/<flattened-path>/`. The flattening
/// rule is "every non-ASCII-alphanumeric character → `-`" (per-char
/// after UTF-8 decoding; no collapsing of consecutive replacements).
/// Returns `None` if `$HOME` is unset.
fn claude_projects_dir_for(workspace: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let abs = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let flattened: String = abs
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(home.join(".claude").join("projects").join(flattened))
}

/// Read at most `byte_budget` bytes from the end of `path`.
fn read_tail(path: &Path, byte_budget: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let want = byte_budget as u64;
    let start = if len > want { len - want } else { 0 };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(byte_budget);
    f.take(byte_budget as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Whole-span sanitizer. Replaces matched spans (file paths, URLs,
/// email addresses, SSH-style git URLs, API-key-shaped tokens, common
/// API-key prefixes) with a single space BEFORE tokenization so the
/// component words of a sensitive span cannot leak into the arXiv
/// query. `tokenize()` only splits on punctuation; without this pass
/// `/Users/alice/secretProject/...` would contribute `alice`,
/// `secretProject`, `Users`, `src`, etc. as keywords.
///
/// Each pattern is matched, the match is replaced with a space, and
/// the remaining text continues through tokenization. Patterns are
/// applied in order; later patterns operate on the result of earlier
/// ones (so e.g. a URL whose path component would otherwise look like
/// a multi-segment path is already gone before the path pattern runs).
pub fn redact_sensitive_spans(input: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;

    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let raw = [
            // Schemed URLs (http(s)://, ws(s)://, ftp://, custom-scheme://...).
            r"[A-Za-z][A-Za-z0-9+\-.]*://[^\s]+",
            // SSH-shaped git URLs: git@github.com:owner/repo.git
            r"\b[A-Za-z_][A-Za-z0-9_\-]*@[A-Za-z0-9.\-]+:[A-Za-z0-9_./\-]+",
            // Email addresses.
            r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
            // Windows drive paths.
            r"\b[A-Za-z]:[\\/][^\s]+",
            // UNC paths: \\server\share\path\to\file (≥2 backslash
            // segments after the leading `\\`). Matches in raw-string
            // regex `\\\\` = two literal `\`, then non-space non-`\`
            // segments separated by single `\`.
            r"\\\\[^\s\\]+(?:\\[^\s\\]+)+",
            // Multi-segment backslash relative paths
            // (`src\secretProject\file.rs`): alphanumeric segment,
            // then ≥1 `\<segment>` chunks. Mirrors the forward-slash
            // multi-segment rule below.
            r"[A-Za-z0-9_\-]+(?:\\[A-Za-z0-9_.\-]+)+",
            // Relative paths starting with `./` or `../`.
            r"\.{1,2}/[^\s]+",
            // Absolute / repository-rooted paths: any token containing
            // a `/` with at least one alphanumeric segment on each side.
            // Catches `/Users/alice/...`, `src/lib/foo.rs`, `a/b`,
            // `path/to/file.ext`. Conservative — also wipes things like
            // `transformer/decoder` from prose, but the loss is
            // acceptable next to the privacy gain.
            r"\S*[A-Za-z0-9_\-]+/[A-Za-z0-9_./\-]+",
            // Common API-key prefixes (specific shapes first so they
            // can't be missed by the generic long-token pattern when
            // they include punctuation).
            r"\b(?:sk|ghp|gho|ghu|ghs|github_pat|xox[abps])-[A-Za-z0-9_\-]{8,}",
            r"\bAKIA[0-9A-Z]{16}\b",
            // Long opaque alphanumeric tokens (>=24 chars) — typical
            // API keys / hashes / bearer tokens.
            r"\b[A-Za-z0-9_\-]{24,}\b",
            // Hex blobs (>=12 chars) that look like commit shas, MD5s,
            // SHA256s, etc.
            r"\b[A-Fa-f0-9]{12,}\b",
        ];
        raw.iter()
            .map(|p| Regex::new(p).expect("redaction regex must compile"))
            .collect()
    });

    let mut current = input.to_string();
    for re in patterns {
        current = re.replace_all(&current, " ").into_owned();
    }
    current
}

/// Tokenize text into alphanumeric chunks of length 4..=24. Strips
/// punctuation, drops the chunk if it contains characters that look
/// path-like, URL-like, or address-like (`/`, `\\`, `@`, `://`).
fn tokenize(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            buf.push(ch);
        } else {
            if !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
            }
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out.into_iter()
        .filter(|s| (4..=24).contains(&s.len()))
        .collect()
}

fn is_useful_token(s: &str) -> bool {
    let lower = s.to_lowercase();
    if STOP_WORDS.contains(&lower.as_str()) {
        return false;
    }
    // Reject anything that looks numeric-only (PII / IDs are
    // not useful for arXiv search).
    if s.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // Reject tokens with no vowel — usually base64 chunks, hashes,
    // or hex IDs. Cheap heuristic to filter noise.
    let has_vowel = s.chars().any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'));
    if !has_vowel {
        return false;
    }
    // Reject tokens consisting only of digits + a few letters (commit
    // SHAs and the like). Heuristic: digit ratio > 60% means drop.
    let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count * 10 > s.len() * 6 {
        return false;
    }
    true
}

/// Spawn `git log --oneline -<n>` in `workspace` with a hard timeout.
/// Returns the commit subject lines on success, or an error if the
/// timeout fired, git was missing, or the workspace was not a git repo.
///
/// Public convenience wrapper around `mine_git_commits_with_git("git", …)`.
/// Existing callers (and the two tests in this module) use this name.
#[allow(dead_code)]
pub fn mine_git_commits(
    workspace: &Path,
    n: usize,
    timeout_ms: u64,
) -> Result<Vec<String>, MineGitError> {
    mine_git_commits_with_git("git", workspace, n, timeout_ms)
}

/// Injectable variant of `mine_git_commits` — tests pass a fake git
/// binary path so the adversarial timeout regression can run without
/// PATH mutation. On Unix the child is launched in its own process
/// group; on timeout the entire group is SIGKILL'd so any descendant
/// process holding the inherited stdout fd is reaped and the reader
/// thread can return EOF immediately.
pub fn mine_git_commits_with_git(
    git_bin: &str,
    workspace: &Path,
    n: usize,
    timeout_ms: u64,
) -> Result<Vec<String>, MineGitError> {
    let mut cmd = Command::new(git_bin);
    cmd.arg("-C")
        .arg(workspace)
        .arg("log")
        .arg("--oneline")
        .arg("--no-color")
        .arg(format!("-{n}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    // On Unix, put git in its own process group so the timeout path
    // can SIGKILL the whole group — wrapper-style git processes
    // (shell scripts, sudo, etc.) leave descendants holding stdout
    // even after the immediate child is killed, which made the
    // reader thread block forever on `read_to_string` waiting for
    // EOF that never came.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| MineGitError::Spawn(e.to_string()))?;
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MineGitError::Spawn("stdout pipe missing".to_string()))?;

    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut s);
        let _ = tx.send(s);
    });

    let start = Instant::now();
    let deadline = Duration::from_millis(timeout_ms);
    let body = loop {
        // Sleep at most until the deadline; never longer than 50 ms.
        let remaining_until_deadline = deadline.saturating_sub(start.elapsed());
        if remaining_until_deadline.is_zero() {
            kill_process_group(child_pid);
            let _ = child.kill();
            let _ = child.wait();
            // With the group dead the descendant's stdout fd is
            // closed, so the reader thread returns EOF immediately
            // — join here is bounded.
            let _ = handle.join();
            return Err(MineGitError::Timeout);
        }
        let poll = remaining_until_deadline.min(Duration::from_millis(50));
        match rx.recv_timeout(poll) {
            Ok(s) => break Some(s),
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(_)) = child.try_wait() {
                    if let Ok(s) = rx.recv_timeout(Duration::from_millis(200)) {
                        break Some(s);
                    }
                    break None;
                }
            }
        }
    };
    let _ = handle.join();
    let _ = child.wait();
    let body = body.ok_or(MineGitError::EmptyOutput)?;
    let subjects: Vec<String> = body
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            // Drop the leading sha.
            let mut parts = trimmed.splitn(2, ' ');
            parts.next();
            parts.next().map(|s| s.to_string())
        })
        .collect();
    Ok(subjects)
}

/// Best-effort SIGKILL of the entire process group whose group-id
/// equals `child_pid`. Only meaningful on Unix; a no-op elsewhere.
/// The fallback `child.kill()` in the caller covers the immediate
/// child if the group send fails for any reason.
fn kill_process_group(child_pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        // Negative PID sends to the entire process group whose
        // group-id is child_pid (set by `process_group(0)` above).
        let pgid = Pid::from_raw(-(child_pid as i32));
        let _ = kill(pgid, Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child_pid;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MineGitError {
    #[error("git spawn failed: {0}")]
    Spawn(String),
    #[error("git log timed out")]
    Timeout,
    #[error("git log produced no output")]
    EmptyOutput,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn tokenize_drops_short_long_and_punctuation() {
        let toks = tokenize("hello world!! abc 12345 a verylongtokenmorethan24chars");
        assert!(toks.contains(&"hello".to_string()));
        assert!(toks.contains(&"world".to_string()));
        assert!(!toks.iter().any(|t| t == "abc")); // too short
        assert!(!toks.iter().any(|t| t.len() > 24));
    }

    #[test]
    fn is_useful_filters_stopwords_and_hashes() {
        assert!(is_useful_token("transformer"));
        assert!(!is_useful_token("error")); // stopword
        assert!(!is_useful_token("123456")); // all digits
        assert!(!is_useful_token("xyzbcdfg")); // no vowel
    }

    #[test]
    fn extract_keywords_handles_missing_claude_dir() {
        let tmp = TempDir::new().unwrap();
        let tokens = extract_keywords_from_claude_dir(tmp.path(), 4096);
        assert!(tokens.is_empty());
    }

    #[test]
    fn extract_keywords_reads_jsonl_tail() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let content = "transformer attention diffusion gradient backpropagation".repeat(200);
        fs::write(claude.join("conv.jsonl"), content).unwrap();
        let tokens = extract_keywords_from_claude_dir(tmp.path(), 16 * 1024);
        assert!(tokens.iter().any(|t| t == "transformer"));
        assert!(tokens.iter().any(|t| t == "diffusion"));
    }

    #[test]
    fn extract_keywords_respects_byte_budget() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let big = "a".repeat(10 * 1024 * 1024); // 10MB of single char (no useful tokens)
        fs::write(claude.join("big.jsonl"), big).unwrap();
        let start = std::time::Instant::now();
        let tokens = extract_keywords_from_claude_dir(tmp.path(), 64 * 1024);
        let elapsed = start.elapsed();
        // Should be near-instant since we only read the tail.
        assert!(elapsed < Duration::from_millis(500));
        assert!(tokens.is_empty());
    }

    #[test]
    fn extract_workspace_keywords_total_budget_bounded_with_many_workspaces() {
        // With 100 fake workspaces, the per-workspace floor (4KB) would
        // explode total reads to 400KB unless we enforce a running total
        // cap. Verify the function still returns within a short time
        // and respects the total cap by checking it walks just a few of
        // the workspaces, not all of them.
        let dirs: Vec<TempDir> = (0..100).map(|_| TempDir::new().unwrap()).collect();
        for dir in &dirs {
            let claude = dir.path().join(".claude");
            fs::create_dir_all(&claude).unwrap();
            fs::write(claude.join("a.jsonl"), "keyword content here ".repeat(50)).unwrap();
        }
        let paths: Vec<&std::path::Path> = dirs.iter().map(|d| d.path()).collect();
        let start = Instant::now();
        let tokens = extract_workspace_keywords(&paths, 32 * 1024); // 32KB cap
        // Must not take long (every workspace's .claude is small, but we
        // also must not visit all 100 once budget runs out).
        assert!(start.elapsed() < Duration::from_secs(2));
        // Cap on token count still applies.
        assert!(tokens.len() <= MAX_TOKENS_PER_QUERY);
    }

    #[test]
    fn extract_workspace_keywords_caps_token_count() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("token{i:04} "));
        }
        fs::write(claude.join("a.jsonl"), &content).unwrap();
        let tokens = extract_workspace_keywords(&[tmp.path()], 64 * 1024);
        assert!(tokens.len() <= MAX_TOKENS_PER_QUERY);
    }

    #[test]
    fn git_mining_returns_empty_on_non_git_dir() {
        // Spawn against a non-git directory — git emits an error to
        // stderr (which is discarded), stdout is empty, and we read
        // an empty body. Real timeout is hard to test deterministically
        // without a hanging git process; the time bound is still
        // enforced and the function returns within the budget.
        let tmp = TempDir::new().unwrap();
        let start = Instant::now();
        let result = mine_git_commits(tmp.path(), 5, 1500);
        assert!(start.elapsed() < Duration::from_millis(1600));
        match result {
            Ok(v) => assert!(v.is_empty()),
            Err(MineGitError::Timeout) => panic!("git on empty dir should not time out"),
            Err(_) => {} // acceptable: empty output or spawn refusal
        }
    }

    #[test]
    fn git_mining_returns_commit_subjects() {
        let tmp = TempDir::new().unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .arg("--quiet")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_err() || !status.unwrap().success() {
            // git not available — skip the test
            return;
        }
        let _ = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["config", "user.email", "t@example.com"])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["config", "user.name", "Test"])
            .status();
        fs::write(tmp.path().join("README.md"), "hi").unwrap();
        let _ = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["add", "."])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["commit", "--quiet", "-m", "Initial transformer attention research"])
            .status();
        let subjects = mine_git_commits(tmp.path(), 5, 2000).unwrap();
        assert_eq!(subjects.len(), 1);
        assert!(subjects[0].contains("transformer"));
    }

    fn write_claude_fixture(workspace: &std::path::Path, body: &str) {
        let claude = workspace.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("conv.jsonl"), body).unwrap();
    }

    fn lower_tokens(workspace: &std::path::Path) -> Vec<String> {
        extract_workspace_keywords(&[workspace], 256 * 1024)
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .collect()
    }

    #[test]
    fn redact_sensitive_spans_removes_paths_emails_urls_api_keys() {
        let input = "research notes /Users/alice/secretProject/src/model_loader.rs more text \
                     contact alice@example.com see https://github.com/foo/bar and \
                     git@github.com:owner/repo.git plus token sk-abcdef1234567890abcdefABCDEFabcd \
                     and AKIAABCDEFGHIJKLMNOP also hash deadbeefcafe0001";
        let out = redact_sensitive_spans(input);
        let lower = out.to_ascii_lowercase();
        for must_be_gone in [
            "/users/alice",
            "secretproject",
            "model_loader",
            "alice@example.com",
            "https://github.com",
            "git@github.com:owner",
            "sk-abcdef1234567890",
            "akiaabcdefghijklmnop",
            "deadbeefcafe0001",
        ] {
            assert!(
                !lower.contains(must_be_gone),
                "redaction missed `{must_be_gone}` in `{out}`"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_drops_path_components() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            "looking at /Users/alice/secretProject/src/model_loader.rs for inspiration",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in [
            "alice",
            "users",
            "secretproject",
            "model_loader",
            "modelloader",
        ] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "path component `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_drops_email_local_and_domain() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            "ping researchers reach alice@example.com about transformers",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in ["alice", "example"] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "email component `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_drops_url_components() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            "see https://github.com/foo/bar and also git@github.com:owner/repo.git",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in ["github", "foo", "owner", "repo"] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "URL component `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_drops_api_key_shaped_tokens() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            "secrets sk-abcdef1234567890abcdefABCDEFabcd and AKIAABCDEFGHIJKLMNOP \
             and ghp_abcdefghijklmnopqrstuvwxyz012345 plus bare token \
             0123456789abcdef0123456789abcdef and shorter dead beef cafe",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in [
            "abcdefghijklmnop",
            "akiaabcdefghijklmnop",
            "0123456789abcdef",
            "ghp_abcdefghijklmn",
        ] {
            // tokens are capped at 24 chars by tokenize; check no
            // long-key fragment slipped through.
            assert!(
                !tokens.iter().any(|t| t.contains(leaked) || t.starts_with(leaked)),
                "api-key-shaped fragment `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_preserves_real_research_terms() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            "studying transformer attention diffusion gradient backpropagation \
             alongside /Users/alice/secretProject/notes.md and alice@example.com",
        );
        let tokens = lower_tokens(tmp.path());
        for kept in [
            "transformer",
            "attention",
            "diffusion",
            "gradient",
            "backpropagation",
        ] {
            assert!(
                tokens.iter().any(|t| t == kept),
                "research term `{kept}` was wrongly dropped; tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn redact_sensitive_spans_removes_unc_paths() {
        // Input chosen so the only place the components below appear is
        // inside the UNC path itself; otherwise the test would be
        // proving the redactor strips ordinary prose words.
        let input =
            r"alpha \\srvhost\netshare\secretProject\src\model_loader.rs omega";
        let out = redact_sensitive_spans(input);
        let lower = out.to_ascii_lowercase();
        for must_be_gone in [
            r"\\srvhost",
            "srvhost",
            "netshare",
            "secretproject",
            "model_loader",
        ] {
            assert!(
                !lower.contains(must_be_gone),
                "redaction missed `{must_be_gone}` in `{out}`"
            );
        }
        // Sanity: surrounding non-sensitive words are preserved.
        assert!(lower.contains("alpha"));
        assert!(lower.contains("omega"));
    }

    #[test]
    fn extract_workspace_keywords_drops_unc_path_components() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            r"shared net mount \\server\share\secretProject\src\model_loader.rs notes",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in [
            "server",
            "share",
            "secretproject",
            "model_loader",
            "modelloader",
        ] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "UNC path component `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn extract_workspace_keywords_drops_backslash_relative_path_components() {
        let tmp = TempDir::new().unwrap();
        write_claude_fixture(
            tmp.path(),
            r"tracing src\secretProject\model_loader.rs and components\widget\index.tsx",
        );
        let tokens = lower_tokens(tmp.path());
        for leaked in [
            "secretproject",
            "model_loader",
            "modelloader",
            "widget",
        ] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "backslash path component `{leaked}` leaked into tokens={tokens:?}"
            );
        }
    }

    #[test]
    fn git_timeout_for_remaining_skips_when_below_floor() {
        // When less than the spawn floor remains, the next git call
        // must be skipped entirely — the spawn + drain overhead alone
        // can exceed the remaining budget and push past the global
        // extraction deadline.
        assert_eq!(git_timeout_for_remaining(0), None);
        assert_eq!(git_timeout_for_remaining(50), None);
        assert_eq!(git_timeout_for_remaining(GIT_SPAWN_FLOOR_MS - 1), None);
        // Exactly at the floor we still spawn (and pass the small
        // remaining as the per-call timeout).
        assert_eq!(
            git_timeout_for_remaining(GIT_SPAWN_FLOOR_MS),
            Some(GIT_SPAWN_FLOOR_MS)
        );
    }

    #[test]
    fn git_timeout_for_remaining_caps_at_per_call_ceiling() {
        // Above the per-call ceiling, the returned timeout is the
        // ceiling, not the remaining wall-clock — that way slow
        // subprocesses still time out at the per-call boundary even
        // when there is plenty of overall budget left.
        assert_eq!(
            git_timeout_for_remaining(GIT_LOG_TIMEOUT_MS + 500),
            Some(GIT_LOG_TIMEOUT_MS)
        );
        // Below the per-call ceiling but above the spawn floor, the
        // remaining budget caps the timeout.
        assert_eq!(git_timeout_for_remaining(800), Some(800));
    }

    #[test]
    #[cfg(unix)]
    fn extract_workspace_keywords_with_hanging_git_respects_2s_contract() {
        // Regression for the subprocess-timeout bug: a wrapper-style
        // git that leaves a descendant holding stdout open would
        // make the reader thread block on EOF forever, so even
        // though `child.kill()` killed the immediate process, the
        // total extraction time would balloon past the plan's
        // "<2 seconds total" budget.
        //
        // This test uses the injectable git-binary helper so it does
        // NOT mutate process-wide PATH — Rust unit tests in this
        // crate run in parallel within one process and a PATH change
        // would race other git-using tests.
        let fake_dir = TempDir::new().unwrap();
        let fake_git = fake_dir.path().join("git");
        fs::write(&fake_git, "#!/bin/sh\nsleep 5\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_git).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_git, perms).unwrap();
        let fake_git_str = fake_git.to_string_lossy().to_string();

        let workspaces: Vec<TempDir> = (0..4).map(|_| TempDir::new().unwrap()).collect();
        for ws in &workspaces {
            write_claude_fixture(
                ws.path(),
                "transformer attention diffusion gradient research notes",
            );
        }
        let paths: Vec<&std::path::Path> = workspaces.iter().map(|d| d.path()).collect();

        let start = Instant::now();
        let _tokens = extract_workspace_keywords_with_git(&paths, 64 * 1024, &fake_git_str);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(2_000),
            "extract_workspace_keywords_with_git exceeded the 2-second context budget; \
             elapsed = {elapsed:?} across {} workspaces with a hanging git",
            paths.len()
        );
    }

    #[test]
    #[cfg(unix)]
    fn mine_git_commits_with_git_returns_timeout_within_budget_on_hang() {
        // Per-call boundary check: a hanging git fixture must return
        // `MineGitError::Timeout` within roughly the configured
        // timeout, not after the sleep finishes. We give a 600 ms
        // headroom over the 200 ms timeout to absorb spawn latency.
        let fake_dir = TempDir::new().unwrap();
        let fake_git = fake_dir.path().join("git");
        fs::write(&fake_git, "#!/bin/sh\nsleep 5\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_git).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_git, perms).unwrap();

        let ws = TempDir::new().unwrap();
        let start = Instant::now();
        let result = mine_git_commits_with_git(
            &fake_git.to_string_lossy(),
            ws.path(),
            5,
            200,
        );
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(MineGitError::Timeout)));
        assert!(
            elapsed < Duration::from_millis(800),
            "mine_git_commits_with_git took {elapsed:?} for a 200ms timeout; group-kill is not working"
        );
    }

    #[test]
    fn extract_workspace_keywords_meets_two_second_contract_under_no_git_load() {
        // Sanity check on the happy path. Many workspaces with small
        // `.claude/` fixtures and real (fast) git on non-repo dirs
        // must complete well inside the global deadline. The
        // wall-clock-deadline test path that intentionally hangs git
        // would race other parallel tests via PATH mutation, so the
        // deadline arithmetic itself is covered by the two
        // `git_timeout_for_remaining_*` tests above; this test just
        // confirms the production happy path stays inside the
        // contract.
        let dirs: Vec<TempDir> = (0..40).map(|_| TempDir::new().unwrap()).collect();
        for d in &dirs {
            let claude = d.path().join(".claude");
            fs::create_dir_all(&claude).unwrap();
            fs::write(
                claude.join("a.jsonl"),
                "transformer attention diffusion gradient research notes",
            )
            .unwrap();
        }
        let paths: Vec<&std::path::Path> = dirs.iter().map(|d| d.path()).collect();
        let start = Instant::now();
        let _ = extract_workspace_keywords(&paths, 64 * 1024);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(2_000),
            "extract_workspace_keywords exceeded the 2-second context budget under no-git-load; elapsed = {elapsed:?}"
        );
    }

    #[test]
    fn workspace_is_stale_returns_true_when_claude_older_than_threshold() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        // Pretend `now` is 100 days after the directory was created.
        let now = SystemTime::now() + Duration::from_secs(100 * 86400);
        assert!(workspace_is_stale(
            tmp.path(),
            now,
            STALE_WORKSPACE_THRESHOLD_SECS,
        ));
    }

    #[test]
    fn workspace_is_stale_returns_false_when_claude_fresh() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let now = SystemTime::now();
        assert!(!workspace_is_stale(
            tmp.path(),
            now,
            STALE_WORKSPACE_THRESHOLD_SECS,
        ));
    }

    #[test]
    fn workspace_is_stale_falls_back_to_workspace_dir_when_no_claude() {
        let tmp = TempDir::new().unwrap();
        // No `.claude` directory. Workspace dir just created (fresh).
        let now = SystemTime::now();
        assert!(!workspace_is_stale(
            tmp.path(),
            now,
            STALE_WORKSPACE_THRESHOLD_SECS,
        ));
        // 100 days in the future → workspace dir's mtime is "old".
        let later = SystemTime::now() + Duration::from_secs(100 * 86400);
        assert!(workspace_is_stale(
            tmp.path(),
            later,
            STALE_WORKSPACE_THRESHOLD_SECS,
        ));
    }

    #[test]
    fn workspace_is_stale_uses_newest_claude_file_when_dir_mtime_old() {
        // Long-lived workspace regression: Claude appends to an
        // existing `*.jsonl` inside `.claude/`, which bumps the
        // file's mtime but not necessarily the directory entry
        // list mtime. The probe must look at the newest FILE
        // mtime, not just the dir mtime, or active workspaces
        // get incorrectly skipped.
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let conv = claude.join("conv.jsonl");
        fs::write(&conv, "fresh conversation content").unwrap();
        // Backdate ONLY the directory mtime to 100 days ago.
        let backdate = SystemTime::now() - Duration::from_secs(100 * 86400);
        let ft = filetime::FileTime::from_system_time(backdate);
        filetime::set_file_mtime(&claude, ft).expect("set_file_mtime on dir");
        // Leave the file mtime fresh.
        let now = SystemTime::now();
        assert!(
            !workspace_is_stale(tmp.path(), now, STALE_WORKSPACE_THRESHOLD_SECS),
            "stale check must consult the newest file mtime, not just the dir mtime"
        );
    }

    #[test]
    fn workspace_is_stale_returns_true_when_all_claude_files_old() {
        // Inverse of the above: every file inside `.claude/` is
        // backdated past the threshold AND the dir itself is
        // backdated → the workspace is correctly classified stale.
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        for name in ["a.jsonl", "b.jsonl"] {
            let p = claude.join(name);
            fs::write(&p, "old content").unwrap();
        }
        let backdate = SystemTime::now() - Duration::from_secs(100 * 86400);
        let ft = filetime::FileTime::from_system_time(backdate);
        filetime::set_file_mtime(claude.join("a.jsonl"), ft).expect("set_file_mtime a");
        filetime::set_file_mtime(claude.join("b.jsonl"), ft).expect("set_file_mtime b");
        filetime::set_file_mtime(&claude, ft).expect("set_file_mtime dir");
        filetime::set_file_mtime(tmp.path(), ft).expect("set_file_mtime ws");

        let now = SystemTime::now();
        assert!(workspace_is_stale(
            tmp.path(),
            now,
            STALE_WORKSPACE_THRESHOLD_SECS
        ));
    }

    #[test]
    fn extract_workspace_keywords_skips_stale_workspace_entirely() {
        // One workspace with fresh `.claude` containing UNIQUE_FRESH
        // tokens; one workspace with `.claude` whose mtime is moved
        // far into the past (older than the threshold) containing
        // UNIQUE_STALE tokens. Only the fresh workspace's tokens
        // must appear in the output.
        let fresh = TempDir::new().unwrap();
        let stale = TempDir::new().unwrap();
        write_claude_fixture(
            fresh.path(),
            "transformerfresh attentionfresh diffusionfresh",
        );
        write_claude_fixture(
            stale.path(),
            "transformerstale attentionstale diffusionstale",
        );
        // Backdate every mtime the stale-probe consults: every file
        // inside `.claude/`, the `.claude/` dir itself, and the
        // workspace dir. The newest-file probe is the primary signal,
        // so the file mtimes are the load-bearing ones here.
        let claude_stale = stale.path().join(".claude");
        let backdate = SystemTime::now() - Duration::from_secs(100 * 86400);
        let file_time = filetime::FileTime::from_system_time(backdate);
        // Backdate every `.jsonl` inside `.claude/` first.
        for entry in std::fs::read_dir(&claude_stale).unwrap().flatten() {
            filetime::set_file_mtime(entry.path(), file_time)
                .expect("set_file_mtime on .claude file");
        }
        filetime::set_file_mtime(&claude_stale, file_time)
            .expect("set_file_mtime on .claude dir");
        // Also backdate the workspace dir itself so the workspace-dir
        // fallback in `workspace_is_stale` won't rescue it.
        filetime::set_file_mtime(stale.path(), file_time)
            .expect("set_file_mtime on workspace dir");

        let paths: Vec<&std::path::Path> = vec![fresh.path(), stale.path()];
        let tokens: Vec<String> = extract_workspace_keywords(&paths, 64 * 1024)
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();

        assert!(
            tokens.iter().any(|t| t == "transformerfresh"),
            "fresh-workspace token missing; tokens={tokens:?}"
        );
        for leaked in ["transformerstale", "attentionstale", "diffusionstale"] {
            assert!(
                !tokens.iter().any(|t| t == leaked),
                "stale-workspace token `{leaked}` leaked through filter; tokens={tokens:?}"
            );
        }
    }
}
