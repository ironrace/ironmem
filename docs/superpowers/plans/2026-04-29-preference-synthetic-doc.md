# Preference-enrichment synthetic-doc Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port mempalace's per-session preference extractor to ironmem as an opt-in ingest enrichment that creates a synthetic "User has mentioned: …" sibling drawer per conversational drawer, plus a search-pipeline collapse step that hides the synthetic and promotes its score onto the parent. Validated against LongMemEval's `single-session-preference` slice.

**Architecture:** New `ironrace-pref-extract` crate exposes a `PreferenceExtractor` trait with a regex-based default impl (V4 patterns ported from mempalace). `crates/ironmem`'s `handle_add_drawer` calls the extractor when `IRONMEM_PREF_ENRICH=1` and the content looks conversational, then inserts a sibling drawer with `source_file = "pref:<parent_id>"`. The search pipeline gains a new step 7.5 between KG boost and shrinkage rerank that collapses synthetic-and-parent pairs into the parent only, with the parent's score elevated to `max(parent.score, synth.score)`. No schema migration; the sibling relationship rides on the existing `source_file` column with a `pref:` sentinel prefix.

**Tech Stack:** Rust 2021 (workspace crate), `regex` crate, `rusqlite`, existing `tracing`. No new runtime dependencies in `ironmem`.

**Spec:** `docs/superpowers/specs/2026-04-29-preference-synthetic-doc-design.md` (commit `b4f63b8`).

**Acceptance:** With `IRONMEM_PREF_ENRICH=1`, the LongMemEval bench (`scripts/benchmark_longmemeval.py --limit 165`) shows ≥ +2pp R@5 on the `single-session-preference` per-type slice vs `IRONMEM_PREF_ENRICH=0`, with no other category regressing by more than -0.5pp R@5.

---

## File map

| Path | Action | Purpose |
|---|---|---|
| `Cargo.toml` (workspace root) | modify | add `crates/ironrace-pref-extract` to `[workspace] members` |
| `crates/ironrace-pref-extract/Cargo.toml` | create | new crate manifest |
| `crates/ironrace-pref-extract/src/lib.rs` | create | trait + `RegexPreferenceExtractor` + helpers |
| `crates/ironrace-pref-extract/src/patterns.rs` | create | V4 regex patterns, `OnceLock`-cached |
| `crates/ironrace-pref-extract/tests/extract.rs` | create | unit tests for extraction + sniff + synthesize |
| `crates/ironmem/Cargo.toml` | modify | add `ironrace-pref-extract` dep |
| `crates/ironmem/src/search/tunables.rs` | modify | add `pref_enrich_enabled()` |
| `crates/ironmem/src/db/drawers.rs` | modify | add `delete_drawers_by_parent_tx`; cascade from `delete_drawer_tx` |
| `crates/ironmem/src/mcp/tools/drawers.rs` | modify | run enrichment in `handle_add_drawer` |
| `crates/ironmem/src/search/pipeline.rs` | modify | new step 7.5 `collapse_synthetic_into_parents` |
| `crates/ironmem/src/search/mod.rs` | modify | re-export collapse fn for tests |
| `crates/ironmem/tests/preference_enrichment_test.rs` | create | integration test: enrichment on/off + delete cascade |
| `crates/ironmem/tests/search_collapse_test.rs` | create | integration test: collapse promotes parent, hides synthetic |

---

## Task 1: Bootstrap `ironrace-pref-extract` crate skeleton

**Files:**
- Create: `crates/ironrace-pref-extract/Cargo.toml`
- Create: `crates/ironrace-pref-extract/src/lib.rs`
- Create: `crates/ironrace-pref-extract/src/patterns.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create the crate manifest**

`crates/ironrace-pref-extract/Cargo.toml`:
```toml
[package]
name = "ironrace-pref-extract"
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
description = "Conversational-text preference extractor for ironrace-memory."

[dependencies]
regex = "1"
```

- [ ] **Step 2: Create a stub `src/lib.rs`**

`crates/ironrace-pref-extract/src/lib.rs`:
```rust
//! Preference extractor for conversational text.
//!
//! Ported from mempalace's V4 regex set
//! (`mempalace/benchmarks/longmemeval_bench.py:1587-1610`). Pure CPU-bound,
//! deterministic, zero I/O. Intended to be called once per drawer at ingest
//! time when the content looks like a conversation.

mod patterns;

/// Strategy for extracting preference phrases from conversational text.
pub trait PreferenceExtractor: Send + Sync {
    /// Return up to N short phrases that describe user preferences,
    /// concerns, ongoing struggles, or memories. Order is the order of
    /// first occurrence in the input. Empty when the input has no matches.
    fn extract(&self, text: &str) -> Vec<String>;
}

/// Default implementation: a fixed set of V4 regexes scanned over the input.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegexPreferenceExtractor;

impl PreferenceExtractor for RegexPreferenceExtractor {
    fn extract(&self, text: &str) -> Vec<String> {
        patterns::extract_v4(text)
    }
}

/// Cheap structural test: does the text contain a first-person pronoun in
/// the first 500 chars? Intended as a guard so we don't run the regex set
/// on file chunks or non-conversational mining input.
pub fn looks_conversational(text: &str) -> bool {
    let head: String = text.chars().take(500).collect::<String>().to_lowercase();
    const NEEDLES: &[&str] = &[" i ", " i'", "i've ", "i'm ", " my ", " me "];
    if head.starts_with("i ") || head.starts_with("i'") {
        return true;
    }
    NEEDLES.iter().any(|n| head.contains(n))
}

