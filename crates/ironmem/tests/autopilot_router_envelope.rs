//! The `/evaluate-issue` router's AUTOPILOT envelope, checked against the
//! library's own `RiskClass::is_low_risk` (#336).
//!
//! This is an integration test, not a unit test in `review.rs`, on purpose. A
//! unit test sees only the `cfg(test)` build of the library, so a
//! `#[cfg(not(test))]` twin of `is_low_risk`, or of a helper it calls, could
//! widen the shipped auto-merge envelope while every unit test passes. An
//! integration test links the library as built for integration tests, without
//! `cfg(test)`, the way the binary links it. What can still differ from a
//! release build is logic gated on `cfg(debug_assertions)` (on in the test
//! profile, off in release) or on a feature this test run does not enable.

use std::path::Path;

use ironmem::autopilot::review::{RiskClass, REVIEW_VERDICT_JSON_SCHEMA};
use serde::Deserialize;

const AUTO_MERGE_OPENING: &str = "AUTOPILOT auto-merges only these classes:";
const NEVER_RECOMMEND_OPENING: &str = "Never recommend AUTOPILOT when the expected class is";
const NEVER_RECOMMEND_CONTINUATION: &str = " — `decide_merge` holds those";
const SURFACES: [&str; 3] = [
    "docs/EVALUATE_ISSUE.md",
    ".claude-plugin/commands/evaluate-issue.md",
    ".codex-plugin/prompts/evaluate-issue.md",
];

/// Every `RiskClass` variant, in declaration order, read from the variant
/// list that `#[derive(Deserialize)]` generates rather than kept by hand.
///
/// The derive passes every deserializable variant's serialized name, and its
/// aliases, to `Deserializer::deserialize_enum`; `VariantNames` captures that
/// list, each name is parsed back into a `RiskClass`, and duplicates (an
/// alias and its variant) are dropped, keeping declaration order. A newly
/// declared variant therefore appears here with no edit to this helper.
///
/// A variant marked `#[serde(skip)]` or `#[serde(skip_deserializing)]` is
/// left out of the derive's list. That gap is closed two ways. The envelope
/// test refuses `#[serde(skip*)]` inside the enum. And it compares this list
/// with `REVIEW_VERDICT_JSON_SCHEMA`, so a skipped class the schema allows
/// fails that check. A skipped class outside the schema is left: the
/// reviewer's reply is parsed by `parse_review_message`, which accepts only
/// what `RiskClass::from_schema_str` accepts, and a Codex reviewer is
/// additionally held to the schema.
fn every_risk_class() -> Vec<RiskClass> {
    use serde::de::value::Error;
    use serde::de::{Deserializer, Error as _, Visitor};

    struct VariantNames(Option<&'static [&'static str]>);

    impl<'de> Deserializer<'de> for &mut VariantNames {
        type Error = Error;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Error> {
            Err(Error::custom("RiskClass must deserialize as an enum"))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Error> {
            self.0 = Some(variants);
            Err(Error::custom("variant names captured"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str
            string bytes byte_buf option unit unit_struct newtype_struct seq
            tuple tuple_struct map struct identifier ignored_any
        }
    }

    let mut captured = VariantNames(None);
    RiskClass::deserialize(&mut captured)
        .expect_err("the capturing deserializer never yields a value");
    let names = captured
        .0
        .expect("#[derive(Deserialize)] on RiskClass must call deserialize_enum");
    let mut classes: Vec<RiskClass> = Vec::new();
    for &name in names {
        let class: RiskClass = serde_json::from_value(serde_json::Value::from(name))
            .unwrap_or_else(|e| panic!("serde variant name {name:?} does not parse: {e}"));
        if !classes.contains(&class) {
            classes.push(class);
        }
    }
    for &class in &classes {
        assert_eq!(
            serde_json::to_value(class).unwrap(),
            class.as_str(),
            "RiskClass::as_str must match the serialized name of {class:?}"
        );
    }
    classes
}

/// The router's spelling of the held list: no items is the empty string,
/// one is `` `a` ``, two are `` `a` or `b` `` (no comma), and three or more
/// are `` `a`, `b`, or `c` `` (serial comma).
fn backticked_or_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// Every occurrence of `needle` in `haystack`, as the text after it.
fn texts_after<'a>(haystack: &'a str, needle: &str) -> Vec<&'a str> {
    haystack
        .match_indices(needle)
        .map(|(at, _)| &haystack[at + needle.len()..])
        .collect()
}

