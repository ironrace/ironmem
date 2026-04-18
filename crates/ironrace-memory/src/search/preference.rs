//! Index-time preference enrichment — port of mempalace hybrid-v4 pattern set.
//!
//! Scans conversational content for first-person disclosures using 20 regexes
//! (16 from mempalace v3, 4 nostalgia/memory patterns added in v4).  When any
//! pattern fires, a `[preferences: ...]` annotation is appended to the content
//! string before it is embedded and FTS-indexed.
//!
//! This bridges the vocabulary gap in `single-session-preference` questions:
//! a query like "recommend video editing resources" now has lexical overlap with
//! a session that contains "I enjoy Adobe Premiere Pro" because the annotation
//! contains the extracted phrase "enjoy adobe premiere pro".
//!
//! Activation: off by default; enable with `IRONMEM_PREF_ENRICH=1`.

use std::sync::LazyLock;

use regex::Regex;

use super::tunables;

// ── 20 first-person preference patterns ──────────────────────────────────────

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // v3: experience / ongoing state (16 patterns)
        r"i(?:'ve been| have been) having (?:trouble|issues?|problems?) with ([^,\.!?\n]{5,80})",
        r"i(?:'ve been| have been) feeling ([^,\.!?\n]{5,60})",
        r"i(?:'ve been| have been) (?:struggling|dealing) with ([^,\.!?\n]{5,80})",
        r"i(?:'ve been| have been) (?:worried|concerned) about ([^,\.!?\n]{5,80})",
        r"i(?:'m| am) (?:worried|concerned) about ([^,\.!?\n]{5,80})",
        r"i prefer ([^,\.!?\n]{5,60})",
        r"i usually ([^,\.!?\n]{5,60})",
        r"i(?:'ve been| have been) (?:trying|attempting) to ([^,\.!?\n]{5,80})",
        r"i(?:'ve been| have been) (?:considering|thinking about) ([^,\.!?\n]{5,80})",
        r"lately[,\s]+(?:i've been|i have been|i'm|i am) ([^,\.!?\n]{5,80})",
        r"recently[,\s]+(?:i've been|i have been|i'm|i am) ([^,\.!?\n]{5,80})",
        r"i(?:'ve been| have been) (?:working on|focused on|interested in) ([^,\.!?\n]{5,80})",
        r"i want to ([^,\.!?\n]{5,60})",
        r"i(?:'m| am) looking (?:to|for) ([^,\.!?\n]{5,60})",
        r"i(?:'m| am) thinking (?:about|of) ([^,\.!?\n]{5,60})",
        r"i(?:'ve been| have been) (?:noticing|experiencing) ([^,\.!?\n]{5,80})",
        // v4: nostalgia / memory (4 patterns)
        r"i (?:still )?remember (?:the |my )?([^,\.!?\n]{5,80})",
        r"i used to ([^,\.!?\n]{5,60})",
        r"when i was (?:in high school|in college|young|a kid|growing up)[,\s]+([^,\.!?\n]{5,80})",
        r"growing up[,\s]+([^,\.!?\n]{5,80})",
        // v5: direct preference declarations (the patterns that actually fire in LongMemEval)
        r"i enjoy ([^,\.!?\n]{5,60})",
        r"i like ([^,\.!?\n]{5,60})",
        r"i love ([^,\.!?\n]{5,60})",
        r"i use ([^,\.!?\n]{5,60})",
        r"i(?:'ve been| have been) using ([^,\.!?\n]{5,60})",
        r"i(?:'m| am) (?:really )?into ([^,\.!?\n]{5,60})",
        r"i(?:'m| am) a fan of ([^,\.!?\n]{5,60})",
        r"i(?:'m| am) (?:really )?passionate about ([^,\.!?\n]{5,60})",
        r"my (?:favorite|favourite) (?:is|are|has been|was) ([^,\.!?\n]{5,60})",
        r"i(?:'ve| have) always (?:loved|enjoyed|liked) ([^,\.!?\n]{5,60})",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("preference pattern is valid regex"))
    .collect()
});