/// Build the synthetic doc string from extracted phrases. Returns `None`
/// when there are no phrases (caller should skip the sibling insert).
pub fn synthesize_doc(phrases: &[String]) -> Option<String> {
    if phrases.is_empty() {
        return None;
    }
    Some(format!("User has mentioned: {}", phrases.join("; ")))
}
```

> **Note:** the V2 format change (commit `3dd59d0`) dropped the `"User has mentioned: "` prefix in favor of bare `phrases.join(". ")`. The shipped code uses the V2 format; this plan section reflects the V1 design as written.

- [ ] **Step 3: Create a stub `src/patterns.rs`** so the crate compiles before we add patterns

`crates/ironrace-pref-extract/src/patterns.rs`:
```rust
//! V4 preference regex set, ported verbatim from mempalace
//! (`benchmarks/longmemeval_bench.py:1587-1610`). Compiled once on first use
//! via `OnceLock`. A bad pattern panics at first call — caught by tests.

pub(crate) fn extract_v4(_text: &str) -> Vec<String> {
    Vec::new()
}
```

- [ ] **Step 4: Add the new crate to the workspace**

Edit `Cargo.toml` (workspace root), in `[workspace] members = [...]`, add the new member after `"crates/ironrace-rerank",`:
```toml
[workspace]
members = [
    "crates/ironrace-core",
    "crates/ironrace-embed",
    "crates/ironrace-rerank",
    "crates/ironrace-pref-extract",
    "crates/ironmem",
]
resolver = "2"
```

- [ ] **Step 5: Verify the crate builds**

Run: `cargo build -p ironrace-pref-extract`
Expected: clean build, no warnings.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ironrace-pref-extract
git commit -m "feat(pref-extract): scaffold ironrace-pref-extract crate"
```

---

## Task 2: V4 regex set with passing pattern-coverage tests

**Files:**
- Modify: `crates/ironrace-pref-extract/src/patterns.rs`
- Create: `crates/ironrace-pref-extract/tests/extract.rs`

- [ ] **Step 1: Write the failing tests first**

`crates/ironrace-pref-extract/tests/extract.rs`:
```rust
use ironrace_pref_extract::{
    looks_conversational, synthesize_doc, PreferenceExtractor, RegexPreferenceExtractor,
};

fn extract(text: &str) -> Vec<String> {
    RegexPreferenceExtractor.extract(text)
}

#[test]
fn extracts_struggling_with_pattern() {
    let text = "I've been having trouble with the battery life on my phone lately.";
    let out = extract(text);
    assert_eq!(out.len(), 1, "got {:?}", out);
    assert!(out[0].contains("battery life"), "got {:?}", out);
}

#[test]
fn extracts_i_prefer_pattern() {
    let out = extract("Honestly, I prefer black coffee in the morning.");
    assert!(
        out.iter().any(|p| p.contains("black coffee")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_lately_pattern() {
    let out = extract("Lately, I've been thinking about switching to a standing desk.");
    assert!(
        out.iter().any(|p| p.contains("standing desk")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_used_to_pattern() {
    let out = extract("I used to play guitar in a college band.");
    assert!(
        out.iter().any(|p| p.contains("play guitar")),
        "got {:?}",
        out,
    );
}

#[test]
fn extracts_high_school_memory_pattern() {
    let out = extract(
        "When I was in high school, I really enjoyed running cross country and \
         did pretty well at state.",
    );
    assert!(!out.is_empty(), "expected memory pattern to fire on high-school clause");
}

#[test]
fn dedupes_and_caps_at_twelve() {
    let mut text = String::new();
    for i in 0..20 {
        text.push_str(&format!("I prefer flavor number {i} above all others. "));
    }
    let out = extract(&text);
    assert!(out.len() <= 12, "expected ≤12 phrases, got {}", out.len());
}

#[test]
fn returns_empty_on_non_conversational_input() {
    let rust = r#"
        fn main() {
            let x = 42;
            println!("{}", x);
        }
    "#;
    assert!(extract(rust).is_empty());
}

#[test]
fn looks_conversational_true_for_first_person_lead() {
    assert!(looks_conversational("I've been thinking about switching jobs."));
    assert!(looks_conversational("So my plan is to take a sabbatical next quarter."));
}

#[test]
fn looks_conversational_false_for_code_or_third_person() {
    assert!(!looks_conversational(
        "fn main() { let x = 42; println!(\"{}\", x); }"
    ));
    assert!(!looks_conversational(
        "The user opened the file and wrote a function."
    ));
}

#[test]
fn synthesize_doc_returns_none_for_empty() {
    assert_eq!(synthesize_doc(&[]), None);
}

#[test]
fn synthesize_doc_joins_phrases() {
    let phrases = vec!["black coffee".to_string(), "standing desks".to_string()];
    assert_eq!(
        synthesize_doc(&phrases),
        Some("User has mentioned: black coffee; standing desks".to_string()),
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ironrace-pref-extract`
Expected: most tests fail (`extract_v4` returns empty). `synthesize_doc_*` and `looks_conversational_*` should already pass against the stubs from Task 1.

- [ ] **Step 3: Implement the V4 regex set**

