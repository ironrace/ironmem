//! Write-time secret scrubbing and length bounding for lineage content.
//!
//! Lineage records embed gate output — test failures, stack traces, command
//! stderr — which can contain tokens, credentials, and environment values.
//! Per the design spec's *Secret handling on the lineage write path* section,
//! no existing module does this at write time: `sanitize::sanitize_content`
//! only validates non-emptiness and length, `search/sanitizer.rs` handles
//! *query* degradation, and `config.rs::redacts_sensitive_content` is a
//! **read-time** MCP output mode, not a write-time guard. Content written to
//! a drawer today is persisted verbatim. This module is the write-time guard
//! the spec calls for — new work, not reuse.
//!
//! The approach is layered, in the order applied:
//!
//! 1. A handful of specific, high-confidence patterns for recognizable
//!    credential shapes (bearer tokens, GitHub/Slack/AWS-style tokens, JWTs).
//! 2. `KEY=value`/`KEY: value`-style assignments where the key name itself
//!    names a credential (`API_KEY`, `AUTH_TOKEN`, `DB_PASSWORD`, ...) — the
//!    key is kept (it's useful diagnostic context) and only the value is
//!    redacted.
//! 3. A generic high-entropy scan over any remaining long alphanumeric-ish
//!    token, to catch secret-shaped strings that don't match a known prefix.
//!
//! None of this is claimed to be exhaustive — secret scanning never is — but
//! it directly covers the shapes named in the spec ("bearer tokens, common
//! API-key patterns, high-entropy secret-looking strings") and errs toward
//! over-redaction rather than under-redaction, which is the safe direction
//! for content bound for long-lived memory.

use regex::{Captures, Regex};
use std::sync::LazyLock;

/// Placeholder substituted for anything this module redacts.
const REDACTED: &str = "[REDACTED]";

static BEARER_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}").unwrap());

/// GitHub personal-access/OAuth/server/refresh token prefixes:
/// `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`.
static GITHUB_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap());

/// AWS access key ids (`AKIA...`) and STS session key ids (`ASIA...`).
static AWS_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap());

/// Slack bot/user/app/refresh tokens.
static SLACK_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap());

/// A JSON Web Token: three base64url segments separated by `.`.
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bey[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b").unwrap()
});

/// `KEY=value` / `KEY: value` assignments where the identifier names a
/// credential. Case-insensitive so `api_key`, `API_KEY`, and `apiKey` all
/// match. The key name is captured separately from the value so the
/// replacement can keep it — "API_KEY=[REDACTED]" is a useful diagnostic
/// line; blanking the whole thing is not.
///
/// The value alternates a double-quoted or single-quoted run before falling
/// back to a bare `\S+`: without the quoted alternatives, `PASSWORD="hello
/// world"` would only match up to the first space (`"hello`), leaving
/// ` world"` — the rest of the secret — in the output untouched. Quoting is
/// the only reliable signal this module has for "the value contains
/// whitespace"; an unquoted multi-word value has no such marker and is out
/// of scope here.
static ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)(?P<prefix>\b(?:export\s+)?[A-Za-z][A-Za-z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|PWD|CREDENTIAL)[A-Za-z0-9_]*\s*[:=]\s*)(?P<value>"[^"\r\n]*"|'[^'\r\n]*'|\S+)"#,
    )
    .unwrap()
});

/// Candidate tokens for the generic high-entropy scan: 20+ characters drawn
/// from the alphabet real secrets are usually encoded in (base64/base64url).
///
/// `=` is deliberately excluded from this class entirely (earlier revisions
/// of this pattern included it, for base64 padding). Including it let a
/// `KEY=value` assignment whose key name doesn't match [`ASSIGNMENT_RE`]'s
/// known-credential vocabulary merge into one candidate spanning both the key
/// name and the value — including a trailing `=*` after the body class has
/// the same failure mode, since it greedily swallows the assignment's own
/// `=` delimiter as if it were padding, re-merging the two. Shannon entropy
/// is an average over the whole candidate, so a long, low-diversity key name
/// dilutes a genuinely high-entropy value below
/// [`ENTROPY_THRESHOLD_BITS_PER_CHAR`] and the secret escapes redaction
/// entirely. Excluding `=` unconditionally splits the two into separate
/// candidates, each scored on its own; [`is_low_signal_token`] then exempts
/// the key-name candidate via its identifier-shape check so a bare config key
/// isn't itself flagged as a secret. The cost is cosmetic: trailing `==`
/// base64 padding on an otherwise-redacted secret is left in the output
/// rather than folded into `[REDACTED]`.
static ENTROPY_CANDIDATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/_.-]{20,}").unwrap());

