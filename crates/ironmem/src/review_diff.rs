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
    /// Stable, line-safe selector accepted by callers as `path#hunk-ordinal`.
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
    snapshot: Vec<ParsedFile>,
}

impl ReviewDiffArtifact {
    /// Expands an original file section or hunk from this artifact's immutable
    /// source snapshot. Unlike request-based expansion, this remains stable if
    /// the worktree changes after the artifact is built.
    pub fn expand(&self, path: &str, hunk: Option<usize>) -> Result<String, MemoryError> {
        expand_parsed(&self.snapshot, path, hunk)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedHunk {
    public: ReviewDiffHunk,
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        snapshot: parsed,
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
    expand_parsed(&parsed, path, hunk)
}

fn expand_parsed(
    parsed: &[ParsedFile],
    path: &str,
    hunk: Option<usize>,
) -> Result<String, MemoryError> {
    let (path, hunk) = resolve_expansion_target(parsed, path, hunk);
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

fn resolve_expansion_target(
    parsed: &[ParsedFile],
    input: &str,
    hunk: Option<usize>,
) -> (String, Option<usize>) {
    if hunk.is_some() {
        return (
            decode_rendered_path(input).unwrap_or_else(|| input.to_owned()),
            hunk,
        );
    }
    if parsed.iter().any(|file| file.public.path == input) {
        return (input.to_owned(), None);
    }
    parse_rendered_selector(input).unwrap_or_else(|| (input.to_owned(), None))
}

fn read_source(request: &ReviewDiffRequest) -> Result<String, MemoryError> {
    let mut command = Command::new("git");
    command.current_dir(&request.repo);
    command.args(["diff", "--no-ext-diff", "--unified=3"]);
    match &request.source {
        ReviewDiffSource::Range { base, head } => {
            validate_revision(base, "base")?;
            validate_revision(head, "head")?;
            command.arg(format!("{base}...{head}")).arg("--");
        }
        ReviewDiffSource::Worktree => {
            command.arg("HEAD").arg("--");
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

fn validate_revision(revision: &str, name: &str) -> Result<(), MemoryError> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.contains('\0')
        || revision
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || revision.contains("..")
    {
        return Err(MemoryError::Validation(format!(
            "review-diff {name} revision is unsafe"
        )));
    }
    Ok(())
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
                    selector: format!("{}#hunk-{ordinal}", render_path(&path)),
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
            line.strip_prefix(marker)
                .and_then(parse_marker_path)
                .and_then(|path| normalize_git_path(&path))
        })
    };

    from_marker("+++ ")
        .or_else(|| from_marker("--- "))
        .or_else(|| lines.first().and_then(|line| parse_diff_header_path(line)))
}

fn parse_marker_path(value: &str) -> Option<String> {
    let value = value.trim_end_matches(['\r', '\n']);
    if value.starts_with('"') {
        parse_quoted_git_path(value).map(|(path, _)| path)
    } else {
        let path = value
            .split_once('\t')
            .map_or(value, |(path, _timestamp)| path);
        (!path.is_empty()).then(|| path.to_owned())
    }
}

fn parse_diff_header_path(line: &str) -> Option<String> {
    let value = line
        .strip_prefix("diff --git ")?
        .trim_end_matches(['\r', '\n']);
    if value.starts_with('"') {
        let (_, remaining) = take_git_path_token(value)?;
        let (new_path, _) = take_git_path_token(remaining)?;
        return normalize_git_path(&new_path);
    }

    parse_unquoted_diff_header_paths(value).and_then(|(_, new_path)| normalize_git_path(&new_path))
}

fn take_git_path_token(value: &str) -> Option<(String, &str)> {
    let value = value.trim_start_matches(' ');
    if value.starts_with('"') {
        return parse_quoted_git_path(value);
    }
    let end = value.find(' ').unwrap_or(value.len());
    (end > 0).then(|| (value[..end].to_owned(), &value[end..]))
}

fn parse_unquoted_diff_header_paths(value: &str) -> Option<(String, String)> {
    let mut candidate_start = 0;
    while let Some(offset) = value[candidate_start..].find(" b/") {
        let separator = candidate_start + offset;
        let old_path = &value[..separator];
        let new_path = &value[separator + 1..];
        if let (Some(old_relative), Some(new_relative)) =
            (old_path.strip_prefix("a/"), new_path.strip_prefix("b/"))
        {
            if old_relative == new_relative {
                return Some((old_path.to_owned(), new_path.to_owned()));
            }
        }
        candidate_start = separator + 1;
    }
    None
}

fn normalize_git_path(path: &str) -> Option<String> {
    (!path.is_empty() && path != "/dev/null").then(|| {
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_owned()
    })
}

fn parse_quoted_git_path(value: &str) -> Option<(String, &str)> {
    let mut characters = value.strip_prefix('"')?.chars();
    let mut path = String::new();
    let mut consumed = 1;
    while let Some(character) = characters.next() {
        consumed += character.len_utf8();
        match character {
            '"' => return Some((path, &value[consumed..])),
            '\\' => {
                let escaped = characters.next()?;
                consumed += escaped.len_utf8();
                match escaped {
                    '"' | '\\' => path.push(escaped),
                    't' => path.push('\t'),
                    'n' => path.push('\n'),
                    'r' => path.push('\r'),
                    'x' => {
                        let first = characters.next()?;
                        let second = characters.next()?;
                        consumed += first.len_utf8() + second.len_utf8();
                        let hex = format!("{first}{second}");
                        path.push(char::from(u8::from_str_radix(&hex, 16).ok()?));
                    }
                    digit @ '0'..='7' => {
                        let mut octal = String::from(digit);
                        for _ in 0..2 {
                            let next = characters.clone().next();
                            match next {
                                Some(next @ '0'..='7') => {
                                    characters.next();
                                    consumed += next.len_utf8();
                                    octal.push(next);
                                }
                                _ => break,
                            }
                        }
                        path.push(char::from(u8::from_str_radix(&octal, 8).ok()?));
                    }
                    other => path.push(other),
                }
            }
            other => path.push(other),
        }
    }
    None
}

fn render_path(path: &str) -> String {
    if !path
        .chars()
        .any(|character| character.is_control() || matches!(character, '"' | '\\'))
    {
        return path.to_owned();
    }

    let mut rendered = String::from('"');
    for character in path.chars() {
        match character {
            '"' => rendered.push_str("\\\""),
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            character if character.is_control() && (character as u32) <= u8::MAX as u32 => {
                rendered.push_str(&format!("\\x{:02X}", character as u32));
            }
            character => rendered.push(character),
        }
    }
    rendered.push('"');
    rendered
}

fn decode_rendered_path(input: &str) -> Option<String> {
    let (path, remaining) = parse_quoted_git_path(input)?;
    remaining.is_empty().then_some(path)
}

fn parse_rendered_selector(input: &str) -> Option<(String, Option<usize>)> {
    let (path, ordinal) = if input.starts_with('"') {
        let (path, remaining) = parse_quoted_git_path(input)?;
        (path, remaining.strip_prefix("#hunk-")?)
    } else {
        let (path, ordinal) = input.rsplit_once("#hunk-")?;
        (path.to_owned(), ordinal)
    };
    let ordinal = ordinal.parse::<usize>().ok()?;
    (ordinal > 0).then_some((path, Some(ordinal)))
}

#[cfg(feature = "headroom-compression")]
fn render_index(files: &[ReviewDiffFile]) -> String {
    let mut rendered = String::from("review-diff source index\n");
    for file in files {
        rendered.push_str("file: ");
        rendered.push_str(&render_path(&file.path));
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

    #[test]
    fn parser_preserves_spaces_and_discards_marker_timestamps() {
        let parsed = parse_unified_diff(
            "diff --git a/space name.txt b/space name.txt\n\
             --- a/space name.txt\t2026-01-01\n\
             +++ b/space name.txt\t2026-01-01\n\
             @@ -1 +1 @@\n-old\n+new\n",
        );

        assert_eq!(parsed[0].public.path, "space name.txt");
    }
}