Replace `crates/ironrace-pref-extract/src/patterns.rs` with:
```rust
//! V4 preference regex set, ported verbatim from mempalace
//! (`benchmarks/longmemeval_bench.py:1587-1610`). Compiled once on first use
//! via `OnceLock`.

use std::sync::OnceLock;

use regex::Regex;

const MAX_PHRASES: usize = 12;
const MIN_LEN: usize = 5;
const MAX_LEN: usize = 80;

/// Raw V4 patterns. Each is wrapped with a leading `(?i)` for case-insensitive
/// matching at compile time. Order matches mempalace's V4 order.
const RAW: &[&str] = &[
    r"i(?:'ve been| have been) having (?:trouble|issues?|problems?) with ([^,\.!?]{5,80})",
    r"i(?:'ve been| have been) feeling ([^,\.!?]{5,60})",
    r"i(?:'ve been| have been) (?:struggling|dealing) with ([^,\.!?]{5,80})",
    r"i(?:'ve been| have been) (?:worried|concerned) about ([^,\.!?]{5,80})",
    r"i(?:'m| am) (?:worried|concerned) about ([^,\.!?]{5,80})",
    r"i prefer ([^,\.!?]{5,60})",
    r"i usually ([^,\.!?]{5,60})",
    r"i(?:'ve been| have been) (?:trying|attempting) to ([^,\.!?]{5,80})",
    r"i(?:'ve been| have been) (?:considering|thinking about) ([^,\.!?]{5,80})",
    r"lately[,\s]+(?:i've been|i have been|i'm|i am) ([^,\.!?]{5,80})",
    r"recently[,\s]+(?:i've been|i have been|i'm|i am) ([^,\.!?]{5,80})",
    r"i(?:'ve been| have been) (?:working on|focused on|interested in) ([^,\.!?]{5,80})",
    r"i want to ([^,\.!?]{5,60})",
    r"i(?:'m| am) looking (?:to|for) ([^,\.!?]{5,60})",
    r"i(?:'m| am) thinking (?:about|of) ([^,\.!?]{5,60})",
    r"i(?:'ve been| have been) (?:noticing|experiencing) ([^,\.!?]{5,80})",
    r"i (?:still )?remember (?:the |my )?([^,\.!?]{5,80})",
    r"i used to ([^,\.!?]{5,60})",
    r"when i was (?:in high school|in college|young|a kid|growing up)[,\s]+([^,\.!?]{5,80})",
    r"growing up[,\s]+([^,\.!?]{5,80})",
    // Last pattern has no capture group; we fall back to the full match.
    r"(?:happy|fond|good|positive) (?:high school|college|childhood|school) (?:experience|memory|memories|time)[^,\.!?]{0,60}",
];

fn compiled() -> &'static [Regex] {
    static V: OnceLock<Vec<Regex>> = OnceLock::new();
    V.get_or_init(|| {
        RAW.iter()
            .map(|p| Regex::new(&format!("(?i){p}")).expect("V4 pref pattern must compile"))
            .collect()
    })
}

pub(crate) fn extract_v4(text: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for re in compiled() {
        for caps in re.captures_iter(text) {
            let m = caps.get(1).or_else(|| caps.get(0));
            let raw = match m {
                Some(m) => m.as_str(),
                None => continue,
            };
            let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || ".,;!?".contains(c));
            if trimmed.len() < MIN_LEN || trimmed.len() > MAX_LEN {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if !seen.iter().any(|s| s.eq_ignore_ascii_case(&lower)) {
                seen.push(trimmed.to_string());
                if seen.len() >= MAX_PHRASES {
                    return seen;
                }
            }
        }
    }
    seen
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ironrace-pref-extract`
Expected: all tests pass.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p ironrace-pref-extract -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/ironrace-pref-extract
git commit -m "feat(pref-extract): port mempalace V4 regex set + extractor trait"
```

---

## Task 3: Add `pref_enrich_enabled()` tunable in ironmem

**Files:**
- Modify: `crates/ironmem/src/search/tunables.rs`

- [ ] **Step 1: Read the existing tunables module**

`Read crates/ironmem/src/search/tunables.rs` lines 1-50 to confirm the `env_bool` helper signature you'll reuse.

- [ ] **Step 2: Add the tunable**

Append to `crates/ironmem/src/search/tunables.rs` (place under the rerank tunables, right before the file ends):
```rust
// ── E5: preference enrichment (off by default) ───────────────────────────────

/// `IRONMEM_PREF_ENRICH=1` enables the synthetic-preference-doc enrichment
/// at ingest time and the search-pipeline collapse step that hides the
/// synthetic from results. Default OFF; the LongMemEval bench flips it on
/// to measure the recall lift on `single-session-preference` questions.
pub fn pref_enrich_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("IRONMEM_PREF_ENRICH", false))
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p ironmem`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/ironmem/src/search/tunables.rs
git commit -m "feat(ironmem): tunable pref_enrich_enabled() (default off)"
```

---

## Task 4: Wire `ironrace-pref-extract` dep into ironmem

**Files:**
- Modify: `crates/ironmem/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `crates/ironmem/Cargo.toml`'s `[dependencies]` section, add (alphabetical placement next to `ironrace-rerank`):
```toml
ironrace-pref-extract = { path = "../ironrace-pref-extract" }
```

- [ ] **Step 2: Verify the workspace builds**

Run: `cargo build`
Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add crates/ironmem/Cargo.toml Cargo.lock
git commit -m "feat(ironmem): depend on ironrace-pref-extract"
```

---

## Task 5: Cascade-delete sibling drawers when parent is deleted

**Files:**
- Modify: `crates/ironmem/src/db/drawers.rs`
- Create: `crates/ironmem/tests/preference_enrichment_test.rs` (test 1 only — more added in Task 6)

- [ ] **Step 1: Write the failing integration test**