/// `pub enum RiskClass { … }` as written in `review.rs`.
fn risk_class_enum_source() -> &'static str {
    let source = include_str!("../src/autopilot/review.rs");
    let start = source
        .find("pub enum RiskClass {")
        .expect("review.rs must declare `pub enum RiskClass {`");
    let len = source[start..]
        .find("\n}\n")
        .expect("`pub enum RiskClass` must close with a `}` line");
    &source[start..start + len]
}

/// The byte offset just past the bracket or parenthesis that closes the
/// group opened right before `text`, or `text.len()` if it never closes.
///
/// Nested `(…)` and `[…]` are tracked, and string literals are skipped, so
/// `rename(serialize = "x")` or `rename = "a)b"` does not end the group.
fn group_end(text: &str) -> usize {
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for (at, ch) in text.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return at;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// `text` split at the commas outside nested groups and string literals.
fn top_level_args(text: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (at, ch) in text.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    args.push(&text[start..]);
    args
}

/// Every `skip`, `skip_*` or `other` argument of a `serde(…)` group in
/// `source`, each with the group it was found in.
///
/// Whitespace is removed first, so `#[ serde ( skip ) ]` is still seen. Each
/// `#[…]` attribute is read up to its own closing `]`, and every `serde(`
/// group inside it, not only a leading `#[serde(`, is checked, so
/// `#[cfg_attr(…, serde(skip))]` is caught. A group's arguments are split at
/// its top-level commas, with nested parentheses and string literals
/// tracked, so `#[serde(rename(serialize = "x"), skip_deserializing)]`
/// cannot hide the argument after the nested group.
fn forbidden_serde_args(source: &str) -> Vec<String> {
    let body: String = source.split_whitespace().collect();
    let mut found = Vec::new();
    for (at, opener) in body.match_indices("#[") {
        let attr_start = at + opener.len();
        let attribute = &body[attr_start..attr_start + group_end(&body[attr_start..])];
        for (serde_at, serde_opener) in attribute.match_indices("serde(") {
            let args_start = serde_at + serde_opener.len();
            let group = &attribute[args_start..args_start + group_end(&attribute[args_start..])];
            for arg in top_level_args(group) {
                let key_len = arg.find(['=', '(']).unwrap_or(arg.len());
                let key = &arg[..key_len];
                if key == "skip" || key.starts_with("skip_") || key == "other" {
                    found.push(format!("`{key}` in `serde({group})`"));
                }
            }
        }
    }
    found
}

/// The file's current text from the first `opening` up to and including the
/// next `.`, or up to the next newline, whichever comes first. A copy
/// wrapped across lines is not found in `raw`, so it is read from the
/// whitespace-collapsed `body` instead, up to its next `.`.
fn current_text_after(raw: &str, body: &str, opening: &str) -> String {
    let (source, stops): (&str, &[char]) = if raw.contains(opening) {
        (raw, &['.', '\n'])
    } else {
        (body, &['.'])
    };
    let Some(at) = source.find(opening) else {
        return String::new();
    };
    let rest = &source[at..];
    match rest.find(stops) {
        Some(end) if rest[end..].starts_with('.') => rest[..=end].to_string(),
        Some(end) => rest[..end].to_string(),
        None => rest.to_string(),
    }
}

/// What one router surface gets wrong about the two envelope sentences.
fn surface_failures(rel: &str, raw: &str, auto_merge: &str, never_recommend: &str) -> Vec<String> {
    let body = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut failures = Vec::new();
    // Where an opening appears exactly once, a mismatch also shows what the
    // file says after it, so the drift is visible beside the expectation.
    let currently = |opening: &str| {
        if body.matches(opening).count() == 1 {
            format!(
                "\n  currently:\n    {}",
                current_text_after(raw, &body, opening)
            )
        } else {
            String::new()
        }
    };

    for opening in [AUTO_MERGE_OPENING, NEVER_RECOMMEND_OPENING] {
        let count = body.matches(opening).count();
        if count != 1 {
            failures.push(format!(
                "{rel}: {opening:?} appears {count} times; expected exactly once"
            ));
        }
    }

    let count = body.matches(auto_merge).count();
    if count != 1 {
        failures.push(format!(
            "{rel}: the auto-merge sentence must appear exactly once, ending at its \
             own `.` (found {count}); expected:\n    {auto_merge}{}",
            currently(AUTO_MERGE_OPENING)
        ));
    }

    let after = texts_after(&body, never_recommend);
    let terminated = after
        .iter()
        .all(|rest| rest.starts_with('.') || rest.starts_with(NEVER_RECOMMEND_CONTINUATION));
    if after.len() != 1 || !terminated {
        failures.push(format!(
            "{rel}: the never-recommend sentence must appear exactly once (found {}), \
             followed by `.` or by {NEVER_RECOMMEND_CONTINUATION:?}; expected:\n    \
             {never_recommend}.{}",
            after.len(),
            currently(NEVER_RECOMMEND_OPENING)
        ));
    }
    failures
}

/// The two sentences the router must state, built from `is_low_risk`.
fn expected_sentences() -> (String, String) {
    let (low, held): (Vec<RiskClass>, Vec<RiskClass>) = every_risk_class()
        .into_iter()
        .partition(|class| class.is_low_risk());
    let low: Vec<&str> = low.iter().map(|class| class.as_str()).collect();
    let held: Vec<&str> = held.iter().map(|class| class.as_str()).collect();
    assert!(
        !low.is_empty(),
        "no RiskClass is low risk: nothing auto-merges"
    );
    assert!(
        !held.is_empty(),
        "every RiskClass is low risk: nothing is held"
    );

    let auto_merge = format!(
        "{AUTO_MERGE_OPENING} {}.",
        low.iter()
            .map(|class| format!("`{class}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let never_recommend = format!("{NEVER_RECOMMEND_OPENING} {}", backticked_or_list(&held));
    (auto_merge, never_recommend)
}

/// The `/evaluate-issue` router's AUTOPILOT envelope sentences are exactly
/// what `decide_merge` grants, in all three router surfaces.
///
/// This is a Rust test rather than a collab-template lint on purpose. It
/// builds the two expected sentences by calling the real
/// `RiskClass::is_low_risk` and `RiskClass::as_str` on every variant (listed
/// by serde's derive, see `every_risk_class`), so there is no parser of the
/// policy to fool: a commented-out arm, a nested `match`, or a conditional
/// `=> true` all mean whatever the compiler says they mean. The one source
/// scan below guards the assumption that makes the variant list complete: no
/// serde `skip` variant. It refuses `other` too, for the reason given on the
/// enum.
///
/// Per surface it pins: the auto-merge sentence, once, ending at its own
/// `.`; the "never recommend" sentence, once, followed by `.` or by
/// `` — `decide_merge` holds those ``; and each sentence's opening words,
/// once, so no widened second copy can sit elsewhere in the file. Text is
/// compared with whitespace runs collapsed, so reflowing a paragraph does
/// not trip it and a copy wrapped across lines is still counted.
///
/// Limit: this is a presence check over those two sentences. It checks no
/// other router prose, and it cannot stop contradicting prose placed after a
/// correct sentence (for example "…`test_only`. Also `logic` when small.").
/// PR review covers that.
#[test]
fn the_router_states_exactly_the_autopilot_envelope_is_low_risk_grants() {
    // `every_risk_class` reads the derive's variant list, which leaves out a
    // skipped variant. `other` is refused too, for the reason on the enum.
    let forbidden = forbidden_serde_args(risk_class_enum_source());
    assert!(
        forbidden.is_empty(),
        "RiskClass must not use serde `skip`/`skip_*` or `other` on a variant (found {}): \
         a skipped variant is left out of the derive's list, so it would slip past this \
         test, and `other` would silently absorb an unknown class in a stored review record",
        forbidden.join(", ")
    );

    let (auto_merge, never_recommend) = expected_sentences();

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut failures = Vec::new();
    for rel in SURFACES {
        let raw = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("{rel}: cannot read router surface: {e}"));
        failures.extend(surface_failures(rel, &raw, &auto_merge, &never_recommend));
    }

    // Separate from the router text: a class the enum declares but the
    // Reviewer's schema cannot return (or the reverse) is its own drift.
    // Collected rather than asserted, so the expected sentences above are
    // still printed when a new variant is the cause.
    let schema: serde_json::Value = serde_json::from_str(REVIEW_VERDICT_JSON_SCHEMA).unwrap();
    let mut schema_classes: Vec<&str> = schema["properties"]["risk_class"]["enum"]
        .as_array()
        .expect("schema must constrain risk_class to an enum")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let mut declared: Vec<&str> = every_risk_class().iter().map(|c| c.as_str()).collect();
    schema_classes.sort_unstable();
    declared.sort_unstable();
    if declared != schema_classes {
        failures.push(format!(
            "REVIEW_VERDICT_JSON_SCHEMA risk_class enum {schema_classes:?} does not match \
             the declared RiskClass variants {declared:?}"
        ));
    }
    assert!(
        failures.is_empty(),
        "the router's AUTOPILOT envelope drifted from RiskClass::is_low_risk:\n{}\n\n\
         The expected sentences are built from `RiskClass::is_low_risk`, not read from \
         the routers. If the auto-merge policy was not meant to change, revert \
         `is_low_risk`, not the router files; widening it is a spec change (see \
         `only_the_spec_s_four_classes_are_low_risk`).",
        failures.join("\n")
    );
}

/// The four classes the spec's *Who classifies, and when* section makes
/// eligible for auto-merge
/// (`docs/iron/specs/2026-08-21-autonomous-backlog-runner-design.md`).
const SPEC_LOW: [RiskClass; 4] = [
    RiskClass::Documentation,
    RiskClass::DependencyBump,
    RiskClass::MechanicalRename,
    RiskClass::TestOnly,
];

/// Every declared class, listed through serde's derive so a new variant is
/// covered with no edit here, is low risk exactly when the spec says so.
#[test]
fn only_the_spec_s_four_classes_are_low_risk() {
    for class in every_risk_class() {
        assert_eq!(
            class.is_low_risk(),
            SPEC_LOW.contains(&class),
            "RiskClass::{class:?}.is_low_risk() is {}, but the spec's auto-merge envelope is \
             exactly documentation, dependency_bump, mechanical_rename and test_only. \
             Widening the auto-merge envelope is a spec change, not a fix: change \
             docs/iron/specs/2026-08-21-autonomous-backlog-runner-design.md first, then \
             `is_low_risk` and SPEC_LOW together",
            class.is_low_risk()
        );
    }
}

#[test]
fn backticked_or_list_spells_one_two_and_three_items() {
    assert_eq!(backticked_or_list(&[]), "");
    assert_eq!(backticked_or_list(&["a"]), "`a`");
    assert_eq!(backticked_or_list(&["a", "b"]), "`a` or `b`");
    assert_eq!(backticked_or_list(&["a", "b", "c"]), "`a`, `b`, or `c`");
}

#[test]
fn the_serde_scan_sees_an_argument_after_a_nested_group() {
    let found = forbidden_serde_args(
        "pub enum RiskClass {\n    #[serde(rename(serialize = \"x\"), skip_deserializing)]\n    A,\n",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("`skip_deserializing`"), "{found:?}");

    for attribute in [
        "#[serde(skip)]",
        "#[ serde ( skip ) ]",
        "#[cfg_attr(test, serde(skip))]",
        "#[serde(rename = \"x\", other)]",
        "#[serde(rename = \"a)b\", skip_serializing)]",
    ] {
        assert_eq!(
            forbidden_serde_args(attribute).len(),
            1,
            "the scan must refuse {attribute}"
        );
    }
    for attribute in [
        "#[serde(rename = \"x\")]",
        "#[serde(alias = \"skip\")]",
        "#[serde(rename(serialize = \"x\", deserialize = \"y\"))]",
    ] {
        assert!(
            forbidden_serde_args(attribute).is_empty(),
            "the scan must accept {attribute}"
        );
    }
}

#[test]
fn a_mismatched_envelope_sentence_shows_the_current_text() {
    let (auto_merge, never_recommend) = expected_sentences();
    let raw = format!(
        "intro\n{AUTO_MERGE_OPENING} `documentation`, `logic`. More prose.\n\
         {NEVER_RECOMMEND_OPENING} `protocol`.\n"
    );
    let failures = surface_failures("x.md", &raw, &auto_merge, &never_recommend);
    let joined = failures.join("\n");
    assert!(
        joined.contains(&format!(
            "currently:\n    {AUTO_MERGE_OPENING} `documentation`, `logic`."
        )),
        "{joined}"
    );
    assert!(
        joined.contains(&format!(
            "currently:\n    {NEVER_RECOMMEND_OPENING} `protocol`."
        )),
        "{joined}"
    );
}
