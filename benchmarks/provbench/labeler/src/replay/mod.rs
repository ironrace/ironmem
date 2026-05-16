//! Per-commit replay: read blobs at each commit, compute post-commit state
//! per fact, classify, emit `FactAtCommit` rows.
//!
//! [`Replay::run`] is the production entry point: it extracts the T₀ fact
//! set across every `.rs` and eligible `.md` file in the pilot tree,
//! walks the first-parent commit chain, and applies the SPEC §5 rule
//! engine ([`crate::label::classify`]) to each (fact, commit) pair.
//! Output is unsorted; deterministic ordering is the writer's
//! responsibility (see [`crate::output::write_jsonl`]).
//!
//! ## Symbol resolution model (Task 3 / Cluster B)
//! Classification is commit-tree-local.  For each commit a
//! [`commit_index::CommitSymbolIndex`] is built from that commit's blobs
//! before any fact is classified; `rust-analyzer` is **not** consulted
//! at replay time.  Live RA tooling stays in the crate for
//! `tests/replay_ra.rs` (pinned-binary test) and for future
//! cross-crate / macro-expanded work.

pub mod commit_index;
mod match_post;

use crate::ast::{spans::Span, RustAst};
use crate::diff::{is_whitespace_or_comment_only, rename_candidate_typed, RenameOrigin};
use crate::facts::{doc_claim, field, function_signature, symbol_existence, test_assertion, Fact};
use crate::label::{classify, Label, PostCommitState};
use crate::repo::{normalize_path_for_fact_id, CommitRef, Pilot, PilotRepoSpec};
use crate::resolve::SymbolResolver;
use anyhow::{Context, Result};
use commit_index::CommitSymbolIndex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

// ── Public surface ────────────────────────────────────────────────────────────

/// One row of the SPEC §3 `fact_at_commit` corpus: a fact ID paired with
/// the label it carries at a specific first-parent descendant of T₀.
///
/// Stamped with the labeler git SHA at write time by
/// [`crate::output::write_jsonl`]; the in-memory form here is unstamped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactAtCommit {
    /// `"{kind}::{qualified}::{source}::{line_start}"`
    pub fact_id: String,
    pub commit_sha: String,
    pub label: Label,
}

/// Inputs to [`Replay::run`].
pub struct ReplayConfig {
    pub repo_path: PathBuf,
    pub t0_sha: String,
    /// When `true`, the per-commit symbol index is not built and cross-file
    /// symbol lookup is skipped entirely.  Symbol resolution is then
    /// approximated as "the fact's qualified name still appears at its
    /// original source path".  Used by unit tests; production runs must set
    /// this `false`.
    pub skip_symbol_resolution: bool,
}

/// Stateless entry point — see [`Replay::run`].
pub struct Replay;

struct ObservedFact {
    fact: Fact,
    t0_span_bytes: Vec<u8>,
    /// Zero-based position of this `Fact::TestAssertion` among same-
    /// `(source_path, test_fn)` siblings, in tree-sitter extraction
    /// order. `None` for every other fact kind. Used by
    /// `match_post::matching_post_fact` to pair T₀ assertion #N with
    /// post-commit assertion #N inside the same test fn — see SPEC §5
    /// rationale in
    /// `benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`.
    test_assertion_ordinal: Option<usize>,
    /// `(cfg_attribute_set, impl_receiver_type, ordinal)` disambiguator
    /// for `Fact::FunctionSignature`, populated by
    /// `push_function_signature_facts`. `None` for every other fact
    /// kind. The cfg set + impl receiver are the primary key when
    /// pairing T₀ → post in `match_post::matching_post_fact`; the
    /// ordinal is a tiebreaker for genuine duplicates under the same
    /// primary key (rare). See SPEC §5 rationale in
    /// `benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`.
    function_signature_disambiguator: Option<FnDisambiguator>,
}

/// Private replay-time disambiguator for `Fact::FunctionSignature`.
///
/// Pairs T₀ → post by `(qualified_name, cfg_set, impl_receiver)` plus
/// an ordinal tiebreaker. Lives only on `ObservedFact`; does NOT
/// appear in the serialized `Fact` or `fact_id` and therefore does
/// not affect the corpus schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FnDisambiguator {
    /// Normalized `#[cfg(...)]` / `#[cfg_attr(...)]` attribute texts,
    /// sorted + deduped so the set is order-independent.
    cfg_set: std::collections::BTreeSet<String>,
    /// Text of the enclosing `impl <T> { … }` receiver type, or
    /// `None` for module-level functions.
    impl_receiver: Option<String>,
    /// Zero-based position among same-`(qualified_name, cfg_set,
    /// impl_receiver)` siblings at the same source path. Used only
    /// when two functions at T₀ share the same primary key (genuine
    /// duplicates — rare).
    ordinal: usize,
}

impl Replay {
    /// Run the full T₀-anchored replay and return one [`FactAtCommit`] row
    /// per (fact, commit) pair.
    ///
    /// Steps:
    /// 1. Extract the closed enum of facts from every `.rs` file present at
    ///    `cfg.t0_sha`, then doc-claim facts from eligible markdown files.
    /// 2. Walk first-parent descendants of T₀ and classify every fact
    ///    against each commit's blobs via the SPEC §5 rule engine.
    ///    Classification is commit-tree-local: a [`CommitSymbolIndex`] is
    ///    built from each commit's blobs before any fact is classified.
    ///    `rust-analyzer` is **not** consulted at replay time.
    ///
    /// Fail-closed: returns `Err` on invalid UTF-8 in a markdown blob
    /// (silently degrading would corrupt the corpus).
    pub fn run(cfg: &ReplayConfig) -> Result<Vec<FactAtCommit>> {
        Self::run_inner(cfg, None)
    }

