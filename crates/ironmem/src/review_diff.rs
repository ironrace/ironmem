//! Deterministic Git diff artifacts for bounded review ingestion.

use std::path::PathBuf;
use std::process::Command;

use crate::MemoryError;

/// The Git revision source used to obtain a review diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiffSource {
    /// The merge-base range from `base` to `head`.
    Range { base: String, head: String },
    /// Uncommitted changes relative to `HEAD`.
    Worktree,
}

/// An immutable request for a review-diff artifact or source expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiffRequest {
    /// Git worktree in which to execute the diff command.
    pub repo: PathBuf,
    /// Revision source to render.
    pub source: ReviewDiffSource,
}

impl ReviewDiffRequest {
    /// Creates a request for the merge-base diff between `base` and `head`.
    pub fn range(
        repo: impl Into<PathBuf>,
        base: impl Into<String>,
        head: impl Into<String>,
    ) -> Self {
        Self {
            repo: repo.into(),
            source: ReviewDiffSource::Range {
                base: base.into(),
                head: head.into(),
            },
        }
    }

    /// Creates a request for the uncommitted worktree diff relative to `HEAD`.
    pub fn worktree(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            source: ReviewDiffSource::Worktree,
        }
    }
}

/// Stable byte and approximate-token measurements for an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewDiffMetrics {
    /// Bytes in the original unified diff.
    pub source_bytes: usize,
    /// Bytes in the rendered review artifact.
    pub artifact_bytes: usize,
    /// Estimated source tokens, computed as `ceil(bytes / 4)`.
    pub source_estimated_tokens: usize,
    /// Estimated artifact tokens, computed as `ceil(bytes / 4)`.
    pub artifact_estimated_tokens: usize,
}

/// A stable hunk reference in a review-diff artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiffHunk {
    /// One-based ordinal within its file.
    pub ordinal: usize,
    /// Original unified-diff hunk header.
    pub header: String,
    /// Stable selector accepted by callers as `path#hunk-ordinal`.
    pub selector: String,
}

/// A changed file and its source hunk references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiffFile {
    /// Stable Git path, preferring the new-side path where one exists.
    pub path: String,
    /// Hunks in their original source order.
    pub hunks: Vec<ReviewDiffHunk>,
}

/// A compressed, indexed review-diff artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiffArtifact {
    /// The complete deterministic artifact suitable for bounded ingestion.
    pub rendered: String,
    /// Every source file and hunk, including content the compressed body omitted.
    pub files: Vec<ReviewDiffFile>,
    /// Byte and estimated-token measurements rendered in the artifact footer.
    pub metrics: ReviewDiffMetrics,
}

#[derive(Debug, Clone)]
struct ParsedHunk {
    public: ReviewDiffHunk,
    source: String,
}

#[derive(Debug, Clone)]
struct ParsedFile {
    public: ReviewDiffFile,
    preamble: String,
    section: String,
    hunks: Vec<ParsedHunk>,
}

/// Builds a deterministic compressed artifact for the requested source diff.
///
/// The feature gate prevents accidental dependency or ingestion-cost expansion:
/// callers can safely fall back to their original source when this returns an
/// error.
#[cfg(feature = "headroom-compression")]
pub fn build_review_diff(request: &ReviewDiffRequest) -> Result<ReviewDiffArtifact, MemoryError> {
    let source = read_source(request)?;
    let parsed = parse_unified_diff(&source);
    let files = parsed
        .iter()
        .map(|file| file.public.clone())
        .collect::<Vec<_>>();
    let index = render_index(&files);
    let compressed = headroom_core::transforms::DiffCompressor::default()
        .compress(&source, "review diff")
        .compressed;
    let source_bytes = source.len();
    let rendered = render_with_metrics(&index, &compressed, source_bytes);
    let metrics = ReviewDiffMetrics {
        source_bytes,
        artifact_bytes: rendered.len(),
        source_estimated_tokens: estimated_tokens(source_bytes),
        artifact_estimated_tokens: estimated_tokens(rendered.len()),
    };

    if metrics.artifact_bytes >= metrics.source_bytes {
        return Err(MemoryError::Validation(
            "review-diff artifact did not reduce ingestion size".into(),
        ));
    }

    Ok(ReviewDiffArtifact {
        rendered,
        files,
        metrics,
    })
}

/// Reports that compressed artifact generation needs the opt-in dependency.
#[cfg(not(feature = "headroom-compression"))]
pub fn build_review_diff(_request: &ReviewDiffRequest) -> Result<ReviewDiffArtifact, MemoryError> {
    Err(MemoryError::Validation(
        "review-diff artifact generation requires the headroom-compression feature".into(),
    ))
}

/// Expands an original file section or a one-based hunk ordinal from source.
///
/// Expansion intentionally does not require compression, so clients can always
/// retrieve the original Git diff content when the Git repository is available.
pub fn expand_review_diff(
    request: &ReviewDiffRequest,
    path: &str,
    hunk: Option<usize>,
) -> Result<String, MemoryError> {
    let source = read_source(request)?;
    let parsed = parse_unified_diff(&source);
    let file = parsed
        .iter()
        .find(|file| file.public.path == path)
        .ok_or_else(|| MemoryError::NotFound(format!("review-diff path not found: {path}")))?;

    match hunk {
        None => Ok(file.section.clone()),
        Some(0) => Err(MemoryError::Validation(
            "review-diff hunk ordinal must be one-based".into(),
        )),
        Some(ordinal) => file
            .hunks
            .get(ordinal - 1)
            .map(|hunk| format!("{}{}", file.preamble, hunk.source))
            .ok_or_else(|| {
                MemoryError::NotFound(format!(
                    "review-diff hunk {ordinal} not found for path: {path}"
                ))
            }),
    }
}