`crates/ironmem/tests/preference_enrichment_test.rs`:
```rust
//! Integration tests for the preference-enrichment ingest pass.

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::{json, Value};

fn request(method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

fn call(app: &App, tool: &str, args: Value) -> Value {
    let req = request("tools/call", json!({ "name": tool, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(
        resp.error.is_none(),
        "unexpected RPC error calling {tool}: {:?}",
        resp.error
    );
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    serde_json::from_str(text).expect("tool response must be valid JSON")
}

#[test]
fn deleting_parent_cascades_to_synthetic_sibling() {
    // Insert one parent + one synthetic sibling pointing at it directly via
    // the DB layer (so this test doesn't depend on Task 6's enrichment wiring).
    let app = App::open_for_test().expect("build test app");
    let parent_id = "a".repeat(32);
    let synth_id = "b".repeat(32);
    let zero_vec: Vec<f32> = vec![0.0; 384];

    app.db
        .insert_drawer(&parent_id, "parent", &zero_vec, "w", "r", "", "test")
        .unwrap();
    app.db
        .insert_drawer(
            &synth_id,
            "User has mentioned: thing",
            &zero_vec,
            "w",
            "r",
            &format!("pref:{parent_id}"),
            "test",
        )
        .unwrap();

    let deleted = call(&app, "delete_drawer", json!({ "id": parent_id }));
    assert_eq!(deleted["success"], true);

    // Synthetic sibling must be gone too.
    let got = app.db.get_drawer(&synth_id).unwrap();
    assert!(got.is_none(), "synthetic sibling should cascade-delete");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ironmem --test preference_enrichment_test deleting_parent_cascades_to_synthetic_sibling`
Expected: FAIL — synthetic row still exists after parent delete.

- [ ] **Step 3: Implement the cascade**

In `crates/ironmem/src/db/drawers.rs`, locate `delete_drawer_tx` (around line 176) and add a sibling delete *before* the parent delete:
```rust
pub(crate) fn delete_drawer_tx(tx: &Transaction<'_>, id: &str) -> Result<bool, MemoryError> {
    // Cascade: any synthetic sibling drawer points back via source_file = "pref:<id>".
    Self::delete_drawers_by_parent_tx(tx, id)?;
    let count = Self::delete_drawer_conn(tx, id)?;
    Ok(count > 0)
}
```

Add the new method below the existing `delete_drawers_by_source_file_tx` (around line 186). FTS deletes go first so the inner SELECT can still see the row IDs we're about to remove:
```rust
pub(crate) fn delete_drawers_by_parent_tx(
    tx: &Transaction<'_>,
    parent_id: &str,
) -> Result<usize, MemoryError> {
    let sentinel = format!("pref:{parent_id}");
    let _ = tx.execute(
        "DELETE FROM drawers_fts WHERE drawer_id IN \
         (SELECT id FROM drawers WHERE source_file = ?1)",
        params![sentinel],
    );
    let n = tx.execute(
        "DELETE FROM drawers WHERE source_file = ?1",
        params![sentinel],
    )?;
    Ok(n)
}
```

Confirm the MCP delete path goes through `delete_drawer_tx` (it does as of `crates/ironmem/src/mcp/tools/drawers.rs:81-85` — `handle_delete_drawer` wraps `delete_drawer_tx` in a `with_transaction`). No changes needed to the non-`_tx` variant for this plan; if a future caller of the non-`_tx` `delete_drawer` needs the cascade, mirror this method against `&rusqlite::Connection` then.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ironmem --test preference_enrichment_test deleting_parent_cascades_to_synthetic_sibling`
Expected: PASS.

- [ ] **Step 5: Run the existing drawer tests to confirm no regression**

Run: `cargo test -p ironmem db::drawers`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/src/db/drawers.rs crates/ironmem/tests/preference_enrichment_test.rs
git commit -m "feat(ironmem): cascade-delete synthetic preference siblings"
```

---

## Task 6: Wire enrichment into `handle_add_drawer`

**Files:**
- Modify: `crates/ironmem/src/mcp/tools/drawers.rs`
- Modify: `crates/ironmem/tests/preference_enrichment_test.rs` (add tests)

- [ ] **Step 1: Add the failing tests**

Append to `crates/ironmem/tests/preference_enrichment_test.rs`:
```rust
const CONVERSATIONAL_BODY: &str = "I've been having trouble with the battery life on my phone lately. \
I prefer carrying a small power bank when I travel. Lately, I've been thinking about switching to a \
phone with a removable battery. I usually plug in overnight.";

fn count_drawers(app: &App) -> usize {
    app.db.count_drawers(None).unwrap()
}

#[test]
fn enrich_off_inserts_only_one_row() {
    std::env::remove_var("IRONMEM_PREF_ENRICH");
    let app = App::open_for_test().expect("build app");
    let added = call(
        &app,
        "add_drawer",
        json!({
            "content": CONVERSATIONAL_BODY,
            "wing": "diary",
            "room": "general"
        }),
    );
    assert_eq!(added["success"], true);
    assert_eq!(count_drawers(&app), 1);
}

#[test]
fn enrich_on_inserts_parent_plus_synthetic() {
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");
    let added = call(
        &app,
        "add_drawer",
        json!({
            "content": CONVERSATIONAL_BODY,
            "wing": "diary",
            "room": "general"
        }),
    );
    assert_eq!(added["success"], true);

    // Two rows: the parent, and a sibling whose source_file is "pref:<parent>".
    assert_eq!(count_drawers(&app), 2);
    let parent_id = added["id"].as_str().unwrap();
    let sentinel = format!("pref:{parent_id}");
    let siblings = app
        .db
        .get_drawers(None, None, 100)
        .unwrap()
        .into_iter()
        .filter(|d| d.source_file == sentinel)
        .collect::<Vec<_>>();
    assert_eq!(siblings.len(), 1, "exactly one synthetic sibling");
    assert!(siblings[0].content.starts_with("User has mentioned: "));

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

#[test]
fn enrich_on_skips_non_conversational_input() {
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");
    let rust_source = "fn main() { let x = 42; println!(\"{}\", x); }";
    let added = call(
        &app,
        "add_drawer",
        json!({ "content": rust_source, "wing": "code", "room": "rust" }),
    );
    assert_eq!(added["success"], true);
    assert_eq!(count_drawers(&app), 1, "non-conversational → no sibling");

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}
```