/// Shannon-entropy floor (bits/char) for the generic scan to redact a
/// candidate token. Tuned so ordinary prose and identifiers stay under it
/// while base64-ish secret material clears it; see `tests` for calibration
/// examples on both sides.
const ENTROPY_THRESHOLD_BITS_PER_CHAR: f64 = 3.6;

/// Redact every recognizable credential-shaped substring in `input`.
/// Returns the scrubbed text and whether anything was redacted.
pub fn scrub_secrets(input: &str) -> (String, bool) {
    let mut redacted = false;
    let mut text = input.to_string();

    for pattern in [
        &*BEARER_TOKEN_RE,
        &*GITHUB_TOKEN_RE,
        &*AWS_KEY_RE,
        &*SLACK_TOKEN_RE,
        &*JWT_RE,
    ] {
        if pattern.is_match(&text) {
            redacted = true;
            text = pattern.replace_all(&text, REDACTED).into_owned();
        }
    }

    if ASSIGNMENT_RE.is_match(&text) {
        redacted = true;
        text = ASSIGNMENT_RE
            .replace_all(&text, |caps: &Captures<'_>| {
                format!("{}{REDACTED}", &caps["prefix"])
            })
            .into_owned();
    }

    let (text, entropy_redacted) = redact_high_entropy_tokens(&text);
    redacted |= entropy_redacted;

    (text, redacted)
}

/// The result of scrubbing and length-bounding one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubOutcome {
    pub text: String,
    /// Whether [`scrub_secrets`] redacted anything.
    pub redacted: bool,
    /// Whether the scrubbed text exceeded `max_chars` and was cut.
    pub truncated: bool,
}

/// Scrub `input` for secrets, then bound it to `max_chars` characters.
///
/// Scrubbing runs before truncation (not the other way around) so a
/// near-boundary secret is still recognized as a whole token rather than
/// being sliced first and silently surviving as a fragment.
pub fn scrub_and_bound(input: &str, max_chars: usize) -> ScrubOutcome {
    let (scrubbed, redacted) = scrub_secrets(input);
    let mut chars = scrubbed.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    ScrubOutcome {
        text: bounded,
        redacted,
        truncated,
    }
}

fn redact_high_entropy_tokens(input: &str) -> (String, bool) {
    let mut redacted = false;
    let out = ENTROPY_CANDIDATE_RE.replace_all(input, |caps: &Captures<'_>| {
        let token = &caps[0];
        if is_low_signal_token(token) {
            token.to_string()
        } else if shannon_entropy_bits_per_char(token) >= ENTROPY_THRESHOLD_BITS_PER_CHAR {
            redacted = true;
            REDACTED.to_string()
        } else {
            token.to_string()
        }
    });
    (out.into_owned(), redacted)
}

/// A canonical UUID shape (`8-4-4-4-12` hex groups). A `session_uuid` or
/// `record_id` mentioned in a `why_failed`/`approach` narrative (e.g. "the IC
/// session `<uuid>` crashed mid-turn") would otherwise clear the entropy
/// threshold — a plain UUID computes to roughly 4.06 bits/char, above
/// [`ENTROPY_THRESHOLD_BITS_PER_CHAR`] — and be redacted as if it were a
/// secret. That destroys exactly the session-correlation data
/// `dispatch_state`/`lineage`'s crash-recovery design depends on, so this
/// shape is exempted the same way hex and decimal runs are.
static UUID_SHAPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .unwrap()
});

/// Tokens common in benign gate output that would otherwise look
/// "high-entropy" to a naive character-distribution check: plain hex (git
/// SHAs, hashes), plain decimal runs, UUIDs, and plain identifier/config-key
/// names. Excluding them is what lets `commit_sha`-shaped text and
/// `session_uuid`-shaped text survive inside a `why_failed` narrative instead
/// of being redacted as if it were a secret.
fn is_low_signal_token(token: &str) -> bool {
    token.chars().all(|c| c.is_ascii_hexdigit())
        || token.chars().all(|c| c.is_ascii_digit())
        || UUID_SHAPE_RE.is_match(token)
        || is_single_case_identifier(token)
}