fn read_source(request: &ReviewDiffRequest) -> Result<String, MemoryError> {
    let mut command = Command::new("git");
    command.current_dir(&request.repo);
    command.args(["diff", "--no-ext-diff", "--unified=3"]);
    match &request.source {
        ReviewDiffSource::Range { base, head } => {
            command.arg(format!("{base}...{head}"));
        }
        ReviewDiffSource::Worktree => {
            command.arg("HEAD");
        }
    }

    let output = command
        .output()
        .map_err(|_| MemoryError::Validation("review-diff git diff could not start".into()))?;
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map_or_else(|| "terminated".to_owned(), |code| format!("exit {code}"));
        return Err(MemoryError::Validation(format!(
            "review-diff git diff failed with status {status}"
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        MemoryError::Validation("review-diff git diff produced non-UTF-8 output".into())
    })
}

fn parse_unified_diff(source: &str) -> Vec<ParsedFile> {
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let starts = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("diff --git ").then_some(index))
        .collect::<Vec<_>>();

    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).copied().unwrap_or(lines.len());
            parse_file_section(&lines[start..end])
        })
        .collect()
}

fn parse_file_section(lines: &[&str]) -> ParsedFile {
    let section = lines.concat();
    let path = stable_path(lines).unwrap_or_else(|| "<unknown>".to_owned());
    let hunk_starts = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("@@ ").then_some(index))
        .collect::<Vec<_>>();
    let preamble_end = hunk_starts.first().copied().unwrap_or(lines.len());
    let preamble = lines[..preamble_end].concat();
    let hunks = hunk_starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = hunk_starts
                .get(position + 1)
                .copied()
                .unwrap_or(lines.len());
            let ordinal = position + 1;
            let header = lines[start].trim_end_matches(['\r', '\n']).to_owned();
            ParsedHunk {
                public: ReviewDiffHunk {
                    ordinal,
                    selector: format!("{path}#hunk-{ordinal}"),
                    header,
                },
                source: lines[start..end].concat(),
            }
        })
        .collect::<Vec<_>>();
    let public = ReviewDiffFile {
        path,
        hunks: hunks.iter().map(|hunk| hunk.public.clone()).collect(),
    };

    ParsedFile {
        public,
        preamble,
        section,
        hunks,
    }
}

fn stable_path(lines: &[&str]) -> Option<String> {
    let from_marker = |marker: &str| {
        lines.iter().find_map(|line| {
            line.strip_prefix(marker).and_then(|path| {
                let path = path.trim_end_matches(['\r', '\n']);
                (!path.is_empty() && path != "/dev/null").then(|| {
                    path.strip_prefix("a/")
                        .or_else(|| path.strip_prefix("b/"))
                        .unwrap_or(path)
                        .to_owned()
                })
            })
        })
    };

    from_marker("+++ ")
        .or_else(|| from_marker("--- "))
        .or_else(|| {
            lines.first().and_then(|line| {
                let mut paths = line.strip_prefix("diff --git ")?.split_whitespace();
                let _old = paths.next()?;
                paths
                    .next()
                    .and_then(|path| path.strip_prefix("b/").or_else(|| path.strip_prefix("a/")))
                    .map(str::to_owned)
            })
        })
}

#[cfg(feature = "headroom-compression")]
fn render_index(files: &[ReviewDiffFile]) -> String {
    let mut rendered = String::from("review-diff source index\n");
    for file in files {
        rendered.push_str("file: ");
        rendered.push_str(&file.path);
        rendered.push('\n');
        if file.hunks.is_empty() {
            rendered.push_str("  hunks: none\n");
        }
        for hunk in &file.hunks {
            rendered.push_str("  ");
            rendered.push_str(&hunk.selector);
            rendered.push_str(": ");
            rendered.push_str(&hunk.header);
            rendered.push('\n');
        }
    }
    rendered
}

#[cfg(feature = "headroom-compression")]
fn render_with_metrics(index: &str, compressed: &str, source_bytes: usize) -> String {
    let source_estimated_tokens = estimated_tokens(source_bytes);
    let mut artifact_bytes = 0;
    loop {
        let rendered = format!(
            "{index}\nreview-diff compressed body\n{compressed}\n\
             review-diff metrics\n\
             source_bytes={source_bytes}\n\
             artifact_bytes={artifact_bytes}\n\
             source_estimated_tokens={source_estimated_tokens}\n\
             artifact_estimated_tokens={}\n",
            estimated_tokens(artifact_bytes),
        );
        let actual_bytes = rendered.len();
        if actual_bytes == artifact_bytes {
            return rendered;
        }
        artifact_bytes = actual_bytes;
    }
}

#[cfg(any(feature = "headroom-compression", test))]
fn estimated_tokens(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::{estimated_tokens, parse_unified_diff};

    #[test]
    fn parser_preserves_hunk_order_and_new_side_path() {
        let parsed = parse_unified_diff(
            "diff --git a/old.txt b/new.txt\n--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n-old\n+new\n",
        );

        assert_eq!(parsed[0].public.path, "new.txt");
        assert_eq!(parsed[0].public.hunks[0].selector, "new.txt#hunk-1");
        assert_eq!(estimated_tokens(5), 2);
    }
}
