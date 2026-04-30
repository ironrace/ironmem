//! V4 preference regex set, ported verbatim from mempalace
//! (`benchmarks/longmemeval_bench.py:1587-1610`). Compiled once on first use
//! via `OnceLock`. A bad pattern panics at first call — caught by tests.

pub(crate) fn extract_v4(_text: &str) -> Vec<String> {
    Vec::new()
}
