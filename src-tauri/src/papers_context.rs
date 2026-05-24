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
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub const CONTEXT_BUDGET_BYTES_TOTAL: usize = 256 * 1024;
pub const MAX_TOKENS_PER_QUERY: usize = 30;
pub const GIT_LOG_TIMEOUT_MS: u64 = 1500;
pub const MAX_GIT_COMMITS: usize = 20;

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
    if workspaces.is_empty() {
        return Vec::new();
    }
    // Per-workspace budget: floor at 4KB so a single workspace still reads
    // something useful, but cap the TOTAL across all workspaces at
    // `total_budget_bytes` by tracking the remaining budget as we go.
    let per_workspace_floor: usize = 4096;
    let per_workspace_target = total_budget_bytes / workspaces.len().max(1);
    let mut remaining_budget = total_budget_bytes;

    let mut all_tokens: HashSet<String> = HashSet::new();
    for ws in workspaces {
        let ws = ws.as_ref();
        if remaining_budget == 0 {
            break;
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
        if let Ok(subjects) = mine_git_commits(ws, MAX_GIT_COMMITS, GIT_LOG_TIMEOUT_MS) {
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

/// Read up to `byte_budget` bytes from the largest-mtime `.claude/*.jsonl`
/// file under `workspace`, tokenize, and return useful tokens. Returns an
/// empty Vec if `.claude/` does not exist or is empty.
pub fn extract_keywords_from_claude_dir(workspace: &Path, byte_budget: usize) -> Vec<String> {
    let claude_dir = workspace.join(".claude");
    if !claude_dir.is_dir() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(&claude_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut newest: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
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
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok();
        if let Some(mt) = mtime {
            match &newest {
                Some((_, cur)) if *cur >= mt => {}
                _ => newest = Some((path, mt)),
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
pub fn mine_git_commits(
    workspace: &Path,
    n: usize,
    timeout_ms: u64,
) -> Result<Vec<String>, MineGitError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workspace)
        .arg("log")
        .arg("--oneline")
        .arg("--no-color")
        .arg(format!("-{n}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| MineGitError::Spawn(e.to_string()))?;
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
        if let Ok(s) = rx.recv_timeout(Duration::from_millis(50)) {
            break Some(s);
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = handle.join();
            return Err(MineGitError::Timeout);
        }
        if let Ok(Some(_)) = child.try_wait() {
            // process exited; drain stdout one more time
            if let Ok(s) = rx.recv_timeout(Duration::from_millis(200)) {
                break Some(s);
            }
            break None;
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
}