/// A plain identifier/constant-name shape: only letters, digits, and
/// underscores, with no *mixing* of upper- and lower-case letters.
/// `SCREAMING_SNAKE_CASE` and `plain_snake_case` config-key names fit this
/// shape; real base64/base64url secret material overwhelmingly does not (it
/// mixes case, or uses `+`/`/`/`.` punctuation this check already excludes).
///
/// This exists because [`ENTROPY_CANDIDATE_RE`] no longer merges a
/// `KEY=value` assignment into one candidate (see its doc): a long,
/// ordinary-looking key name can now surface as its own candidate, and such
/// names naturally run 3.9-4+ bits/char — above
/// [`ENTROPY_THRESHOLD_BITS_PER_CHAR`] — purely from using a large,
/// low-repetition alphabet (26 letters + `_`), not because they're secret.
fn is_single_case_identifier(token: &str) -> bool {
    if !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    !(has_upper && has_lower)
}

fn shannon_entropy_bits_per_char(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    let mut len = 0usize;
    for b in s.bytes() {
        counts[b as usize] += 1;
        len += 1;
    }
    if len == 0 {
        return 0.0;
    }
    let len_f = len as f64;
    counts.iter().filter(|&&c| c > 0).fold(0.0, |acc, &c| {
        let p = c as f64 / len_f;
        acc - p * p.log2()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_token() {
        // Deliberately generic — no known vendor prefix (Stripe's
        // `sk_live_`/`sk_test_`, etc.) so this fixture can't be mistaken for
        // a real credential by a source-level scanner such as GitHub's push
        // protection. `BEARER_TOKEN_RE` doesn't care about vendor shape, only
        // "Bearer " followed by a long-enough token, so this still exercises
        // it fully.
        let fake_token = "test-fixture-bearer-token-not-a-real-secret-0011223344";
        let input = format!("curl failed: Authorization: Bearer {fake_token}");
        let (out, redacted) = scrub_secrets(&input);
        assert!(redacted);
        assert!(!out.contains(fake_token));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn redacts_github_token() {
        let input =
            "git push failed: remote rejected token ghp_1234567890abcdefghijklmnopqrstuvwxyz";
        let (out, redacted) = scrub_secrets(input);
        assert!(redacted);
        assert!(!out.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redacts_aws_access_key() {
        let (out, redacted) = scrub_secrets("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE in env dump");
        assert!(redacted);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_slack_token() {
        // Same reasoning as the bearer-token fixture above: the "xoxb-"
        // vendor prefix is itself split across the concatenation boundary.
        let fake_token = format!("{}{}", "xox", "b-FAKE1234567890abcdefghijklmnop");
        let (out, redacted) = scrub_secrets(&format!("posted with {fake_token}"));
        assert!(redacted);
        assert!(!out.contains(&fake_token));
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dGhpc19pc19hX2Zha2Vfc2ln";
        let (out, redacted) = scrub_secrets(&format!("Authorization header was: {jwt}"));
        assert!(redacted);
        assert!(!out.contains(jwt));
    }

    #[test]
    fn redacts_credential_assignment_value_but_keeps_the_key_name() {
        let (out, redacted) = scrub_secrets("export API_KEY=abcd1234efgh5678ijkl\nother=fine");
        assert!(redacted);
        assert!(out.contains("API_KEY="));
        assert!(!out.contains("abcd1234efgh5678ijkl"));
    }

    #[test]
    fn redacts_password_assignment_case_insensitively() {
        let (out, redacted) = scrub_secrets("db_password: Sup3rSecretValue!!");
        assert!(redacted);
        assert!(out.contains("db_password:"));
        assert!(!out.contains("Sup3rSecretValue!!"));
    }

    #[test]
    fn redacts_generic_high_entropy_token_with_no_known_prefix() {
        let secret = "Qz7mK2pL9xVw4tRn8sYb1cJd6fHg3eUi0oAq5zXk";
        let (out, redacted) = scrub_secrets(&format!("leaked value: {secret}"));
        assert!(
            redacted,
            "a long mixed-case+digit token should trip the entropy scan"
        );
        assert!(!out.contains(secret));
    }

    #[test]
    fn does_not_redact_a_git_commit_sha() {
        let sha = "a1b2c3d4e5f6789012345678901234567890abcd";
        let (out, redacted) = scrub_secrets(&format!("failed after commit {sha}"));
        assert!(
            !redacted,
            "a plain hex commit sha must not be treated as a secret"
        );
        assert!(out.contains(sha));
    }

    #[test]
    fn does_not_redact_ordinary_prose() {
        let input = "cargo test failed: assertion `left == right` failed\n  left: 3\n right: 4";
        let (out, redacted) = scrub_secrets(input);
        assert!(!redacted);
        assert_eq!(out, input);
    }

    #[test]
    fn scrub_and_bound_truncates_after_scrubbing() {
        let long = "x".repeat(50);
        let outcome = scrub_and_bound(&long, 10);
        assert_eq!(outcome.text.chars().count(), 10);
        assert!(outcome.truncated);
        assert!(!outcome.redacted);
    }

    #[test]
    fn scrub_and_bound_reports_no_truncation_when_within_bound() {
        let outcome = scrub_and_bound("short", 10);
        assert_eq!(outcome.text, "short");
        assert!(!outcome.truncated);
    }

    // ── Regression: quoted/multi-word assignment values must be fully
    // redacted, not just their first whitespace-delimited word. ────────────
    #[test]
    fn redacts_a_quoted_multi_word_password_value_in_full() {
        let (out, redacted) = scrub_secrets(r#"DB_PASSWORD="hello world secret""#);
        assert!(redacted);
        assert!(out.contains("DB_PASSWORD="));
        assert!(out.contains(REDACTED));
        assert!(!out.contains("hello"));
        assert!(!out.contains("world"));
        assert!(!out.contains("secret"));
    }

    #[test]
    fn redacts_a_single_quoted_multi_word_token_value_in_full() {
        let (out, redacted) = scrub_secrets("export AUTH_TOKEN='multi word token value'");
        assert!(redacted);
        assert!(out.contains("AUTH_TOKEN="));
        assert!(!out.contains("multi"));
        assert!(!out.contains("word"));
        assert!(!out.contains("token value"));
    }

    // ── Regression: a real secret behind an unrecognized key name must not
    // evade redaction just because a long, low-diversity prefix is merged
    // into the same entropy candidate. ──────────────────────────────────────
    #[test]
    fn redacts_a_secret_behind_a_long_prefix_with_an_unrecognized_key_name() {
        // "VALUE" isn't in ASSIGNMENT_RE's credential vocabulary, so this
        // relies entirely on the fallback entropy scan.
        let prefix = "SOME_VERY_LONG_APPLICATION_CONFIGURATION_ENDPOINT_URL_VALUE";
        let secret = "H9jInyXgBylbrgihjsiyw"; // mixed-case, ~3.88 bits/char alone
        let input = format!("{prefix}={secret}");
        let (out, redacted) = scrub_secrets(&input);
        assert!(
            redacted,
            "a real secret must still be caught even when a long prefix is merged ahead of it"
        );
        assert!(
            !out.contains(secret),
            "the secret value must not survive in the output"
        );
        assert!(
            out.contains(prefix),
            "the low-signal identifier prefix should be left intact for diagnostic context"
        );
    }

    // ── Regression: a session UUID mentioned in narrative text must survive
    // un-redacted — it's exactly the correlation data crash recovery needs. ─
    #[test]
    fn does_not_redact_a_session_uuid_in_prose() {
        let uuid = "11111111-1111-1111-1111-111111111111";
        let input = format!("the IC session {uuid} crashed mid-turn");
        let (out, redacted) = scrub_secrets(&input);
        assert!(
            !redacted,
            "a plain session UUID must not be treated as a secret"
        );
        assert!(out.contains(uuid));
    }

    // ── Regression: splitting the KEY=value entropy candidate (to fix the
    // dilution-evasion bug above) must not turn ordinary identifiers into
    // false-positive redactions. ─────────────────────────────────────────────
    #[test]
    fn does_not_redact_a_long_screaming_snake_case_identifier_on_its_own() {
        let input = "the failing check referenced SOME_VERY_LONG_APPLICATION_CONFIGURATION_ENDPOINT_URL_VALUE in its output";
        let (out, redacted) = scrub_secrets(input);
        assert!(
            !redacted,
            "a plain constant/env-var name must not be treated as a secret"
        );
        assert_eq!(out, input);
    }
}