    /// Test-only entry point preserved for API compatibility after Task 3.
    /// The `resolver` parameter is accepted but never invoked; per-commit
    /// classification uses [`CommitSymbolIndex`] exclusively.
    /// Use [`Self::run`] for production.
    #[doc(hidden)]
    pub fn run_with_resolver(
        cfg: &ReplayConfig,
        _resolver: Option<Box<dyn SymbolResolver>>,
    ) -> Result<Vec<FactAtCommit>> {
        // skip_symbol_resolution must be false so the CommitSymbolIndex is
        // built and per-commit tree resolution is exercised.  Returns Err
        // rather than panicking — this is a `pub` entry point even with
        // `#[doc(hidden)]`.
        anyhow::ensure!(
            !cfg.skip_symbol_resolution,
            "run_with_resolver: set skip_symbol_resolution=false so the \
             per-commit tree resolution path is exercised"
        );
        Self::run_inner(cfg, None)
    }

    /// Emit one [`crate::output::FactBodyRow`] per fact_id in `wanted`,
    /// extracting the T₀ fact set the same way [`Self::run`] does but
    /// stopping after the extraction phase.
    ///
    /// Bodies are rendered per SPEC §3 (single-line claim strings — see
    /// [`render_fact_body`]). Source bytes are kept in memory long
    /// enough to compute `FunctionSignature` parameter / return-type
    /// text from the span; the function-signature extractor itself
    /// doesn't store those fields on [`Fact`], so they're parsed from
    /// the T₀ blob via tree-sitter at emit time.
    ///
    /// Returns rows in the order facts are extracted; the writer
    /// ([`crate::output::write_facts_jsonl`]) is responsible for the
    /// final sort by `fact_id`.
    ///
    /// Facts whose `fact_id` is not in `wanted` are skipped. If
    /// `wanted` references a `fact_id` that doesn't exist at T₀, this
    /// returns an error: the Phase 0c artifact contract is one emitted
    /// fact body for every unique corpus fact id.
    pub fn emit_facts(
        cfg: &ReplayConfig,
        wanted: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<crate::output::FactBodyRow>> {
        let pilot = Pilot::open(&AdHocSpec {
            path: cfg.repo_path.clone(),
            t0_sha: cfg.t0_sha.clone(),
        })?;
        let mut facts: Vec<ObservedFact> = Vec::new();
        let mut t0_blobs: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let rust_paths = rust_paths_at(&pilot, &cfg.t0_sha)?;
        let mut facts_so_far: Vec<Fact> = Vec::new();
        for path in &rust_paths {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, path)? {
                let ast = RustAst::parse(&blob)
                    .with_context(|| format!("parse {} @ T0", path.display()))?;
                push_function_signature_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    path,
                    function_signature::extract_observations(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    field::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    symbol_existence::extract(&ast, path),
                );
                let test_facts: Vec<Fact> =
                    test_assertion::extract(&ast, path, &facts_so_far).collect();
                push_test_assertion_facts(&mut facts, &mut facts_so_far, &blob, path, test_facts);
                t0_blobs.insert(path.clone(), blob);
            }
        }
        // Python branch — parallel to the Rust loop above. Python facts are
        // wrapped in `ObservedFact` with `cfg_attribute_set`/`impl_receiver_type`
        // disambiguators left empty (Rust-specific, no Python analog).
        // `doc_claim` is intentionally skipped — the Python stub returns an
        // empty iter today; calling it would just churn.
        let python_paths = python_paths_at(&pilot, &cfg.t0_sha)?;
        for path in &python_paths {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, path)? {
                let ast = crate::ast::python::PythonAst::parse(&blob)
                    .with_context(|| format!("parse {} @ T0 (python)", path.display()))?;
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::function_signature::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::field::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::symbol_existence::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::test_assertion::extract(&ast, path),
                );
                t0_blobs.insert(path.clone(), blob);
            }
        }
        let rust_dirs = rust_paths
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect::<std::collections::BTreeSet<_>>();
        for path in markdown_paths_at(&pilot, &cfg.t0_sha)?
            .into_iter()
            .filter(|p| is_replay_doc_path(p, &rust_dirs))
        {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, &path)? {
                let doc_facts =
                    doc_claim::extract(&blob, &path, &facts_so_far).with_context(|| {
                        format!("parse README at {} @ {}", path.display(), cfg.t0_sha)
                    })?;
                push_observed_facts(&mut facts, &mut facts_so_far, &blob, doc_facts);
                t0_blobs.insert(path, blob);
            }
        }

        let mut out = Vec::with_capacity(wanted.len());
        for observed in &facts {
            let id = fact_id(&observed.fact);
            if !wanted.contains(&id) {
                continue;
            }
            let source_path_norm =
                crate::repo::normalize_path_for_fact_id(observed.fact.source_path());
            let blob = t0_blobs.get(observed.fact.source_path());
            let body = render_fact_body(&observed.fact, blob.map(|v| v.as_slice()));
            out.push(crate::output::FactBodyRow {
                fact_id: id,
                kind: observed.fact.kind_name().to_string(),
                body,
                source_path: source_path_norm,
                line_span: observed.fact.line_span(),
                symbol_path: observed.fact.symbol_path(),
                content_hash_at_observation: observed.fact.content_hash().to_string(),
            });
        }
        let emitted: BTreeSet<String> = out.iter().map(|row| row.fact_id.clone()).collect();
        let missing: Vec<String> = wanted.difference(&emitted).take(10).cloned().collect();
        anyhow::ensure!(
            emitted.len() == wanted.len(),
            "emit-facts could not reconstruct {} requested fact_id(s); first missing: {}",
            wanted.len().saturating_sub(emitted.len()),
            missing.join(", ")
        );
        Ok(out)
    }

    /// Shared implementation used by both [`Self::run`] and
    /// [`Self::run_with_resolver`].
    ///
    /// When `cfg.skip_symbol_resolution` is `false`, a
    /// [`CommitSymbolIndex`] is built for each commit (reusing blobs already
    /// read for fact-source paths) and used as the authoritative source for
    /// cross-file symbol presence checks.
    fn run_inner(
        cfg: &ReplayConfig,
        _resolver: Option<Box<dyn SymbolResolver>>,
    ) -> Result<Vec<FactAtCommit>> {
        let pilot = Pilot::open(&AdHocSpec {
            path: cfg.repo_path.clone(),
            t0_sha: cfg.t0_sha.clone(),
        })?;
        let commits: Vec<CommitRef> = pilot.walk_first_parent()?.collect();

        // Extract the fact set at T₀ across every .rs file present at T₀.
        //
        // We also stash each fact-source blob's T₀ bytes in `t0_blobs` so
        // the per-commit step-3 fast-path below can compare them to the
        // post-commit bytes without re-reading. The map is bounded by the
        // number of fact-source paths and is tiny relative to the corpus;
        // see SPEC §5 invariant in
        // `benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`.
        let mut facts: Vec<ObservedFact> = Vec::new();
        let mut t0_blobs: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let rust_paths = rust_paths_at(&pilot, &cfg.t0_sha)?;
        let mut facts_so_far: Vec<Fact> = Vec::new();
        for path in &rust_paths {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, path)? {
                let ast = RustAst::parse(&blob)
                    .with_context(|| format!("parse {} @ T0", path.display()))?;
                push_function_signature_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    path,
                    function_signature::extract_observations(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    field::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    symbol_existence::extract(&ast, path),
                );
                let test_facts: Vec<Fact> =
                    test_assertion::extract(&ast, path, &facts_so_far).collect();
                push_test_assertion_facts(&mut facts, &mut facts_so_far, &blob, path, test_facts);
                t0_blobs.insert(path.clone(), blob);
            }
        }
        // Python branch — parallel to the Rust loop above. See `emit_facts`
        // for rationale on the empty disambiguators / skipped `doc_claim`.
        // Limitation: Python rows are not added to the per-commit
        // `CommitSymbolIndex` (it is Rust-only via tree-sitter). Python
        // facts are classified by row-level rules R1-R5 at replay time
        // without cross-symbol resolution; R7 (rename detection) over
        // Python is out of scope for v1.2b.
        let python_paths = python_paths_at(&pilot, &cfg.t0_sha)?;
        for path in &python_paths {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, path)? {
                let ast = crate::ast::python::PythonAst::parse(&blob)
                    .with_context(|| format!("parse {} @ T0 (python)", path.display()))?;
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::function_signature::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::field::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::symbol_existence::extract(&ast, path),
                );
                push_observed_facts(
                    &mut facts,
                    &mut facts_so_far,
                    &blob,
                    crate::facts::python::test_assertion::extract(&ast, path),
                );
                t0_blobs.insert(path.clone(), blob);
            }
        }
        let rust_dirs = rust_paths
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect::<std::collections::BTreeSet<_>>();
        for path in markdown_paths_at(&pilot, &cfg.t0_sha)?
            .into_iter()
            .filter(|p| is_replay_doc_path(p, &rust_dirs))
        {
            if let Some(blob) = pilot.read_blob_at(&cfg.t0_sha, &path)? {
                let doc_facts =
                    doc_claim::extract(&blob, &path, &facts_so_far).with_context(|| {
                        format!("parse README at {} @ {}", path.display(), cfg.t0_sha)
                    })?;
                push_observed_facts(&mut facts, &mut facts_so_far, &blob, doc_facts);
                t0_blobs.insert(path, blob);
            }
        }

        #[cfg(test)]
        crate::repo::reset_read_blob_at_call_count();

        let facts_by_path = group_facts_by_source_path(&facts);
        // Build the T₀ qualified-name index per source path.  Used by the
        // rename-candidate filter pipeline (Gate 2: "candidate was not a T₀
        // fact") to reject surviving siblings from consideration as rename
        // targets.  Keyed the same way as `facts_by_path` so the lookup at
        // classify time is O(1).
        let t0_names_by_path = build_t0_names_by_path(&facts);

        let mut rows: Vec<FactAtCommit> = Vec::new();
        for commit in &commits {
            // ── Step 1: read post-commit blobs for all fact-source paths ──────
            // Collect into a HashMap so we can pass them as the blob cache to
            // CommitSymbolIndex::build without double-reading.
            let mut cached_blobs: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
            let mut post_asts: HashMap<PathBuf, Option<RustAst>> = HashMap::new();
            for path in facts_by_path.keys() {
                let blob = pilot.read_blob_at(&commit.sha, path)?;
                // Only parse Rust files as `RustAst`. tree-sitter-rust will
                // silently produce a garbled tree if handed Python source,
                // which then violates `matching_post_fact`'s Rust-disambiguator
                // contract (Plan A.1 fix; see
                // `tests/python_replay_changed_file.rs`). Python paths get
                // `post_ast = None` here and short-circuit to
                // `Label::NeedsRevalidation` at the per-fact classify step
                // below.
                let ast = match crate::lang::Language::for_path(path) {
                    Some(crate::lang::Language::Rust) | None => {
                        blob.as_ref().and_then(|bytes| RustAst::parse(bytes).ok())
                    }
                    Some(crate::lang::Language::Python) => None,
                };
                cached_blobs.insert(path.clone(), blob);
                post_asts.insert(path.clone(), ast);
            }

            // ── Step 2: build commit-local symbol index (when enabled) ────────
            let commit_index: Option<CommitSymbolIndex> = if cfg.skip_symbol_resolution {
                None
            } else {
                let commit_rs_paths = rust_paths_at(&pilot, &commit.sha)?;
                Some(CommitSymbolIndex::build(
                    &pilot,
                    &commit.sha,
                    &commit_rs_paths,
                    &cached_blobs,
                )?)
            };

            // ── Step 3: classify each fact ────────────────────────────────────
            for (path, facts_at_path) in &facts_by_path {
                let post_bytes = cached_blobs.get(path).and_then(|b| b.as_deref());

                // SPEC §5 invariant: an unchanged source file cannot
                // contain a stale fact. When the blob at `commit_sha`
                // is byte-identical to its T₀ counterpart, classify
                // every fact at this path as `Valid` and skip per-fact
                // matching entirely. The bypass lives at the call site
                // (not inside `classify_against_commit`) so it is
                // computed once per `(path, commit)` and visibly
                // sidesteps `matching_post_fact`, `symbol_exists_in_tree`,
                // rename detection, and whitespace/comment diffing.
                // Rationale: benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md.
                let file_byte_identical = match (post_bytes, t0_blobs.get(path)) {
                    (Some(p), Some(t)) => p == t.as_slice(),
                    _ => false,
                };
                if file_byte_identical {
                    for observed in facts_at_path {
                        rows.push(FactAtCommit {
                            fact_id: fact_id(&observed.fact),
                            commit_sha: commit.sha.clone(),
                            label: Label::Valid,
                        });
                    }
                    continue;
                }

                let post_ast = post_asts.get(path).and_then(|a| a.as_ref());
                let t0_names: &HashSet<String> =
                    t0_names_by_path.get(path).unwrap_or(&EMPTY_STRING_SET);

                // Plan A.1 short-circuit (Option C): for Python paths
                // whose post-commit blob is NOT byte-identical to T₀,
                // we cannot mechanically classify via the Rust-oriented
                // AST match path (`matching_post_fact`,
                // `CommitSymbolIndex`, and the rename detector are all
                // Rust-only). Building a parallel PythonAst post-cache
                // + Python-flavored matching is a v1.2c-and-beyond
                // project; for v1.2b's flask held-out round we surface
                // the limitation honestly via `Label::NeedsRevalidation`
                // — the rule chain's existing "ask the baseline" hatch.
                // Documented in `docs/superpowers/plans/...flask-heldout.md`
                // hygiene flags and `tests/python_replay_changed_file.rs`.
                let is_python_path = matches!(
                    crate::lang::Language::for_path(path),
                    Some(crate::lang::Language::Python)
                );
                if is_python_path {
                    for observed in facts_at_path {
                        rows.push(FactAtCommit {
                            fact_id: fact_id(&observed.fact),
                            commit_sha: commit.sha.clone(),
                            label: Label::NeedsRevalidation,
                        });
                    }
                    continue;
                }

                for observed in facts_at_path {
                    let label = classify_against_commit(
                        &observed.fact,
                        path,
                        post_bytes,
                        post_ast,
                        &observed.t0_span_bytes,
                        observed.test_assertion_ordinal,
                        observed.function_signature_disambiguator.as_ref(),
                        cfg,
                        commit_index.as_ref(),
                        t0_names,
                        &commit.sha,
                    )?;
                    rows.push(FactAtCommit {
                        fact_id: fact_id(&observed.fact),
                        commit_sha: commit.sha.clone(),
                        label,
                    });
                }
            }
        }
        Ok(rows)
    }
}