Note: the `pref_enrich_enabled()` tunable is `OnceLock`-cached (set once per process). To keep these tests independent in a single test binary, run them serially. The simplest path: rely on `cargo test`'s default per-test isolation here is *not* sufficient because cargo runs tests in a shared process.

Add a single shared mutex at the top of the file:
```rust
use std::sync::Mutex;

// Serializes the IRONMEM_PREF_ENRICH-touching tests because the tunable is
// process-cached after first read.
static ENV_LOCK: Mutex<()> = Mutex::new(());
```
…and acquire it as the first line of each `enrich_*` test:
```rust
let _g = ENV_LOCK.lock().unwrap();
```

Important: the `pref_enrich_enabled()` tunable's `OnceLock` will latch the first value it sees. To exercise both ON and OFF in the same binary, we change the tunable definition (next step) to read the env var on every call instead of caching it. Update the spec note in the previous task is already aligned with this — small change only.

- [ ] **Step 2: Update the tunable to read env on every call**

In `crates/ironmem/src/search/tunables.rs`, replace the `pref_enrich_enabled` body added in Task 3 with:
```rust
pub fn pref_enrich_enabled() -> bool {
    // Not OnceLock-cached: the integration tests need to flip it per-test.
    // Runtime cost is one env-var read per add_drawer / search call, which is
    // negligible vs an embed or HNSW probe.
    matches!(
        std::env::var("IRONMEM_PREF_ENRICH").as_deref(),
        Ok("1") | Ok("true")
    )
}
```

- [ ] **Step 3: Run the new tests; expect failures**

Run: `cargo test -p ironmem --test preference_enrichment_test enrich_`
Expected: `enrich_on_inserts_parent_plus_synthetic` fails (only 1 row inserted); `enrich_on_skips_non_conversational_input` passes (no enrichment yet); `enrich_off_inserts_only_one_row` passes.

- [ ] **Step 4: Implement the enrichment in `handle_add_drawer`**

Replace the body of `crates/ironmem/src/mcp/tools/drawers.rs::handle_add_drawer` with:
```rust
pub(super) fn handle_add_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    if app.is_warming_up() {
        return Ok(json!({
            "warming_up": true,
            "message": "Memory server is initializing. Please retry in a moment.",
        }));
    }
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("content is required".into()))?;
    let wing = args
        .get("wing")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("wing is required".into()))?;
    let room = args
        .get("room")
        .and_then(|v| v.as_str())
        .unwrap_or("general");

    let content = sanitize::sanitize_content(content, 100_000)?;
    let wing = sanitize::sanitize_name(wing, "wing")?;
    let room = sanitize::sanitize_name(room, "room")?;

    let id = crate::db::drawers::generate_id(content, &wing, &room);

    app.ensure_embedder_ready()?;

    let embedding = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        emb.embed_one(content).map_err(MemoryError::Embed)?
    };

    // Compute synthetic sibling, if enrichment is enabled and content qualifies.
    let synth: Option<(String, String, Vec<f32>)> = build_synthetic(app, content, &wing, &room, &id)?;

    app.db.with_transaction(|tx| {
        crate::db::schema::Database::insert_drawer_tx(
            tx, &id, content, &embedding, &wing, &room, "", "mcp",
        )?;
        if let Some((sid, scontent, semb)) = synth.as_ref() {
            let parent_ref = format!("pref:{id}");
            crate::db::schema::Database::insert_drawer_tx(
                tx, sid, scontent, semb, &wing, &room, &parent_ref, "mcp",
            )?;
        }
        crate::db::schema::Database::wal_log_tx(
            tx,
            "add_drawer",
            &json!({"id": &id, "wing": &wing, "room": &room, "synth": synth.is_some()}),
            None,
        )?;
        Ok(())
    })?;

    app.insert_into_index(&id, &embedding)?;
    if let Some((sid, _, semb)) = synth.as_ref() {
        app.insert_into_index(sid, semb)?;
    }

    Ok(json!({
        "success": true,
        "id": id,
        "wing": wing,
        "room": room,
        "synth": synth.is_some(),
    }))
}

/// Build a synthetic preference-enrichment drawer, or return Ok(None) if the
/// tunable is off, the content doesn't look conversational, or the extractor
/// produced no phrases. A failure to embed the synthetic body logs at warn
/// and returns Ok(None) — the parent insert continues unaffected.
fn build_synthetic(
    app: &App,
    content: &str,
    wing: &str,
    room: &str,
    parent_id: &str,
) -> Result<Option<(String, String, Vec<f32>)>, MemoryError> {
    use ironrace_pref_extract::{
        looks_conversational, synthesize_doc, PreferenceExtractor, RegexPreferenceExtractor,
    };

    if !crate::search::tunables::pref_enrich_enabled() {
        return Ok(None);
    }
    if !looks_conversational(content) {
        return Ok(None);
    }
    let phrases = RegexPreferenceExtractor.extract(content);
    let synth_body = match synthesize_doc(&phrases) {
        Some(s) => s,
        None => return Ok(None),
    };
    let synth_id = crate::db::drawers::generate_id(&synth_body, wing, room);
    let synth_emb = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        match emb.embed_one(&synth_body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, parent = parent_id, "pref_enrich embed failed; skipping synth");
                return Ok(None);
            }
        }
    };
    tracing::debug!(
        parent = parent_id,
        synth = %synth_id,
        phrases = phrases.len(),
        "pref_enrich"
    );
    Ok(Some((synth_id, synth_body, synth_emb)))
}
```

