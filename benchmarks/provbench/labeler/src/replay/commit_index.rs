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

/// Per-commit, kind-partitioned symbol index present in the tree.
///
/// Rust entries retain the historical path-agnostic qualified-name sets.
/// Python entries carry `(qualified_name, container, leaf, path)` so
/// cross-file moves can be routed to `NeedsRevalidation` without guessing.
/// Markdown blobs are not parsed here because `DocClaim` resolution is
/// byte-range–based and does not benefit from a tree-wide symbol index.
pub struct CommitSymbolIndex {
    function_names: HashSet<String>,
    field_names: HashSet<String>,
    symbol_names: HashSet<String>,
    test_names: HashSet<String>,
    /// Python-specific entries, keyed by (fact_kind, container, leaf, path).
    /// Populated by Task 4's builder; defaults to empty so existing Rust
    /// behavior is byte-stable.
    python_entries: Vec<PythonEntry>,
}

impl CommitSymbolIndex {
    /// Build the index for `commit_sha` over all Rust and Python source paths.
    ///
    /// `cached_blobs` is a map of blobs already read in the same commit
    /// iteration (keyed by repo-relative path).  Paths present in the map
    /// are used directly; absent paths are fetched via `pilot.read_blob_at`.
    /// Deleted paths (`None` in the map, or returning `None` from the pilot)
    /// are skipped.
    pub fn build(
        pilot: &Pilot,
        commit_sha: &str,
        rust_paths: &[PathBuf],
        python_paths: &[PathBuf],
        cached_blobs: &HashMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<Self> {
        let mut function_names = HashSet::new();
        let mut field_names = HashSet::new();
        let mut symbol_names = HashSet::new();
        let mut test_names = HashSet::new();

        for path in rust_paths {
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

        let mut python_entries: Vec<PythonEntry> = Vec::new();

        for path in python_paths {
            let fetched: Option<Vec<u8>>;
            let bytes: &[u8] = match cached_blobs.get(path) {
                Some(Some(cached)) => cached,
                Some(None) => continue,
                None => {
                    fetched = pilot.read_blob_at(commit_sha, path)?;
                    match &fetched {
                        Some(b) => b,
                        None => continue,
                    }
                }
            };
            let Ok(py_ast) = crate::ast::python::PythonAst::parse(bytes) else {
                continue;
            };
            for fact in crate::facts::python::function_signature::extract(&py_ast, path) {
                if let Fact::FunctionSignature { qualified_name, .. } = fact {
                    let (container, leaf) = split_python_qualified_name(&qualified_name, path);
                    python_entries.push(PythonEntry {
                        fact_kind: PythonFactKind::FunctionSignature,
                        qualified_name,
                        container,
                        leaf,
                        path: path.clone(),
                    });
                }
            }
            for fact in crate::facts::python::field::extract(&py_ast, path) {
                if let Fact::Field { qualified_path, .. } = fact {
                    let (container, leaf) = split_python_qualified_name(&qualified_path, path);
                    python_entries.push(PythonEntry {
                        fact_kind: PythonFactKind::Field,
                        qualified_name: qualified_path,
                        container,
                        leaf,
                        path: path.clone(),
                    });
                }
            }
            for fact in crate::facts::python::symbol_existence::extract(&py_ast, path) {
                if let Fact::PublicSymbol { qualified_name, .. } = fact {
                    let (container, leaf) = split_python_qualified_name(&qualified_name, path);
                    // SPEC §11 row 2026-05-19: single-underscore filter for PublicSymbol.
                    if leaf.starts_with('_') {
                        continue;
                    }
                    python_entries.push(PythonEntry {
                        fact_kind: PythonFactKind::PublicSymbol,
                        qualified_name,
                        container,
                        leaf,
                        path: path.clone(),
                    });
                }
            }
            for fact in crate::facts::python::test_assertion::extract(&py_ast, path) {
                if let Fact::TestAssertion { test_fn, .. } = fact {
                    python_entries.push(PythonEntry {
                        fact_kind: PythonFactKind::TestAssertion,
                        qualified_name: test_fn.clone(),
                        container: String::new(),
                        leaf: test_fn,
                        path: path.clone(),
                    });
                }
            }
        }

        Ok(Self {
            function_names,
            field_names,
            symbol_names,
            test_names,
            python_entries,
        })
    }

    /// Test-only constructor: build a `CommitSymbolIndex` with empty Rust
    /// symbol sets and the provided `python_entries`. Avoids exposing the
    /// private `python_entries` field in test code.
    #[cfg(test)]
    pub fn with_python_entries_for_test(entries: Vec<PythonEntry>) -> Self {
        Self {
            function_names: HashSet::new(),
            field_names: HashSet::new(),
            symbol_names: HashSet::new(),
            test_names: HashSet::new(),
            python_entries: entries,
        }
    }

    /// Test-only iterator over `python_entries`. Avoids exposing the field
    /// as `pub` while still letting integration tests in this module inspect
    /// what `build` populated.
    #[cfg(test)]
    pub fn iter_python_entries(&self) -> impl Iterator<Item = &PythonEntry> {
        self.python_entries.iter()
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
    /// rename / move resolution. First tries an exact qualified-name match;
    /// only when that misses does it apply the path-stripped fallback
    /// matching `(fact_kind, container, leaf)` at any path.
    ///
    /// Callers MUST route both `UniqueFallbackAtPath` and `AmbiguousFallback`
    /// to `Label::NeedsRevalidation` — without body-hash confirmation we
    /// don't claim `Stale_Symbol_Renamed` for cross-file movement.
    pub fn lookup_python(
        &self,
        fact_kind: PythonFactKind,
        qualified_name: &str,
        container: &str,
        leaf: &str,
        original_path: &Path,
    ) -> PythonLookup {
        // Primary: exact qualified-name match across all Python source paths.
        let mut exact_paths: Vec<&PathBuf> = self
            .python_entries
            .iter()
            .filter(|e| e.fact_kind == fact_kind && e.qualified_name == qualified_name)
            .map(|e| &e.path)
            .collect();
        exact_paths.sort();
        exact_paths.dedup();
        if exact_paths.iter().any(|p| p.as_path() == original_path) {
            return PythonLookup::ExactAtOriginalPath;
        }
        match exact_paths.len() {
            0 => {}
            1 => return PythonLookup::UniqueFallbackAtPath(exact_paths[0].clone()),
            _ => return PythonLookup::AmbiguousFallback,
        }

        // Fallback: same (fact_kind, container, leaf) at a DIFFERENT path.
        // Exclude `original_path` so that an in-file container rename
        // (e.g. `src.a.Foo.method` → `src.a.Bar.method`) does not produce
        // `UniqueFallbackAtPath(original_path)` — that would mislead the
        // caller into treating an in-file rename as a cross-file move.
        let mut matches: Vec<&PathBuf> = self
            .python_entries
            .iter()
            .filter(|e| {
                e.fact_kind == fact_kind
                    && e.container == container
                    && e.leaf == leaf
                    && e.path.as_path() != original_path
            })
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

/// Split a Python qualified name into (container, leaf), stripping the
/// module-path prefix derived from `path`. For `src.a.Greeter.greet` at
/// `src/a.py`, returns (`"Greeter"`, `"greet"`). For module-level
/// `src.a.foo` at `src/a.py`, returns (`""`, `"foo"`).
///
/// This is the canonical implementation shared by `commit_index` (index
/// population), `replay/mod.rs` (`python_fact_kind_container_leaf`), and
/// `diff/mod.rs` (`RenameCandidate::new_python`). All three must use
/// identical logic so lookup keys are byte-stable.
pub(crate) fn split_python_qualified_name(qualified_name: &str, path: &Path) -> (String, String) {
    let path_str = path.to_string_lossy();
    let module_path = path_str
        .strip_suffix(".py")
        .unwrap_or(&path_str)
        .replace('/', ".");
    let stripped = qualified_name
        .strip_prefix(&format!("{}.", module_path))
        .unwrap_or(qualified_name);
    if let Some(idx) = stripped.rfind('.') {
        (stripped[..idx].to_string(), stripped[idx + 1..].to_string())
    } else {
        (String::new(), stripped.to_string())
    }
}

#[cfg(test)]
mod python_lookup_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn python_lookup_exact_at_original_path() {
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![PythonEntry {
            fact_kind: PythonFactKind::FunctionSignature,
            qualified_name: "src.a.foo".into(),
            container: "".into(),
            leaf: "foo".into(),
            path: Path::new("src/a.py").to_path_buf(),
        }]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.a.foo",
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::ExactAtOriginalPath));
    }

    #[test]
    fn python_lookup_unique_fallback_at_path() {
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![PythonEntry {
            fact_kind: PythonFactKind::FunctionSignature,
            qualified_name: "src.b.foo".into(),
            container: "".into(),
            leaf: "foo".into(),
            path: Path::new("src/b.py").to_path_buf(),
        }]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.a.foo",
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::UniqueFallbackAtPath(_)));
    }