/// Shared empty set for paths that have no T₀ facts (avoids allocation).
static EMPTY_STRING_SET: std::sync::LazyLock<HashSet<String>> =
    std::sync::LazyLock::new(HashSet::new);

fn push_observed_facts(
    facts: &mut Vec<ObservedFact>,
    facts_so_far: &mut Vec<Fact>,
    blob: &[u8],
    extracted: impl IntoIterator<Item = Fact>,
) {
    for fact in extracted {
        facts.push(ObservedFact {
            t0_span_bytes: observed_span_bytes(blob, &fact),
            fact: fact.clone(),
            test_assertion_ordinal: None,
            function_signature_disambiguator: None,
        });
        facts_so_far.push(fact);
    }
}

/// Append `Fact::FunctionSignature` items to `facts`, computing each
/// fact's `FnDisambiguator` from its `(cfg_attribute_set,
/// impl_receiver_type)` plus a per-path ordinal tiebreaker for genuine
/// duplicates.
///
/// The disambiguator is carried only on the private [`ObservedFact`];
/// it does NOT appear in the serialized [`Fact`] or `fact_id` and
/// therefore does not affect the corpus schema. Counter map is keyed
/// by `(path, qualified_name, cfg_set, impl_receiver)` so a future
/// refactor that batches facts across paths cannot silently alias
/// ordinals.
fn push_function_signature_facts(
    facts: &mut Vec<ObservedFact>,
    facts_so_far: &mut Vec<Fact>,
    blob: &[u8],
    path: &Path,
    observations: impl IntoIterator<
        Item = crate::facts::function_signature::FunctionSignatureObservation,
    >,
) {
    use std::collections::BTreeSet;
    let mut counters: HashMap<(PathBuf, String, BTreeSet<String>, Option<String>), usize> =
        HashMap::new();
    for obs in observations {
        let qualified_name = match &obs.fact {
            Fact::FunctionSignature { qualified_name, .. } => qualified_name.clone(),
            _ => unreachable!(
                "function_signature::extract_observations only yields FunctionSignature"
            ),
        };
        let cfg_set: BTreeSet<String> = obs.cfg_attribute_set.iter().cloned().collect();
        let impl_receiver = obs.impl_receiver_type.clone();
        let key = (
            path.to_path_buf(),
            qualified_name.clone(),
            cfg_set.clone(),
            impl_receiver.clone(),
        );
        let counter = counters.entry(key).or_insert(0);
        let ordinal = *counter;
        *counter += 1;
        let disamb = FnDisambiguator {
            cfg_set,
            impl_receiver,
            ordinal,
        };
        facts.push(ObservedFact {
            t0_span_bytes: observed_span_bytes(blob, &obs.fact),
            fact: obs.fact.clone(),
            test_assertion_ordinal: None,
            function_signature_disambiguator: Some(disamb),
        });
        facts_so_far.push(obs.fact);
    }
}

