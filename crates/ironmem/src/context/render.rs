//! Human-readable rendering of a [`super::ContextPack`].

use super::ContextPack;

/// Render a context pack as plain text. Fleshed out in a later task; for now a
/// minimal header keeps the non-JSON path working.
pub fn render_text(pack: &ContextPack) -> String {
    format!(
        "Context pack for task: {}\nrepo: {}\nbudget: ~{} tokens\n",
        pack.task, pack.repo, pack.budget_tokens
    )
}
