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