/// Append `Fact::TestAssertion` items to `facts`, computing each fact's
/// zero-based ordinal among same-`(source_path, test_fn)` siblings in
/// the order they arrive from `test_assertion::extract`.
///
/// The ordinal is a structural disambiguator carried only on the
/// private [`ObservedFact`]; it does NOT appear in the serialized
/// [`Fact`] or `fact_id` and therefore does not affect the corpus
/// schema. The counter map is keyed by `(path, test_fn)` so a future
/// refactor that batches facts from multiple source paths into a
/// single call cannot silently alias ordinals across files — the
/// contract is enforced by the key, not by the caller's invocation
/// pattern.
fn push_test_assertion_facts(
    facts: &mut Vec<ObservedFact>,
    facts_so_far: &mut Vec<Fact>,
    blob: &[u8],
    path: &Path,
    extracted: impl IntoIterator<Item = Fact>,
) {
    let mut counters: HashMap<(PathBuf, String), usize> = HashMap::new();
    for fact in extracted {
        let ordinal = match &fact {
            Fact::TestAssertion { test_fn, .. } => {
                let counter = counters
                    .entry((path.to_path_buf(), test_fn.clone()))
                    .or_insert(0);
                let n = *counter;
                *counter += 1;
                Some(n)
            }
            _ => None,
        };
        facts.push(ObservedFact {
            t0_span_bytes: observed_span_bytes(blob, &fact),
            fact: fact.clone(),
            test_assertion_ordinal: ordinal,
            function_signature_disambiguator: None,
        });
        facts_so_far.push(fact);
    }
}

