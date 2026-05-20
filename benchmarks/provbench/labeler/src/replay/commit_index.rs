//! Per-commit symbol index built from tree-sitter ASTs.
//!
//! [`CommitSymbolIndex`] answers "does a fact's qualified symbol exist
//! anywhere in this commit's tree?" using only blobs from that commit —
//! never the working tree or HEAD.  It is built once per commit, before
//! the per-fact classification loop, so each blob is **read** at most once.
//!
//! # Blob-read budget
//! `build` accepts a map of already-read blobs (keyed by repo-relative
//! path) so callers can reuse reads that happened earlier in the same
//! commit iteration.  Paths absent from `cached_blobs` are fetched via
//! [`Pilot::read_blob_at`].

use crate::ast::RustAst;
use crate::facts::{field, function_signature, symbol_existence, test_assertion, Fact};
use crate::repo::Pilot;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Which kind of Python fact an entry represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonFactKind {
    FunctionSignature,
    Field,
    PublicSymbol,
    TestAssertion,
}

/// A single Python symbol entry in the commit index, carrying path information
/// so `lookup_python` can distinguish exact-path matches from cross-file ones.
#[derive(Debug, Clone)]
pub struct PythonEntry {
    pub fact_kind: PythonFactKind,
    #[allow(dead_code)] // read by Task 4's builder; not yet consumed
    pub qualified_name: String,
    pub container: String,
    pub leaf: String,
    pub path: PathBuf,
}

/// Result of a path-aware Python symbol lookup.
///
/// Callers MUST route both `UniqueFallbackAtPath` and `AmbiguousFallback`
/// to `Label::NeedsRevalidation` — without body-hash confirmation we cannot
/// claim `Stale_Symbol_Renamed` for cross-file movement.
#[derive(Debug)]
pub enum PythonLookup {
    /// Exact (fact_kind, container, leaf) match at the original path. No movement.
    ExactAtOriginalPath,
    /// Path-stripped fallback matched exactly one entry at a different path.
    UniqueFallbackAtPath(PathBuf),
    /// Fallback returned ≥2 candidates at different paths.
    AmbiguousFallback,
    /// Neither primary nor fallback found any match.
    Absent,
}

/// Per-commit, kind-partitioned set of qualified names present in the tree.
///
/// Only `.rs` blobs are indexed; markdown blobs are not parsed here because
/// `DocClaim` resolution is byte-range–based and does not benefit from a
/// tree-wide symbol index.
pub struct CommitSymbolIndex {
    function_names: HashSet<String>,
    field_names: HashSet<String>,
    symbol_names: HashSet<String>,
    test_names: HashSet<String>,
    /// Python-specific entries, keyed by (fact_kind, container, leaf, path).
    /// Populated by Task 4's builder; defaults to empty so existing Rust
    /// behavior is byte-stable.
    pub python_entries: Vec<PythonEntry>,
}

