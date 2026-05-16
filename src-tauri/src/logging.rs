//! Structured JSON logging with sensitive-value redaction (task4 / AC-9.5).
//!
//! Wraps `tracing-subscriber`'s JSON formatter in a `RedactingMakeWriter` so
//! any log line that mentions a known-sensitive field (`refresh_token`,
//! `access_token`, `password`, `bearer`, `email_body`) gets its value rewritten
//! to `"<redacted>"` before it leaves the process.
//!
//! The redaction is a backstop. Call sites that handle secrets should still
//! avoid logging them; the regex protects against accidental string
//! interpolation by future contributors. The pattern targets JSON-shaped
//! output specifically — the only writer feeding this layer.

use std::io::{self, Write};
use std::sync::OnceLock;

use regex::Regex;
use tracing_subscriber::fmt::MakeWriter;

/// Compiled once per process. The pattern matches a sensitive key followed by
/// `:` (JSON) or `=` (tracing field syntax leaking into a string), then a
/// quoted string value. The replacement collapses the value to `"<redacted>"`
/// without disturbing surrounding JSON structure.
fn redaction_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?P<key>"?(?:refresh_token|access_token|password|bearer|email_body)"?)\s*[:=]\s*"(?:[^"\\]|\\.)*""#,
        )
        .expect("redaction regex compiles")
    })
}

/// Apply redaction to an already-rendered log line. Exposed for tests.
pub fn redact_line(line: &str) -> String {
    redaction_regex()
        .replace_all(line, |caps: &regex::Captures<'_>| {
            format!("{}:\"<redacted>\"", &caps["key"])
        })
        .into_owned()
}

/// Writer adapter that buffers each `write` call, applies redaction, and
/// forwards to stderr. tracing-subscriber's JSON formatter emits exactly one
/// JSON object per `write`, so per-call redaction is sufficient.
pub struct RedactingStderr;

impl Write for RedactingStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let original = buf.len();
        let as_str = std::str::from_utf8(buf).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("log line not utf-8: {err}"),
            )
        })?;
        let redacted = redact_line(as_str);
        io::stderr().write_all(redacted.as_bytes())?;
        Ok(original)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// `MakeWriter` impl: every log event gets a fresh `RedactingStderr`.
#[derive(Clone, Default)]
pub struct RedactingMakeWriter;

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingStderr;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingStderr
    }
}

/// Initialize the global subscriber. Idempotent: subsequent calls do nothing
/// (matches the prior inline behavior of `tracing_subscriber::fmt().init()`,
/// which also installs only the first attempt).
pub fn init_subscriber() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(RedactingMakeWriter)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_refresh_token() {
        let line = r#"{"timestamp":"…","fields":{"refresh_token":"ya29.fake_value_abc"}}"#;
        let redacted = redact_line(line);
        assert!(
            redacted.contains("<redacted>"),
            "expected <redacted> marker; got: {redacted}"
        );
        assert!(
            !redacted.contains("ya29.fake_value_abc"),
            "raw token leaked through: {redacted}"
        );
    }

    #[test]
    fn redaction_masks_access_token_and_password_and_bearer_and_email_body() {
        let line = r#"{"access_token":"secret1","password":"secret2","bearer":"secret3","email_body":"hello\nworld"}"#;
        let redacted = redact_line(line);
        for raw in ["secret1", "secret2", "secret3", "hello"] {
            assert!(!redacted.contains(raw), "{raw} leaked through: {redacted}");
        }
        assert_eq!(redacted.matches("<redacted>").count(), 4);
    }

    #[test]
    fn redaction_leaves_unrelated_fields_alone() {
        let line = r#"{"plugin_id":"example-notes","mount_id":"abc","message":"hello"}"#;
        let redacted = redact_line(line);
        assert_eq!(redacted, line);
    }

    #[test]
    fn redaction_handles_field_style_with_equals() {
        // tracing's structured field syntax can render as `key="value"` in
        // some formatters; ensure both `:` and `=` separators are covered.
        let line = r#"event refresh_token="ya29.leak""#;
        let redacted = redact_line(line);
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains("ya29.leak"));
    }
}