fn group_facts_by_source_path(facts: &[ObservedFact]) -> BTreeMap<PathBuf, Vec<&ObservedFact>> {
    let mut grouped: BTreeMap<PathBuf, Vec<&ObservedFact>> = BTreeMap::new();
    for observed in facts {
        grouped
            .entry(observed.fact.source_path().to_path_buf())
            .or_default()
            .push(observed);
    }
    grouped
}

/// Build a map from source path → set of T₀ qualified names at that path.
///
/// Used by the rename-candidate Gate 2 ("candidate was not a T₀ fact"):
/// before accepting a post-commit symbol as a rename target we verify it
/// did NOT exist at T₀ as an independent fact in the same file.
fn build_t0_names_by_path(facts: &[ObservedFact]) -> BTreeMap<PathBuf, HashSet<String>> {
    let mut by_path: BTreeMap<PathBuf, HashSet<String>> = BTreeMap::new();
    for observed in facts {
        let key = qualified_key_for(&observed.fact).to_string();
        by_path
            .entry(observed.fact.source_path().to_path_buf())
            .or_default()
            .insert(key);
    }
    by_path
}

/// Return the qualified key that uniquely identifies a fact for T₀-presence
/// checks.  Mirrors the per-kind primary key used by `matching_post_fact`:
/// - `FunctionSignature` / `PublicSymbol` / `DocClaim` → `qualified_name`
/// - `Field` → `qualified_path`
/// - `TestAssertion` → `test_fn`
fn qualified_key_for(fact: &Fact) -> &str {
    match fact {
        Fact::FunctionSignature { qualified_name, .. } => qualified_name,
        Fact::Field { qualified_path, .. } => qualified_path,
        Fact::PublicSymbol { qualified_name, .. } => qualified_name,
        Fact::DocClaim { qualified_name, .. } => qualified_name,
        Fact::TestAssertion { test_fn, .. } => test_fn,
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

struct AdHocSpec {
    path: PathBuf,
    t0_sha: String,
}

impl PilotRepoSpec for AdHocSpec {
    fn local_clone_path(&self) -> &Path {
        &self.path
    }
    fn t0_sha(&self) -> &str {
        &self.t0_sha
    }
}

/// Return all `.rs` file paths present in the git tree at `sha`. Filter
/// is expressed via [`crate::lang::Language`] dispatch so future
/// languages (Python) can extend with sibling helpers without touching
/// this function.
fn rust_paths_at(pilot: &Pilot, sha: &str) -> Result<Vec<PathBuf>> {
    use crate::lang::Language;
    Ok(tree_paths_at(pilot, sha)?
        .into_iter()
        .filter(|p| Language::for_path(p) == Some(Language::Rust))
        .collect())
}

/// Return all `.py` file paths present in the git tree at `sha`. Filter
/// is expressed via [`crate::lang::Language`] dispatch so the Rust and
/// Python branches in `emit_facts` / `run_inner` share the same path-
/// classification policy.
fn python_paths_at(pilot: &Pilot, sha: &str) -> Result<Vec<PathBuf>> {
    use crate::lang::Language;
    Ok(tree_paths_at(pilot, sha)?
        .into_iter()
        .filter(|p| Language::for_path(p) == Some(Language::Python))
        .collect())
}

fn markdown_paths_at(pilot: &Pilot, sha: &str) -> Result<Vec<PathBuf>> {
    Ok(tree_paths_at(pilot, sha)?
        .into_iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect())
}

fn tree_paths_at(pilot: &Pilot, sha: &str) -> Result<Vec<PathBuf>> {
    validate_sha_hex(sha)?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(pilot.repo_path())
        .args(["ls-tree", "-r", "--name-only", sha])
        .output()
        .with_context(|| format!("git ls-tree {sha}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .collect())
}

fn is_replay_doc_path(path: &Path, rust_dirs: &std::collections::BTreeSet<PathBuf>) -> bool {
    path.components().count() == 1
        || path
            .parent()
            .map(|parent| rust_dirs.contains(parent))
            .unwrap_or(false)
}

/// Reject anything that isn't exactly 40 lowercase hex characters.
///
/// Used as a defence-in-depth check before passing a SHA into
/// `git ls-tree` so a malformed value cannot reach a subprocess argv.
pub fn validate_sha_hex(sha: &str) -> Result<()> {
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        anyhow::bail!("git commit sha must be exactly 40 lowercase hex characters: {sha}");
    }
    Ok(())
}

/// Stable, unique ID for a fact — used as the primary key in output rows.
///
/// Source paths are normalized via [`normalize_path_for_fact_id`] (a pure
/// string transform) so that no absolute filesystem path can ever leak
/// into a `fact_id`, regardless of the user's `pwd` or where the repo
/// lives on disk.
pub(crate) fn fact_id(fact: &Fact) -> String {
    match fact {
        Fact::FunctionSignature {
            qualified_name,
            source_path,
            span,
            ..
        } => {
            format!(
                "FunctionSignature::{qualified_name}::{}::{}",
                normalize_path_for_fact_id(source_path),
                span.line_start
            )
        }
        Fact::Field {
            qualified_path,
            source_path,
            span,
            ..
        } => {
            format!(
                "Field::{qualified_path}::{}::{}",
                normalize_path_for_fact_id(source_path),
                span.line_start
            )
        }
        Fact::PublicSymbol {
            qualified_name,
            source_path,
            span,
            ..
        } => {
            format!(
                "PublicSymbol::{qualified_name}::{}::{}",
                normalize_path_for_fact_id(source_path),
                span.line_start
            )
        }
        Fact::DocClaim {
            qualified_name,
            doc_path,
            mention_span,
            ..
        } => {
            format!(
                "DocClaim::{qualified_name}::{}::{}",
                normalize_path_for_fact_id(doc_path),
                mention_span.line_start
            )
        }
        Fact::TestAssertion {
            test_fn,
            source_path,
            span,
            ..
        } => {
            format!(
                "TestAssertion::{test_fn}::{}::{}",
                normalize_path_for_fact_id(source_path),
                span.line_start
            )
        }
    }
}

// ── Per-commit state ──────────────────────────────────────────────────────────

struct CommitState {
    file_exists: bool,
    post_span_hash: Option<String>,
    structurally_classifiable: bool,
    whitespace_or_comment_only: bool,
    symbol_resolves: bool,
    rename: Option<String>,
}

impl PostCommitState for CommitState {
    fn file_exists(&self) -> bool {
        self.file_exists
    }
    fn symbol_resolves(&self) -> bool {
        self.symbol_resolves
    }
    fn rename_candidate(&self) -> Option<&str> {
        self.rename.as_deref()
    }
    fn post_span_hash(&self) -> Option<&str> {
        self.post_span_hash.as_deref()
    }
    fn whitespace_or_comment_only(&self) -> bool {
        self.whitespace_or_comment_only
    }
    fn structurally_classifiable(&self) -> bool {
        self.structurally_classifiable
    }
}

impl CommitState {
    fn deleted() -> Self {
        Self {
            file_exists: false,
            post_span_hash: None,
            structurally_classifiable: false,
            whitespace_or_comment_only: false,
            symbol_resolves: false,
            rename: None,
        }
    }
}

/// Classify `fact` against the post-commit blob.
///
/// Symbol presence is determined commit-tree-locally:
/// - If `matching_post_fact` finds the symbol at its original path → the
///   symbol is present; compute hash/whitespace deltas and classify.
/// - If not found at the original path AND `commit_index` is `Some`:
///   - If the index says the symbol exists elsewhere in the tree
///     (moved to a different file) → `symbol_resolves = true, no hash`
///     → `classify` returns `NeedsRevalidation` (gray area for LLM).
///   - Otherwise → `symbol_resolves = false` → attempt rename detection
///     → `StaleSourceDeleted` or `StaleSymbolRenamed`.
/// - When `commit_index` is `None` (`skip_symbol_resolution = true`) →
///   bypass cross-file lookup (unit-test mode).
#[allow(clippy::too_many_arguments)]
fn classify_against_commit(
    fact: &Fact,
    path: &Path,
    post_blob: Option<&[u8]>,
    post_ast: Option<&RustAst>,
    t0_span_bytes: &[u8],
    test_assertion_ordinal: Option<usize>,
    function_signature_disambiguator: Option<&FnDisambiguator>,
    cfg: &ReplayConfig,
    commit_index: Option<&CommitSymbolIndex>,
    t0_qualified_names: &HashSet<String>,
    commit_sha: &str,
) -> Result<Label> {
    let state = match post_blob {
        // File was deleted at this commit.
        None => CommitState::deleted(),

        Some(post_bytes) => {
            let observed_hash = fact.content_hash();

            // Search the post-commit file for the same fact kind/key.
            let post_fact = match_post::matching_post_fact(
                fact,
                path,
                post_bytes,
                post_ast,
                test_assertion_ordinal,
                function_signature_disambiguator,
                commit_sha,
            )?;

            match post_fact {
                Some((post_span, post_hash)) => {
                    // Symbol found at its original path — compute deltas.
                    let after_bytes = &post_bytes[post_span.byte_range.clone()];
                    // Per-path language dispatch: Rust paths use the Rust grammar
                    // (byte-identical to the pre-Task-13 behavior); Python paths
                    // use the Python grammar. Paths without a recognized
                    // extension default to Rust to preserve historical behavior.
                    let lang = crate::lang::Language::for_path(path)
                        .unwrap_or(crate::lang::Language::Rust);
                    let ws_only = is_whitespace_or_comment_only(t0_span_bytes, after_bytes, lang);
                    // Any signature-level hash difference is structurally classifiable.
                    let structural = post_hash != observed_hash;
                    CommitState {
                        file_exists: true,
                        post_span_hash: Some(post_hash),
                        structurally_classifiable: structural,
                        whitespace_or_comment_only: ws_only,
                        symbol_resolves: true,
                        rename: None,
                    }
                }
                None => {
                    // Pass-5 Cluster F: for `Fact::Field`, before any
                    // tree-wide symbol-resolution branch, do a file-
                    // local same-leaf-elsewhere check. If the field's
                    // exact `qualified_path` is gone but the leaf
                    // name appears in another struct/variant in the
                    // SAME file (e.g. `Config::dfa_size_limit` →
                    // `ConfigInner::dfa_size_limit` after a nesting
                    // refactor), route to `NeedsRevalidation`. The
                    // check is file-local; it does not depend on
                    // `cfg.skip_symbol_resolution` or `commit_index`,
                    // so unit-test fixtures with
                    // `skip_symbol_resolution = true` get the same
                    // routing as production runs. SPEC §5 rationale
                    // in `benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`.
                    if let Fact::Field { qualified_path, .. } = fact {
                        if let Some(ast) = post_ast {
                            if field::same_file_leaf_elsewhere(ast, path, qualified_path) {
                                return Ok(classify(
                                    fact,
                                    &CommitState {
                                        file_exists: true,
                                        post_span_hash: None,
                                        structurally_classifiable: false,
                                        whitespace_or_comment_only: false,
                                        symbol_resolves: true,
                                        rename: None,
                                    },
                                ));
                            }
                        }
                    }
                    // Symbol not found at its original path in the post-commit AST.
                    if cfg.skip_symbol_resolution {
                        // Unit-test mode: no cross-file or rename detection.
                        CommitState {
                            file_exists: true,
                            post_span_hash: None,
                            structurally_classifiable: false,
                            whitespace_or_comment_only: false,
                            symbol_resolves: false,
                            rename: None,
                        }
                    } else if let Some(index) = commit_index {
                        // Commit-tree-local resolution: check whether the symbol
                        // exists anywhere in this commit's tree.
                        //
                        // Case 1 — symbol exists at a different path: the fact's
                        // source was moved to another file.  This is a gray area
                        // for the LLM (moved vs. renamed vs. coincidental name
                        // match), so we route to NeedsRevalidation without
                        // triggering StaleSymbolRenamed.
                        //
                        // Case 2 — symbol absent from the commit tree entirely:
                        // the qualified name is gone from every file.  Run the
                        // typed rename pipeline against the same-file post-commit
                        // AST to detect within-file renames (e.g. `old_name` →
                        // `new_name` in the same module/impl block).  If a
                        // candidate passes all four gates → StaleSymbolRenamed;
                        // otherwise → StaleSourceDeleted.
                        if index.symbol_exists_in_tree(fact) {
                            CommitState {
                                file_exists: true,
                                post_span_hash: None,
                                structurally_classifiable: false,
                                whitespace_or_comment_only: false,
                                symbol_resolves: true,
                                rename: None,
                            }
                        } else {
                            let candidates = match_post::rename_candidates_for_typed(
                                fact, path, post_bytes, post_ast,
                            );
                            let origin = RenameOrigin::new(qualified_key_for(fact), t0_span_bytes);
                            let rename = rename_candidate_typed(
                                &origin,
                                &candidates,
                                t0_qualified_names,
                                0.6,
                            );
                            CommitState {
                                file_exists: true,
                                post_span_hash: None,
                                structurally_classifiable: false,
                                whitespace_or_comment_only: false,
                                symbol_resolves: false,
                                rename,
                            }
                        }
                    } else {
                        // `skip_symbol_resolution = false` but `commit_index` is
                        // `None`.  This cannot happen in practice: `run_inner`
                        // always builds the index when `skip_symbol_resolution`
                        // is `false`.  Treat as deleted (no rename detection)
                        // rather than panicking so behaviour is defined.
                        CommitState {
                            file_exists: true,
                            post_span_hash: None,
                            structurally_classifiable: false,
                            whitespace_or_comment_only: false,
                            symbol_resolves: false,
                            rename: None,
                        }
                    }
                }
            }
        }
    };

    Ok(classify(fact, &state))
}

/// Render the SPEC §3 single-line claim body for `fact`. For
/// `Fact::FunctionSignature` the parameter list and return type are
/// parsed from the T₀ source blob via tree-sitter (the public
/// [`Fact::FunctionSignature`] variant only stores the qualified name
/// and content hash — parameter and return-type text are not part of
/// the serialized schema and would otherwise need a separate
/// extraction pass).
///
/// `blob` should be the T₀ bytes of `fact.source_path()` when
/// available; missing blobs fall back to a span-empty rendering so
/// `emit_facts` never panics on a malformed input.
pub fn render_fact_body(fact: &Fact, blob: Option<&[u8]>) -> String {
    match fact {
        Fact::FunctionSignature { qualified_name, .. } => {
            let (params, ret) = blob
                .map(|b| parse_fn_signature_parts(b, fact))
                .unwrap_or((String::new(), "()".to_string()));
            format!("function {qualified_name} has parameters ({params}) with return type {ret}")
        }
        Fact::Field {
            qualified_path,
            type_text,
            ..
        } => {
            // Split `T::name` (or `E::Variant::name`) into `parent` + `name`.
            // The leaf is always the last `::` segment; the parent is
            // everything before it. Single-segment paths (shouldn't happen
            // for fields but stay defensive) fall back to parent = "".
            let (parent, name) = match qualified_path.rsplit_once("::") {
                Some((p, n)) => (p, n),
                None => ("", qualified_path.as_str()),
            };
            format!("type {parent} has field {name} of type {type_text}")
        }
        Fact::PublicSymbol {
            qualified_name,
            source_path,
            ..
        } => {
            // The extractor stores only the leaf name in `qualified_name`
            // (`extract_named_item` in `symbol_existence.rs`). Use the
            // normalized source path as the module identifier so the body
            // is stable across machines and never includes an absolute
            // filesystem path.
            let module = crate::repo::normalize_path_for_fact_id(source_path);
            format!("exported name {qualified_name} resolves in module {module}")
        }
        Fact::DocClaim {
            qualified_name,
            doc_path,
            ..
        } => {
            let doc_file = crate::repo::normalize_path_for_fact_id(doc_path);
            format!("doc {doc_file} mentions symbol {qualified_name}")
        }
        Fact::TestAssertion {
            test_fn,
            asserted_symbol,
            ..
        } => {
            // SPEC §3.1 binds the assertion to the test function and
            // optionally to an asserted-on symbol. When the extractor
            // can't resolve a symbol from the macro arguments
            // (`asserted_symbol = None`), fall back to the literal
            // `(none)` so the body string is always present.
            let target = asserted_symbol.as_deref().unwrap_or("(none)");
            format!("test {test_fn} asserts property about symbol {target}")
        }
    }
}

/// Parse the parameter list and return type out of the T₀ bytes of a
/// `function_item`'s signature span. Returns `(params, ret)` where
/// `params` is the contents of the `()` and `ret` is the return type
/// text (with `()` as the default for functions whose declaration omits
/// `-> R`).
///
/// Implementation note: we re-parse the entire source blob rather than
/// trying to lex inside the span alone — the span ends before the
/// function body's opening brace, so the `function_item` node is fully
/// present in the AST and tree-sitter gives us labeled `parameters` /
/// `return_type` child fields without further normalization. Matching
/// is done by `(qualified_name, line_start)` so multiple `fn foo` items
/// in the same file disambiguate cleanly.
fn parse_fn_signature_parts(blob: &[u8], fact: &Fact) -> (String, String) {
    let (qualified_name, line_start) = match fact {
        Fact::FunctionSignature {
            qualified_name,
            span,
            ..
        } => (qualified_name.as_str(), span.line_start),
        _ => return (String::new(), "()".to_string()),
    };
    let ast = match RustAst::parse(blob) {
        Ok(ast) => ast,
        Err(_) => return (String::new(), "()".to_string()),
    };
    let src = ast.source();
    let leaf = qualified_name.rsplit("::").next().unwrap_or(qualified_name);
    let mut found: Option<(String, String)> = None;
    walk_function_items(ast.root(), src, leaf, line_start, &mut found);
    found.unwrap_or((String::new(), "()".to_string()))
}

/// Maximum number of source lines that a function's leading attributes
/// and doc comments may push the recorded fact span above the actual
/// `fn` keyword line. Functions with deeper attribute stacks will fall
/// through to the empty-params / `()` default in `parse_fn_signature_parts`.
/// Set conservatively from the pilot corpus (ripgrep at af6b6c54);
/// raise if a downstream test surfaces a missed match.
const FN_ATTR_LINE_WINDOW: i64 = 8;

fn walk_function_items(
    node: tree_sitter::Node<'_>,
    src: &[u8],
    leaf: &str,
    line_start: u32,
    out: &mut Option<(String, String)>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "function_item" {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(src) {
                if name == leaf {
                    // The fact's `line_start` is the 1-based start of the
                    // (possibly attribute-leading) signature span. The
                    // tree-sitter `function_item` itself starts at the
                    // `fn` keyword (or the visibility modifier), so we
                    // accept any match whose `function_item` start row is
                    // within a small window of `line_start` — leading
                    // attributes and doc comments push the fact span
                    // upwards but never downwards.
                    let item_row = (node.start_position().row + 1) as i64;
                    if (item_row - line_start as i64).abs() <= FN_ATTR_LINE_WINDOW {
                        let params = node
                            .child_by_field_name("parameters")
                            .and_then(|n| n.utf8_text(src).ok())
                            .map(|s| {
                                // `parameters` includes the surrounding
                                // `(` and `)`; strip them so the rendered
                                // body uses our own parentheses.
                                let t = s.trim();
                                let inner = t
                                    .strip_prefix('(')
                                    .and_then(|s| s.strip_suffix(')'))
                                    .unwrap_or(t);
                                inner.trim().to_string()
                            })
                            .unwrap_or_default();
                        let ret = node
                            .child_by_field_name("return_type")
                            .and_then(|n| n.utf8_text(src).ok())
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|| "()".to_string());
                        *out = Some((params, ret));
                        return;
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_function_items(child, src, leaf, line_start, out);
        if out.is_some() {
            return;
        }
    }
}

fn observed_span_bytes(blob: &[u8], fact: &Fact) -> Vec<u8> {
    blob[span_for(fact).byte_range.clone()].to_vec()
}

fn span_for(fact: &Fact) -> &Span {
    match fact {
        Fact::FunctionSignature { span, .. } => span,
        Fact::Field { span, .. } => span,
        Fact::PublicSymbol { span, .. } => span,
        Fact::DocClaim { mention_span, .. } => mention_span,
        Fact::TestAssertion { span, .. } => span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit_all(repo: &Path, message: &str) {
        git(repo, &["add", "."]);
        git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                message,
            ],
        );
    }

    #[test]
    fn replay_reads_each_source_blob_once_per_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "--initial-branch=main"]);
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            b"[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/lib.rs"),
            b"pub fn a() -> i32 { 1 }\npub fn b() -> i32 { 2 }\npub fn c() -> i32 { 3 }\npub fn d() -> i32 { 4 }\npub fn e() -> i32 { 5 }\n",
        )
        .unwrap();
        commit_all(repo, "init");
        let t0 = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        std::fs::write(
            repo.join("src/lib.rs"),
            b"pub fn a() -> i32 { 10 }\npub fn b() -> i32 { 2 }\npub fn c() -> i32 { 3 }\npub fn d() -> i32 { 4 }\npub fn e() -> i32 { 5 }\n",
        )
        .unwrap();
        commit_all(repo, "body tweak one");
        std::fs::write(
            repo.join("src/lib.rs"),
            b"pub fn a() -> i32 { 10 }\npub fn b() -> i32 { 20 }\npub fn c() -> i32 { 3 }\npub fn d() -> i32 { 4 }\npub fn e() -> i32 { 5 }\n",
        )
        .unwrap();
        commit_all(repo, "body tweak two");

        let cfg = ReplayConfig {
            repo_path: repo.to_path_buf(),
            t0_sha: t0,
            skip_symbol_resolution: true,
        };
        let rows = Replay::run(&cfg).unwrap();
        assert_eq!(rows.len(), 30);
        let reads = crate::repo::read_blob_at_call_count();
        assert!(
            reads <= 3,
            "expected at most one blob read per commit for one source file, got {reads}"
        );
    }
}
