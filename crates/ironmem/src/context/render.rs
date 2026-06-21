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
        for d in &pack.decisions {
            let _ = writeln!(out, "  - {} {} {}", d.subject, d.predicate, d.object);
        }
    }

    out.push_str("\n## Relevant memory\n");
    if pack.memory_hits.is_empty() {
        out.push_str("  (no matching memory)\n");
    } else {
        for h in &pack.memory_hits {
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
}