impl CommitSymbolIndex {
    /// Build the index for `commit_sha` over all `.rs` paths in `rs_paths`.
    ///
    /// `cached_blobs` is a map of blobs already read in the same commit
    /// iteration (keyed by repo-relative path).  Paths present in the map
    /// are used directly; absent paths are fetched via `pilot.read_blob_at`.
    /// Deleted paths (`None` in the map, or returning `None` from the pilot)
    /// are skipped.
    pub fn build(
        pilot: &Pilot,
        commit_sha: &str,
        rs_paths: &[PathBuf],
        cached_blobs: &HashMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<Self> {
        let mut function_names = HashSet::new();
        let mut field_names = HashSet::new();
        let mut symbol_names = HashSet::new();
        let mut test_names = HashSet::new();

        for path in rs_paths {
            // Reuse a cached blob if available; only call read_blob_at when
            // the path was not already fetched for this commit.
            // Borrow cached bytes in-place to avoid cloning when possible.
            let fetched: Option<Vec<u8>>;
            let bytes: &[u8] = match cached_blobs.get(path) {
                Some(Some(cached)) => cached,
                Some(None) => continue, // path was deleted at this commit
                None => {
                    fetched = pilot.read_blob_at(commit_sha, path)?;
                    match &fetched {
                        Some(b) => b,
                        None => continue,
                    }
                }
            };
            let Ok(ast) = RustAst::parse(bytes) else {
                continue;
            };
            for fact in function_signature::extract(&ast, path) {
                if let Fact::FunctionSignature { qualified_name, .. } = fact {
                    function_names.insert(qualified_name);
                }
            }
            for fact in field::extract(&ast, path) {
                if let Fact::Field { qualified_path, .. } = fact {
                    field_names.insert(qualified_path);
                }
            }
            for fact in symbol_existence::extract(&ast, path) {
                if let Fact::PublicSymbol { qualified_name, .. } = fact {
                    symbol_names.insert(qualified_name);
                }
            }
            // test_assertion::extract needs a prior-facts slice; pass empty
            // since we only need the test function names (not cross-refs).
            for fact in test_assertion::extract(&ast, path, &[]) {
                if let Fact::TestAssertion { test_fn, .. } = fact {
                    test_names.insert(test_fn);
                }
            }
        }

        Ok(Self {
            function_names,
            field_names,
            symbol_names,
            test_names,
            python_entries: Vec::new(),
        })
    }

    /// Returns `true` if a same-kind, same-qualified Rust symbol exists
    /// anywhere in this commit's `.rs` tree (including at the fact's
    /// original source path).
    ///
    /// The index is path-agnostic — it tracks only qualified names, not
    /// which file each name comes from.  To answer "does the symbol exist
    /// _elsewhere_ in the tree" (i.e., excluding the original path), the
    /// caller must first verify the symbol is absent from its original path
    /// via `matching_post_fact`.  This method should only be invoked after
    /// that check returns `None`; the caller's control flow provides the
    /// "elsewhere" guarantee that this path-agnostic index cannot.
    ///
    /// `DocClaim` always returns `false` — doc claims are byte-range–
    /// anchored and are not indexed here.
    pub fn symbol_exists_in_tree(&self, fact: &Fact) -> bool {
        match fact {
            Fact::FunctionSignature { qualified_name, .. } => {
                self.function_names.contains(qualified_name.as_str())
            }
            Fact::Field { qualified_path, .. } => {
                self.field_names.contains(qualified_path.as_str())
            }
            Fact::PublicSymbol { qualified_name, .. } => {
                self.symbol_names.contains(qualified_name.as_str())
            }
            Fact::TestAssertion { test_fn, .. } => self.test_names.contains(test_fn.as_str()),
            Fact::DocClaim { .. } => false,
        }
    }

    /// SPEC §11 row 2026-05-19 (Plan A.2): path-aware lookup for Python
    /// rename / move resolution. Returns `PythonLookup::ExactAtOriginalPath`
    /// if a matching entry exists at `original_path`; otherwise applies
    /// the path-stripped fallback (matching `(fact_kind, container, leaf)`
    /// at different paths) and returns Unique / Ambiguous / Absent.
    ///
    /// Callers MUST route both `UniqueFallbackAtPath` and `AmbiguousFallback`
    /// to `Label::NeedsRevalidation` — without body-hash confirmation we
    /// don't claim `Stale_Symbol_Renamed` for cross-file movement.
    pub fn lookup_python(
        &self,
        fact_kind: PythonFactKind,
        container: &str,
        leaf: &str,
        original_path: &Path,
    ) -> PythonLookup {
        // Primary: exact at original path.
        for entry in &self.python_entries {
            if entry.fact_kind == fact_kind
                && entry.path == original_path
                && entry.container == container
                && entry.leaf == leaf
            {
                return PythonLookup::ExactAtOriginalPath;
            }
        }
        // Fallback: same (fact_kind, container, leaf) at any path.
        let mut matches: Vec<&PathBuf> = self
            .python_entries
            .iter()
            .filter(|e| e.fact_kind == fact_kind && e.container == container && e.leaf == leaf)
            .map(|e| &e.path)
            .collect();
        matches.sort();
        matches.dedup();
        match matches.len() {
            0 => PythonLookup::Absent,
            1 => PythonLookup::UniqueFallbackAtPath(matches[0].clone()),
            _ => PythonLookup::AmbiguousFallback,
        }
    }
}

#[cfg(test)]
mod python_lookup_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn python_lookup_exact_at_original_path() {
        let idx = CommitSymbolIndex {
            function_names: HashSet::new(),
            field_names: HashSet::new(),
            symbol_names: HashSet::new(),
            test_names: HashSet::new(),
            python_entries: vec![PythonEntry {
                fact_kind: PythonFactKind::FunctionSignature,
                qualified_name: "src.a.foo".into(),
                container: "".into(),
                leaf: "foo".into(),
                path: Path::new("src/a.py").to_path_buf(),
            }],
        };
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::ExactAtOriginalPath));
    }

    #[test]
    fn python_lookup_unique_fallback_at_path() {
        let idx = CommitSymbolIndex {
            function_names: HashSet::new(),
            field_names: HashSet::new(),
            symbol_names: HashSet::new(),
            test_names: HashSet::new(),
            python_entries: vec![PythonEntry {
                fact_kind: PythonFactKind::FunctionSignature,
                qualified_name: "src.b.foo".into(),
                container: "".into(),
                leaf: "foo".into(),
                path: Path::new("src/b.py").to_path_buf(),
            }],
        };
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::UniqueFallbackAtPath(_)));
    }

    #[test]
    fn python_lookup_ambiguous_fallback() {
        let idx = CommitSymbolIndex {
            function_names: HashSet::new(),
            field_names: HashSet::new(),
            symbol_names: HashSet::new(),
            test_names: HashSet::new(),
            python_entries: vec![
                PythonEntry {
                    fact_kind: PythonFactKind::FunctionSignature,
                    qualified_name: "src.a.foo".into(),
                    container: "".into(),
                    leaf: "foo".into(),
                    path: Path::new("src/a.py").to_path_buf(),
                },
                PythonEntry {
                    fact_kind: PythonFactKind::FunctionSignature,
                    qualified_name: "src.b.foo".into(),
                    container: "".into(),
                    leaf: "foo".into(),
                    path: Path::new("src/b.py").to_path_buf(),
                },
            ],
        };
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "",
            "foo",
            Path::new("src/c.py"),
        );
        assert!(matches!(result, PythonLookup::AmbiguousFallback));
    }

    #[test]
    fn python_lookup_absent() {
        let idx = CommitSymbolIndex {
            function_names: HashSet::new(),
            field_names: HashSet::new(),
            symbol_names: HashSet::new(),
            test_names: HashSet::new(),
            python_entries: Vec::new(),
        };
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::Absent));
    }
}
