//! Human-readable rendering of a [`super::ContextPack`].

use std::fmt::Write;

use super::{AreaStatus, ContextPack};

/// Disclaimer shown above fresh code-map summaries — they are navigation
/// pointers, not authoritative facts.
const POINTER_DISCLAIMER: &str =
    "(code-map summaries are pointers to where things live, not authoritative facts — verify in source)";

/// Render a context pack as plain text.
pub fn render_text(pack: &ContextPack) -> String {
    let mut out = String::new();
    // Writes to a `String` are infallible, so the `write!` results are unwrapped.
    let _ = writeln!(out, "Context pack for task: {}", pack.task);
    let _ = writeln!(out, "repo: {}", pack.repo);
    let _ = write!(out, "budget: ~{} tokens", pack.budget_tokens);
    if pack.truncated {
        out.push_str(" (memory hits truncated to fit)");
    }
    out.push('\n');

    out.push_str("\n## Known decisions\n");
    if pack.decisions.is_empty() {
        out.push_str("  (none recorded for the requested areas)\n");
    } else {
        let mut decisions: Vec<_> = pack.decisions.iter().collect();
        decisions.sort_by(|a, b| {
            (&a.subject, &a.predicate, &a.object).cmp(&(&b.subject, &b.predicate, &b.object))
        });
        for d in decisions {
            let _ = writeln!(out, "  - {} {} {}", d.subject, d.predicate, d.object);
        }
    }

    out.push_str("\n## Relevant memory\n");
    if pack.memory_hits.is_empty() {
        out.push_str("  (no matching memory)\n");
    } else {
        // Recall relevance selects the hits before rendering. Canonicalizing
        // their display order here keeps this early prompt prefix byte-stable
        // without changing which memories the search pipeline selected.
        let mut memory_hits: Vec<_> = pack.memory_hits.iter().collect();
        memory_hits.sort_by(|a, b| a.id.cmp(&b.id));
        for h in memory_hits {
            let _ = writeln!(
                out,
                "  - [{}/{}] {} (score {:.3})",
                h.wing, h.room, h.snippet, h.score
            );
        }
    }

    out.push_str("\n## Code maps\n");
    if pack.areas.is_empty() {
        out.push_str("  (no areas requested)\n");
    } else {
        let _ = writeln!(out, "  {POINTER_DISCLAIMER}");
        for a in &pack.areas {
            match &a.status {
                AreaStatus::Fresh {
                    head_sha,
                    source_file_count,
                    summary,
                } => {
                    let _ = writeln!(
                        out,
                        "  - {} [FRESH @ {} · {} files]\n      {}",
                        a.area, head_sha, source_file_count, summary
                    );
                }
                AreaStatus::Stale {
                    head_sha,
                    changed_files,
                    refresh_recommendation,
                } => {
                    let _ = writeln!(
                        out,
                        "  - {} [STALE @ {}] {}\n      changed: {}",
                        a.area,
                        head_sha,
                        refresh_recommendation,
                        changed_files.join(", ")
                    );
                }
                AreaStatus::Missing { reason } => {
                    let _ = writeln!(out, "  - {} [SCOUT REQUIRED] {}", a.area, reason);
                }
            }
        }
    }

    // Only emit the Warnings section when there are degradations to report.
    if !pack.warnings.is_empty() {
        out.push_str("\n## Warnings\n");
        for w in &pack.warnings {
            let _ = writeln!(out, "  - {w}");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{AreaContext, ContextPack};

    fn base_pack(areas: Vec<AreaContext>) -> ContextPack {
        ContextPack {
            task: "touch collab".to_string(),
            repo: "/repo".to_string(),
            budget_tokens: 2000,
            memory_hits: Vec::new(),
            decisions: Vec::new(),
            areas,
            truncated: false,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn fresh_area_renders_pointer_disclaimer_and_fresh_marker() {
        let pack = base_pack(vec![AreaContext {
            area: "core".to_string(),
            status: AreaStatus::Fresh {
                head_sha: "abc1234".to_string(),
                source_file_count: 3,
                summary: "collab handoff lives in state_machine.rs".to_string(),
            },
        }]);

        let text = render_text(&pack);
        assert!(text.contains(POINTER_DISCLAIMER));
        assert!(text.contains("FRESH"));
        assert!(text.contains("collab handoff lives in state_machine.rs"));
    }

    #[test]
    fn stale_area_renders_changed_files_and_refresh_recommendation() {
        let pack = base_pack(vec![AreaContext {
            area: "collab".to_string(),
            status: AreaStatus::Stale {
                head_sha: "def5678".to_string(),
                changed_files: vec!["a.rs".to_string(), "b.rs".to_string()],
                refresh_recommendation: "re-scout this area before trusting it".to_string(),
            },
        }]);

        let text = render_text(&pack);
        assert!(text.contains("STALE"));
        assert!(text.contains("re-scout this area before trusting it"));
        assert!(text.contains("a.rs"));
        assert!(text.contains("b.rs"));
    }

    #[test]
    fn missing_area_renders_scout_required_section() {
        let pack = base_pack(vec![AreaContext {
            area: "collab".to_string(),
            status: AreaStatus::Missing {
                reason: "no code map for area 'collab'; scout required".to_string(),
            },
        }]);

        let text = render_text(&pack);
        assert!(text.contains("SCOUT REQUIRED"));
        assert!(text.contains("collab"));
    }

    #[test]
    fn empty_sections_render_explicit_empty_state_lines() {
        let pack = base_pack(Vec::new());
        let text = render_text(&pack);
        assert!(text.contains("(none recorded for the requested areas)"));
        assert!(text.contains("(no matching memory)"));
        assert!(text.contains("(no areas requested)"));
    }

    #[test]
    fn truncated_flag_is_surfaced_in_budget_line() {
        let mut pack = base_pack(Vec::new());
        pack.truncated = true;
        let text = render_text(&pack);
        assert!(text.contains("budget: ~2000 tokens (memory hits truncated to fit)"));
    }

    #[test]
    fn warnings_section_renders_only_when_present() {
        // No warnings → no Warnings section header.
        let clean = base_pack(Vec::new());
        assert!(!render_text(&clean).contains("## Warnings"));

        // With warnings → header plus each warning listed.
        let mut pack = base_pack(Vec::new());
        pack.warnings = vec![
            "memory recall failed: boom".to_string(),
            "repo path '/x' could not be canonicalized".to_string(),
        ];
        let text = render_text(&pack);
        assert!(text.contains("## Warnings"));
        assert!(text.contains("  - memory recall failed: boom"));
        assert!(text.contains("  - repo path '/x' could not be canonicalized"));
    }

    #[test]
    fn rendering_canonicalizes_unordered_memory_and_decision_hits() {
        let mut first = base_pack(vec![
            AreaContext {
                area: "zeta".to_string(),
                status: AreaStatus::Missing {
                    reason: "no map".to_string(),
                },
            },
            AreaContext {
                area: "alpha".to_string(),
                status: AreaStatus::Missing {
                    reason: "no map".to_string(),
                },
            },
        ]);
        first.memory_hits = vec![
            super::super::MemoryHit {
                id: "drawer-z".to_string(),
                wing: "w".to_string(),
                room: "r".to_string(),
                score: 0.1,
                snippet: "last".to_string(),
            },
            super::super::MemoryHit {
                id: "drawer-a".to_string(),
                wing: "w".to_string(),
                room: "r".to_string(),
                score: 0.9,
                snippet: "first".to_string(),
            },
        ];
        first.decisions = vec![
            super::super::DecisionHit {
                subject: "z".to_string(),
                predicate: "uses".to_string(),
                object: "z".to_string(),
            },
            super::super::DecisionHit {
                subject: "a".to_string(),
                predicate: "uses".to_string(),
                object: "a".to_string(),
            },
        ];
        let mut second = first.clone();
        second.memory_hits.reverse();
        second.decisions.reverse();

        let rendered = render_text(&first);
        assert_eq!(rendered, render_text(&second));
        assert!(rendered.find("first").unwrap() < rendered.find("last").unwrap());
        assert!(rendered.find("a uses a").unwrap() < rendered.find("z uses z").unwrap());
    }
}