const MAX_PREFS: usize = 12;

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract first-person preference/experience phrases from content (lowercased internally).
/// Returns deduplicated phrases in first-match order, capped at `MAX_PREFS`.
pub fn extract_preferences(content: &str) -> Vec<String> {
    let lower = content.to_lowercase();
    let mut seen: Vec<String> = Vec::new();

    'outer: for pattern in PATTERNS.iter() {
        for cap in pattern.captures_iter(&lower) {
            if let Some(m) = cap.get(1) {
                let pref = m.as_str().trim().to_string();
                if !pref.is_empty() && !seen.contains(&pref) {
                    seen.push(pref);
                    if seen.len() >= MAX_PREFS {
                        break 'outer;
                    }
                }
            }
        }
    }

    seen
}

/// Append a `[preferences: ...]` annotation to content if any phrases are detected.
///
/// Returns the original content unchanged when:
/// - `IRONMEM_PREF_ENRICH=0` (disabled at runtime)
/// - No preference patterns match
pub fn enrich_content(content: &str) -> String {
    if !tunables::pref_enrich_enabled() {
        return content.to_string();
    }
    let prefs = extract_preferences(content);
    if prefs.is_empty() {
        return content.to_string();
    }
    format!("{}\n[preferences: {}]", content, prefs.join("; "))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_prefer_pattern() {
        let prefs = extract_preferences("I prefer Adobe Premiere Pro for video editing.");
        assert!(prefs.iter().any(|p| p.contains("adobe premiere pro")));
    }

    #[test]
    fn detects_ive_been_interested_in() {
        let prefs = extract_preferences("I've been interested in learning Rust lately.");
        assert!(!prefs.is_empty());
        assert!(prefs.iter().any(|p| p.contains("learning rust")));
    }

    #[test]
    fn detects_i_used_to() {
        let prefs = extract_preferences("I used to play guitar when I was in college.");
        assert!(prefs.iter().any(|p| p.contains("play guitar")));
    }

    #[test]
    fn detects_growing_up_pattern() {
        let prefs = extract_preferences("Growing up, I spent a lot of time outdoors.");
        assert!(prefs
            .iter()
            .any(|p| p.contains("spent a lot of time outdoors")));
    }

    #[test]
    fn deduplicates_repeated_phrases() {
        let content = "I prefer coffee. I prefer coffee. I prefer coffee. I want to learn piano.";
        let prefs = extract_preferences(content);
        let coffee_count = prefs.iter().filter(|p| p.contains("coffee")).count();
        assert_eq!(coffee_count, 1);
    }

    #[test]
    fn caps_at_max_prefs() {
        // 13 distinct short preference statements
        let content = (1..=13)
            .map(|i| format!("I prefer option{i:02}."))
            .collect::<Vec<_>>()
            .join(" ");
        let prefs = extract_preferences(&content);
        assert!(prefs.len() <= 12);
    }

    #[test]
    fn enrich_content_appends_annotation() {
        // Test extraction + annotation format directly (enrich_content is gated by tunable).
        let content = "I prefer dark mode in my editor.";
        let prefs = extract_preferences(content);
        assert!(!prefs.is_empty());
        let annotation = format!("{}\n[preferences: {}]", content, prefs.join("; "));
        assert!(annotation.contains("[preferences:"));
        assert!(annotation.contains("dark mode"));
    }

    #[test]
    fn detects_v5_direct_preference_patterns() {
        let cases = [
            (
                "I enjoy Adobe Premiere Pro for video editing.",
                "adobe premiere pro",
            ),
            (
                "I like Python over JavaScript for backend work.",
                "python over javascript",
            ),
            ("I love cooking Italian food.", "cooking italian food"),
            (
                "I use Final Cut Pro for my YouTube videos.",
                "final cut pro",
            ),
            (
                "I've been using Figma for all my design work.",
                "figma for all my design work",
            ),
            (
                "I'm really into video editing and cinematography.",
                "video editing and cinematography",
            ),
            ("I'm a fan of open-source software.", "open-source software"),
            ("My favorite is dark roast coffee.", "dark roast coffee"),
        ];
        for (content, expected_substr) in cases {
            let prefs = extract_preferences(content);
            assert!(
                prefs.iter().any(|p| p.contains(expected_substr)),
                "pattern did not match '{content}' — got: {prefs:?}"
            );
        }
    }

    #[test]
    fn enrich_content_noop_when_no_match() {
        let content = "The weather is nice today.";
        let enriched = enrich_content(content);
        assert_eq!(enriched, content);
    }
}