- [ ] **Step 5: Run the new tests; expect all to pass**

Run: `cargo test -p ironmem --test preference_enrichment_test`
Expected: all four tests pass.

- [ ] **Step 6: Run the full test suite to confirm no regression**

Run: `cargo test -p ironmem`
Expected: all green.

- [ ] **Step 7: Lint**

Run: `cargo clippy -p ironmem -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/ironmem/src/mcp/tools/drawers.rs crates/ironmem/src/search/tunables.rs crates/ironmem/tests/preference_enrichment_test.rs
git commit -m "feat(ironmem): synthesize preference sibling drawer at add_drawer time"
```

---

## Task 7: `collapse_synthetic_into_parents` in the search pipeline

**Files:**
- Modify: `crates/ironmem/src/search/pipeline.rs`
- Modify: `crates/ironmem/src/search/mod.rs`
- Create: `crates/ironmem/tests/search_collapse_test.rs`

- [ ] **Step 1: Write the failing unit-style integration test**

`crates/ironmem/tests/search_collapse_test.rs`:
```rust
//! Pipeline-level tests for the synthetic-doc collapse step (step 7.5).

use ironmem::db::ScoredDrawer;
use ironmem::db::SearchFilters;
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use ironmem::search::collapse_synthetic_into_parents;
use serde_json::{json, Value};

fn fixture_drawer(id: &str, content: &str, source_file: &str, score: f32) -> ScoredDrawer {
    ScoredDrawer {
        drawer: ironmem::db::Drawer {
            id: id.to_string(),
            content: content.to_string(),
            wing: "w".to_string(),
            room: "r".to_string(),
            source_file: source_file.to_string(),
            added_by: "test".to_string(),
            filed_at: "2026-04-29".to_string(),
            date: "2026-04-29".to_string(),
        },
        score,
    }
}

#[test]
fn synth_above_parent_promotes_parent_score_and_drops_synth() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "p".repeat(32);
    let synth_id = "s".repeat(32);

    // Parent already loaded; synth ranks higher.
    let mut scored = vec![
        fixture_drawer(&synth_id, "User has mentioned: x", &format!("pref:{parent_id}"), 0.9),
        fixture_drawer(&parent_id, "parent body", "", 0.4),
    ];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    // Synth dropped; parent remains with the elevated score.
    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.9).abs() < 1e-6);
}

#[test]
fn parent_above_synth_keeps_parent_unchanged_and_drops_synth() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "p".repeat(32);
    let synth_id = "s".repeat(32);

    let mut scored = vec![
        fixture_drawer(&parent_id, "parent body", "", 0.7),
        fixture_drawer(&synth_id, "User has mentioned: x", &format!("pref:{parent_id}"), 0.5),
    ];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.7).abs() < 1e-6);
}

#[test]
fn orphan_synth_fetches_missing_parent_when_present_in_db() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "a".repeat(32);
    let synth_id = "b".repeat(32);
    let zero_vec: Vec<f32> = vec![0.0; 384];

    // Insert parent in DB but NOT in `scored` (parent didn't make HNSW top-N).
    app.db
        .insert_drawer(&parent_id, "parent body", &zero_vec, "w", "r", "", "test")
        .unwrap();
    app.db
        .insert_drawer(
            &synth_id,
            "User has mentioned: x",
            &zero_vec,
            "w",
            "r",
            &format!("pref:{parent_id}"),
            "test",
        )
        .unwrap();

    let mut scored = vec![fixture_drawer(
        &synth_id,
        "User has mentioned: x",
        &format!("pref:{parent_id}"),
        0.8,
    )];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    // Synth dropped; parent surfaced from DB with synth's score.
    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.8).abs() < 1e-6);
}

#[test]
fn orphan_synth_with_deleted_parent_drops_quietly() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "z".repeat(32);
    let synth_id = "y".repeat(32);

    // Parent is NOT in DB and NOT in `scored`.
    let mut scored = vec![fixture_drawer(
        &synth_id,
        "User has mentioned: x",
        &format!("pref:{parent_id}"),
        0.6,
    )];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    assert!(scored.is_empty(), "orphan synth without parent must drop");
}

fn request(method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

fn _call(app: &App, tool: &str, args: Value) -> Value {
    let req = request("tools/call", json!({ "name": tool, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    serde_json::from_str(text).expect("tool response must be valid JSON")
}

#[test]
fn search_response_never_contains_synthetic_marker() {
    // End-to-end: with enrichment ON, search for the conversational topic and
    // verify no result row's content starts with the synthetic marker.
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");
    let _filters = SearchFilters::default();

    // Add a conversational drawer (creates parent + synth via Task 6).
    let _ = _call(
        &app,
        "add_drawer",
        json!({
            "content": "I've been having trouble with the battery life on my phone. \
                        I prefer carrying a small power bank when I travel.",
            "wing": "diary",
            "room": "general"
        }),
    );

    // Search — both rows in HNSW; collapse must hide the synth.
    let resp = _call(&app, "search", json!({ "query": "battery", "limit": 10 }));
    let results = resp["results"].as_array().unwrap();
    for r in results {
        let body = r["content"].as_str().unwrap_or("");
        assert!(
            !body.starts_with("User has mentioned: "),
            "synthetic doc leaked into search response: {body}",
        );
    }
    std::env::remove_var("IRONMEM_PREF_ENRICH");
}
```