    #[test]
    fn python_lookup_ambiguous_fallback() {
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![
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
        ]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.c.foo",
            "",
            "foo",
            Path::new("src/c.py"),
        );
        assert!(matches!(result, PythonLookup::AmbiguousFallback));
    }

    #[test]
    fn python_lookup_absent() {
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.a.foo",
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(matches!(result, PythonLookup::Absent));
    }

    #[test]
    fn python_lookup_primary_qualified_name_precedes_fallback_collision() {
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![
            PythonEntry {
                fact_kind: PythonFactKind::FunctionSignature,
                qualified_name: "src.a.foo".into(),
                container: "".into(),
                leaf: "foo".into(),
                path: Path::new("src/b.py").to_path_buf(),
            },
            PythonEntry {
                fact_kind: PythonFactKind::FunctionSignature,
                qualified_name: "src.c.foo".into(),
                container: "".into(),
                leaf: "foo".into(),
                path: Path::new("src/c.py").to_path_buf(),
            },
        ]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.a.foo",
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(
            matches!(result, PythonLookup::UniqueFallbackAtPath(ref p) if p == Path::new("src/b.py")),
            "primary exact qualified-name match should win before path-stripped fallback; got {:?}",
            result
        );
    }

    /// Fix 4: in-file container rename must resolve to Absent, not
    /// UniqueFallbackAtPath(original_path).
    ///
    /// Before the fix, the fallback matched `(kind, "Foo", "method")` at
    /// `src/a.py` — the original path — and returned
    /// `UniqueFallbackAtPath(PathBuf::from("src/a.py"))`. The caller would
    /// route that to NeedsRevalidation with misleading "cross-file move"
    /// semantics. After the fix, original_path is excluded from the fallback
    /// match set so the result is `Absent`, correctly falling through to
    /// within-file rename detection.
    #[test]
    fn python_lookup_in_file_container_rename_routes_absent() {
        // Index has `src.a.Bar.method` at src/a.py (post-rename state).
        // Fact asks for `src.a.Foo.method` at src/a.py (pre-rename).
        // Primary: no entry with qualified_name "src.a.Foo.method" → miss.
        // Fallback: (FunctionSignature, "Foo", "method") — no match because
        //   the only entry has container "Bar". Result: Absent.
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![PythonEntry {
            fact_kind: PythonFactKind::FunctionSignature,
            qualified_name: "src.a.Bar.method".into(),
            container: "Bar".into(),
            leaf: "method".into(),
            path: Path::new("src/a.py").to_path_buf(),
        }]);
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.a.Foo.method",
            "Foo",
            "method",
            Path::new("src/a.py"),
        );
        assert!(
            matches!(result, PythonLookup::Absent),
            "in-file container rename should resolve to Absent; got {:?}",
            result
        );
    }

    /// Fix 4 variant: same leaf at same file must not produce
    /// UniqueFallbackAtPath(original_path) when qualified_name differs.
    #[test]
    fn python_lookup_same_file_fallback_excluded() {
        // Index has `src.a.helper` at src/a.py (module-level).
        // Fact was `src.a.util` at src/a.py — same leaf "helper" is not
        // there; leaf is different too. But if leaf matched (e.g. same leaf,
        // different container at the SAME path), it must return Absent not
        // UniqueFallbackAtPath(original_path).
        let idx = CommitSymbolIndex::with_python_entries_for_test(vec![PythonEntry {
            fact_kind: PythonFactKind::FunctionSignature,
            qualified_name: "src.a.foo".into(),
            container: "".into(),
            leaf: "foo".into(),
            path: Path::new("src/a.py").to_path_buf(),
        }]);
        // Ask for ("", "foo") at src/a.py — primary misses (qualified_name
        // is "src.b.foo" which is not in the index). Fallback matches
        // (kind, "", "foo") but only at src/a.py which is original_path →
        // excluded → Absent.
        let result = idx.lookup_python(
            PythonFactKind::FunctionSignature,
            "src.b.foo",
            "",
            "foo",
            Path::new("src/a.py"),
        );
        assert!(
            matches!(result, PythonLookup::Absent),
            "fallback at original_path must be excluded; got {:?}",
            result
        );
    }

    // ── build() integration tests (Option B: tempdir + git init) ─────────────

    fn git_cmd(repo: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap_or_else(|e| panic!("git {:?} failed to spawn: {e}", args));
        assert!(status.success(), "git {:?} exited non-zero", args);
    }

    fn make_pilot_with_py(py_path: &str, py_source: &[u8]) -> (tempfile::TempDir, Pilot, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_cmd(root, &["init", "--initial-branch=main"]);
        // Minimal Cargo.toml so `git ls-tree` sees something in the tree.
        std::fs::write(root.join("placeholder.txt"), b"placeholder\n").unwrap();
        // Write the python file, creating parent dirs as needed.
        let full_path = root.join(py_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, py_source).unwrap();
        git_cmd(root, &["add", "."]);
        git_cmd(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init",
            ],
        );
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        struct AdHoc {
            path: PathBuf,
            sha: String,
        }
        impl crate::repo::PilotRepoSpec for AdHoc {
            fn local_clone_path(&self) -> &Path {
                &self.path
            }
            fn t0_sha(&self) -> &str {
                &self.sha
            }
        }
        let pilot = Pilot::open(&AdHoc {
            path: root.to_path_buf(),
            sha: sha.clone(),
        })
        .expect("Pilot::open");
        (tmp, pilot, sha)
    }

    #[test]
    fn build_populates_python_entries_from_python_paths() {
        let py_source = b"def foo():\n    return 1\n";
        let py_path = "src/a.py";
        let (_tmp, pilot, sha) = make_pilot_with_py(py_path, py_source);

        let path = PathBuf::from(py_path);
        let cached: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();

        let idx = CommitSymbolIndex::build(
            &pilot,
            &sha,
            &[],                         // rust_paths empty
            std::slice::from_ref(&path), // python_paths
            &cached,
        )
        .expect("build");

        let py_foos: Vec<_> = idx
            .iter_python_entries()
            .filter(|e| e.fact_kind == PythonFactKind::FunctionSignature && e.leaf == "foo")
            .collect();
        assert_eq!(
            py_foos.len(),
            1,
            "expected exactly one foo entry; got {:?}",
            py_foos
        );
        assert_eq!(py_foos[0].path, path);
    }

    #[test]
    fn build_filters_underscore_prefixed_public_symbols() {
        let py_source = b"def foo():\n    return 1\n\ndef _internal_foo():\n    return 2\n";
        let py_path = "src/a.py";
        let (_tmp, pilot, sha) = make_pilot_with_py(py_path, py_source);

        let path = PathBuf::from(py_path);
        let cached: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();

        let idx = CommitSymbolIndex::build(&pilot, &sha, &[], &[path], &cached).expect("build");

        let underscored: Vec<_> = idx
            .iter_python_entries()
            .filter(|e| e.fact_kind == PythonFactKind::PublicSymbol && e.leaf.starts_with('_'))
            .collect();
        assert!(
            underscored.is_empty(),
            "underscore-prefixed public symbols must be filtered; found {:?}",
            underscored
        );
    }
}