- [ ] **Step 2: Run tests; expect failure due to missing symbol**

Run: `cargo test -p ironmem --test search_collapse_test`
Expected: compile error — `collapse_synthetic_into_parents` does not exist.

- [ ] **Step 3: Implement `collapse_synthetic_into_parents`**

Append to `crates/ironmem/src/search/pipeline.rs` (above the closing of the file's helper section):
```rust
/// Step 7.5: collapse synthetic preference siblings into their parent rows.
///
/// A synthetic drawer carries `source_file = "pref:<parent_drawer_id>"`. After
/// scoring, we want the parent to absorb the synth's score (if higher) and the
/// synth to disappear from the candidate list. If the parent is missing from
/// `candidates` (because it didn't make HNSW top-N) but the synth did, fetch
/// the parent by id and surface it with the synth's score; drop the synth.
/// If the parent has been deleted from the DB, drop the synth quietly.
///
/// This runs *before* the rerank stages so all downstream scoring sees only
/// real drawers and so RRF/KG scores remain commensurable.
pub fn collapse_synthetic_into_parents(
    app: &App,
    candidates: &mut Vec<ScoredDrawer>,
) -> Result<(), MemoryError> {
    const SENTINEL: &str = "pref:";

    // Partition: (synth, real). Both keep insertion order to keep the ordering
    // step downstream deterministic.
    let mut synths: Vec<ScoredDrawer> = Vec::new();
    let mut reals: Vec<ScoredDrawer> = Vec::with_capacity(candidates.len());
    for sd in candidates.drain(..) {
        if sd.drawer.source_file.starts_with(SENTINEL) {
            synths.push(sd);
        } else {
            reals.push(sd);
        }
    }

    if synths.is_empty() {
        *candidates = reals;
        return Ok(());
    }

    // Build a parent_id → real-index map for O(1) lookup.
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, r) in reals.iter().enumerate() {
        by_id.insert(r.drawer.id.clone(), i);
    }

    // First pass: promote scores for parents already in `reals`. Defer
    // orphan-parent fetches to a single batched DB call.
    let mut orphan_parent_ids: Vec<String> = Vec::new();
    let mut orphan_scores: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();

    for s in synths {
        let parent_id = &s.drawer.source_file[SENTINEL.len()..];
        if let Some(&idx) = by_id.get(parent_id) {
            if s.score > reals[idx].score {
                reals[idx].score = s.score;
            }
        } else {
            // Track the highest synth score per orphan parent.
            let cur = orphan_scores.entry(parent_id.to_string()).or_insert(s.score);
            if s.score > *cur {
                *cur = s.score;
            }
            if !orphan_parent_ids.iter().any(|pid| pid == parent_id) {
                orphan_parent_ids.push(parent_id.to_string());
            }
        }
    }

    if !orphan_parent_ids.is_empty() {
        let id_refs: Vec<&str> = orphan_parent_ids.iter().map(|s| s.as_str()).collect();
        let fetched = app.db.get_drawers_by_ids(&id_refs)?;
        for pid in &orphan_parent_ids {
            if let Some(parent) = fetched.get(pid) {
                let score = orphan_scores.get(pid).copied().unwrap_or(0.0);
                reals.push(ScoredDrawer {
                    drawer: parent.clone(),
                    score,
                });
            }
            // else: parent deleted between index and query — drop quietly.
        }
    }

    *candidates = reals;
    Ok(())
}
```

- [ ] **Step 4: Re-export the fn for tests**

In `crates/ironmem/src/search/mod.rs`, add:
```rust
pub use pipeline::collapse_synthetic_into_parents;
```
Confirm `mod pipeline;` already declares the module; add `pub mod pipeline;` if it isn't already public.

Check: the existing search/mod.rs is 8 lines; read it first and make a minimal addition.

- [ ] **Step 5: Run the unit-style tests; expect them to pass and the e2e to still fail**

Run: `cargo test -p ironmem --test search_collapse_test`
Expected: the four `synth_*`/`parent_*`/`orphan_*` tests pass; `search_response_never_contains_synthetic_marker` fails because pipeline doesn't yet call collapse.

- [ ] **Step 6: Insert step 7.5 into `pipeline::search`**

In `crates/ironmem/src/search/pipeline.rs::search`, locate step 7 (KG boost; around line 300-302):
```rust
    // Step 7: KG score boosts (inert when entities table is empty)
    let kg = KnowledgeGraph::new(&app.db);
    kg_boost(&mut scored, &sanitized.clean_query, &kg)?;
```
Insert immediately after:
```rust
    // Step 7.5: collapse synthetic preference siblings into their parent rows.
    // Cheap when no synthetic hit is in `scored` (single partition pass + early
    // return). Always-on by structural check; the only way a synth hit reaches
    // here is if pref-enrichment was enabled at ingest time.
    collapse_synthetic_into_parents(app, &mut scored)?;
```
Update the file-level doc comment at the top to add the new step:
```rust
//! Search pipeline: sanitize → embed → HNSW (multi-query) → BM25 → RRF → KG boost
//!     → collapse synthetic preference siblings → shrinkage rerank → LLM rerank → rank.
```

- [ ] **Step 7: Run the e2e test; expect it to pass**

Run: `cargo test -p ironmem --test search_collapse_test search_response_never_contains_synthetic_marker`
Expected: PASS.

- [ ] **Step 8: Run the full ironmem test suite**

Run: `cargo test -p ironmem`
Expected: all green. Pay special attention to existing rerank tests — they must still pass because step 7.5 is a no-op when no synth hits are present.

- [ ] **Step 9: Lint and format**

Run: `cargo fmt --all && cargo clippy --workspace -- -D warnings`
Expected: no warnings.

- [ ] **Step 10: Commit**

```bash
git add crates/ironmem/src/search/pipeline.rs crates/ironmem/src/search/mod.rs crates/ironmem/tests/search_collapse_test.rs
git commit -m "feat(ironmem): step 7.5 collapses synthetic preference siblings into parents"
```

---

## Task 8: Bench verification of the acceptance criterion

**Files:** none (read-only verification using existing harness)

- [ ] **Step 1: Run the bench OFF for the baseline**

Run:
```bash
IRONMEM_PREF_ENRICH=0 \
python3 scripts/benchmark_longmemeval.py \
    --limit 165 \
    --per-question-json /tmp/pref_off.json
```
Expected: completes; prints overall and per-type R@k tables. Note `single-session-preference R@5` value as `R5_off`.

- [ ] **Step 2: Run the bench ON**

Run:
```bash
IRONMEM_PREF_ENRICH=1 \
python3 scripts/benchmark_longmemeval.py \
    --limit 165 \
    --per-question-json /tmp/pref_on.json
```
Expected: completes; note `single-session-preference R@5` as `R5_on`.

- [ ] **Step 3: Check the acceptance criterion**

Run:
```bash
python3 - <<'PY'
import json
off = json.load(open("/tmp/pref_off.json"))
on  = json.load(open("/tmp/pref_on.json"))
def at(d, k):
    return d["per_type"].get("single-session-preference", {}).get(k, 0.0) * 100
print(f"preference R@5: off={at(off,5):.1f}%  on={at(on,5):.1f}%  delta={at(on,5)-at(off,5):+.2f}pp")
for t in ("knowledge-update","multi-session","single-session-user",
          "single-session-assistant","temporal-reasoning"):
    a = on["per_type"].get(t,{}).get(5,0.0)*100
    b = off["per_type"].get(t,{}).get(5,0.0)*100
    print(f"{t} R@5: off={b:.1f}%  on={a:.1f}%  delta={a-b:+.2f}pp")
PY
```
Expected:
- `preference R@5 delta` ≥ +2.00pp.
- No other category's `delta` < -0.50pp.

If the preference delta is below +2pp:
- Verify enrichment fired: grep for `pref_enrich` in any captured logs.
- Inspect a missed question's gold session to see if PREF_PATTERNS would have matched the user turn (port the regex check to a Python one-liner against the gold session text).
- Likely failure modes: V4 patterns require specific phrasings, the embedding model places the synthetic doc in a different region than mempalace's, or the LongMemEval slice you ran has different per-type variance than the full 500.

- [ ] **Step 4: Capture the verification numbers in the commit message**

```bash
git commit --allow-empty -m "chore(bench): preference R@5 +X.Xpp with IRONMEM_PREF_ENRICH=1

Off:           single-session-preference R@5 = ...%
On:            single-session-preference R@5 = ...%
Delta:         +X.Xpp
Other deltas:  ...
"
```

---

## Self-review

| Spec section | Plan task |
|---|---|
| New crate `ironrace-pref-extract` | Tasks 1-2 |
| `pref_enrich_enabled()` tunable | Tasks 3, 6 (uncached form) |
| ironmem dep | Task 4 |
| `delete_drawers_by_parent_tx` + cascade | Task 5 |
| `handle_add_drawer` enrichment | Task 6 |
| `collapse_synthetic_into_parents` | Task 7 |
| Step 7.5 wiring in `pipeline::search` | Task 7 step 6 |
| Re-export for tests | Task 7 step 4 |
| Pattern coverage tests | Task 2 |
| Enrichment integration tests | Tasks 5-6 |
| Collapse integration tests | Task 7 |
| Bench acceptance run | Task 8 |
| No schema migration | (no migration task, intentional) |
| Default OFF | Task 3 + Task 6 step 2 (env-read on every call) |
| Observability `tracing::debug!` | Task 6 step 4 (`pref_enrich` debug line) |

**Type consistency check:**
- `RegexPreferenceExtractor` (struct) used uniformly in Tasks 2, 6.
- `collapse_synthetic_into_parents(app, &mut Vec<ScoredDrawer>)` signature consistent in Tasks 7 step 3, 4, 6.
- `delete_drawers_by_parent_tx` named consistently in Task 5.
- `pref_enrich_enabled()` returns `bool` everywhere referenced.
- Tests use `App::open_for_test()` consistently — confirm this exists; if not, replace with whatever the codebase uses (rerank tests use `App::with_reranker`, which itself calls `App::open_for_test`).

**Placeholder scan:** no TBDs, no "implement appropriately" stubs, no "similar to Task N" hand-waves. Every code step shows the code.

**Verified at plan time:**
- `App::open_for_test()` exists and is public — `crates/ironmem/src/mcp/app.rs:135`.
- `handle_delete_drawer` already routes through `delete_drawer_tx` — `crates/ironmem/src/mcp/tools/drawers.rs:81-85`.
- No metrics framework is wired in `ironmem` (no `metrics::`, `prometheus`, or counter macros), so the `tracing::debug!` line in Task 6 is the full observability surface for this plan; no separate counter task.
