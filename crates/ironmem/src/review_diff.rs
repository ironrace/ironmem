//! Deterministic Git diff artifacts for bounded review ingestion.

#[cfg(any(feature = "headroom-compression", test))]
use std::collections::HashMap;
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
/// The artifact is the index of every source file and hunk, then the
/// compressed body, then the metrics. The compressor splits and renumbers
/// hunks as it trims context, so each body hunk header names the index hunk
/// it came from: `@@ -8,5 +8,5 @@ [a.txt#hunk-1]` (see `tag_body_hunks`).
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
    let body = tag_body_hunks(&compressed, &parsed);
    let source_bytes = source.len();
    // Rendered from the tagged body, so the size check below counts the tags.
    let rendered = render_with_metrics(&index, &body, source_bytes);
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
    if let Some(path) = decode_rendered_path(input) {
        return (path, None);
    }
    parse_rendered_selector(input).unwrap_or_else(|| (input.to_owned(), None))
}

fn read_source(request: &ReviewDiffRequest) -> Result<String, MemoryError> {
    let mut command = Command::new("git");
    // Git repository and configuration overrides can redirect a command away
    // from `request.repo`. The shared scrub removes the redirecting ones
    // unconditionally, so one set after this line is not inherited either.
    crate::git_env::scrub_git_environment(&mut command);
    command.current_dir(&request.repo);
    command.args(["diff", "--no-ext-diff", "--no-textconv", "--unified=3"]);
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

    lines
        .iter()
        .find_map(|line| {
            line.strip_prefix("rename to ")
                .and_then(parse_marker_path)
                .and_then(|path| normalize_git_path(&path))
        })
        .or_else(|| from_marker("+++ "))
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
    let bytes = value.as_bytes();
    (bytes.first() == Some(&b'"')).then_some(())?;
    let mut index = 1;
    let mut path = Vec::new();
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        match byte {
            b'"' => {
                return String::from_utf8(path)
                    .ok()
                    .map(|path| (path, &value[index..]))
            }
            b'\\' => {
                let escaped = *bytes.get(index)?;
                index += 1;
                match escaped {
                    b'"' | b'\\' => path.push(escaped),
                    b'a' => path.push(0x07),
                    b'b' => path.push(0x08),
                    b'f' => path.push(0x0c),
                    b't' => path.push(b'\t'),
                    b'n' => path.push(b'\n'),
                    b'r' => path.push(b'\r'),
                    b'v' => path.push(0x0b),
                    b'x' => {
                        let first = *bytes.get(index)?;
                        let second = *bytes.get(index + 1)?;
                        index += 2;
                        path.push(hex_byte(first, second)?);
                    }
                    b'0'..=b'7' => {
                        let mut octal = escaped - b'0';
                        for _ in 0..2 {
                            match bytes.get(index) {
                                Some(next @ b'0'..=b'7') => {
                                    octal = octal.checked_mul(8)?.checked_add(*next - b'0')?;
                                    index += 1;
                                }
                                _ => break,
                            }
                        }
                        path.push(octal);
                    }
                    other => path.push(other),
                }
            }
            other => path.push(other),
        }
    }
    None
}

fn hex_byte(first: u8, second: u8) -> Option<u8> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    digit(first)?.checked_mul(16)?.checked_add(digit(second)?)
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

/// Appends to each hunk header of a compressed `body` the index selector of
/// the source hunk it was cut from: `@@ -8,5 +8,5 @@ [a.txt#hunk-1]`, with any
/// section text Git wrote after the closing `@@` following the tag.
///
/// The compressor splits a source hunk wherever it drops context and numbers
/// each piece like `git diff -U2` would, so a body header matches no index
/// header and the Nth body hunk of a file is not index hunk N. The tag is
/// the selector that expansion accepts for the hunk a body hunk came from.
///
/// A header is tagged only when exactly one source hunk of the file section
/// it sits in holds the body hunk's lines at the body hunk's line numbers.
/// The file is the one whose `diff --git` line opens the section, a line the
/// compressor writes as it read it. Nothing under that line names the file:
/// the compressor does not read a quoted `diff --git "a/..."` line as a file
/// header, so it prints that file's `rename` lines in the section before it
/// and can leave that file's hunks there too, where they fit the wrong
/// file's line numbers. Everything else is returned byte for byte, so an
/// untagged header means "no source hunk found", never a guess.
///
/// Combined-diff headers (`@@@`) and `diff --cc` / `diff --combined` file
/// sections are left untagged: the source index parses neither, so there is
/// no selector to give them. `git diff <base>...<head>` and `git diff HEAD`
/// do not write them.
///
/// The work is linear in the size of `body` plus `source`: each source file
/// is read once, when its section opens, and each body hunk is compared with
/// source lines once per source hunk that reaches its first line's numbers.
#[cfg(any(feature = "headroom-compression", test))]
fn tag_body_hunks(body: &str, source: &[ParsedFile]) -> String {
    let lines = body.split('\n').collect::<Vec<_>>();
    let files = taggable_files(source);
    let mut tagged = String::with_capacity(body.len());
    // The source hunks of the file section being read; none before the first
    // section and in a section with no file to tag from.
    let mut section = SectionHunks::default();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            tagged.push('\n');
        }
        if is_body_section_start(line) {
            section = files
                .get(line)
                .map_or_else(SectionHunks::default, |file| SectionHunks::read(file));
        }
        match section.tag(&lines[index..]) {
            Some((selector, ranges_end)) => {
                tagged.push_str(&line[..ranges_end]);
                tagged.push_str(" [");
                tagged.push_str(selector);
                tagged.push(']');
                tagged.push_str(&line[ranges_end..]);
            }
            None => tagged.push_str(line),
        }
    }
    tagged
}

#[cfg(any(feature = "headroom-compression", test))]
fn is_body_section_start(line: &str) -> bool {
    ["diff --git ", "diff --cc ", "diff --combined "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

/// The indexed files a body file section can be tagged from, by the
/// `diff --git` line that opens their section in the source.
///
/// A file is left out when another has its `diff --git` line or its path. A
/// type change is a deletion and a creation of one path under one such line;
/// both index entries render the same selectors, and expansion opens the
/// first. A combined section's line is not a `diff --git` line, so it finds
/// no file here.
#[cfg(any(feature = "headroom-compression", test))]
fn taggable_files(source: &[ParsedFile]) -> HashMap<&str, &ParsedFile> {
    let mut paths = HashMap::<&str, usize>::new();
    let mut diff_lines = HashMap::<&str, usize>::new();
    for file in source {
        *paths.entry(file.public.path.as_str()).or_default() += 1;
        *diff_lines.entry(diff_line(file)).or_default() += 1;
    }
    source
        .iter()
        .filter(|file| paths[file.public.path.as_str()] == 1 && diff_lines[diff_line(file)] == 1)
        .map(|file| (diff_line(file), file))
        .collect()
}

/// The line that opens a file's section in the source, as the compressor
/// reads it: without its newline.
#[cfg(any(feature = "headroom-compression", test))]
fn diff_line(file: &ParsedFile) -> &str {
    file.section.split('\n').next().unwrap_or_default()
}

/// The source hunks of the indexed file a body file section belongs to, read
/// once for every body hunk of the section.
#[cfg(any(feature = "headroom-compression", test))]
#[derive(Default)]
struct SectionHunks<'a> {
    hunks: Vec<SourceHunk<'a>>,
    /// Where the source reaches each pair of old and new line numbers: the
    /// hunk, by its place in `hunks`, and the place in its `lines` of the
    /// line that starts there. Hunks that Git wrote never share a pair.
    starts: HashMap<(usize, usize), Vec<(usize, usize)>>,
}

#[cfg(any(feature = "headroom-compression", test))]
struct SourceHunk<'a> {
    ranges: HunkRanges,
    selector: &'a str,
    /// The text after the header line, split as a body is.
    lines: Vec<&'a str>,
}

#[cfg(any(feature = "headroom-compression", test))]
impl<'a> SectionHunks<'a> {
    fn read(file: &'a ParsedFile) -> Self {
        let mut section = Self::default();
        for hunk in &file.hunks {
            let Some((ranges, _)) = parse_hunk_header(&hunk.public.header) else {
                continue;
            };
            let lines = hunk.source.split('\n').skip(1).collect::<Vec<_>>();
            let (mut old, mut new) = (ranges.old_start, ranges.new_start);
            for (index, line) in lines.iter().enumerate() {
                // Nothing after a line that is not hunk content is numbered.
                let Some(step) = hunk_line_advance(line) else {
                    break;
                };
                // A `\` line is on neither side, so the line after it starts
                // at the same numbers; `tag` steps back over those.
                if step != (0, 0) {
                    section
                        .starts
                        .entry((old, new))
                        .or_default()
                        .push((section.hunks.len(), index));
                }
                old = old.saturating_add(step.0);
                new = new.saturating_add(step.1);
            }
            section.hunks.push(SourceHunk {
                ranges,
                selector: &hunk.public.selector,
                lines,
            });
        }
        section
    }

    /// The selector to tag the body hunk header at `lines[0]` with, and the
    /// byte offset of the end of its closing `@@`, where the tag goes. `None`
    /// unless exactly one source hunk holds the body hunk: its header's
    /// ranges contain the body hunk's, and its text has the body hunk's
    /// lines at the body hunk's line numbers.
    fn tag(&self, lines: &[&str]) -> Option<(&'a str, usize)> {
        let (ranges, ranges_end) = parse_hunk_header(lines.first()?)?;
        let body = hunk_lines(&lines[1..], ranges)?;
        // `hunk_lines` ends on a line that is on a side, so `body` has one
        // and the `\` lines before its first share that line's numbers.
        let markers = body
            .iter()
            .take_while(|line| line.starts_with('\\'))
            .count();
        let mut holding = self
            .starts
            .get(&(ranges.old_start, ranges.new_start))?
            .iter()
            .filter(|&&(hunk, line)| {
                let hunk = &self.hunks[hunk];
                hunk.ranges.contains(ranges)
                    && line
                        .checked_sub(markers)
                        .is_some_and(|start| hunk.lines[start..].starts_with(body))
            })
            .map(|&(hunk, _)| hunk);
        let hunk = holding.next()?;
        // One hunk can hold the lines at two places only where its line
        // numbers have stopped counting, at `usize::MAX`.
        holding
            .all(|other| other == hunk)
            .then_some((self.hunks[hunk].selector, ranges_end))
    }
}

/// The lines a two-way hunk header covers on each side. `start` is the first
/// line covered; an empty range starts at the line an insertion there would
/// become, one past the line Git names for it (`-12,0` starts at 13).
#[cfg(any(feature = "headroom-compression", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkRanges {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
}

#[cfg(any(feature = "headroom-compression", test))]
impl HunkRanges {
    /// Whether both of `inner`'s ranges lie within this hunk's. An empty
    /// range lies within when the gap it names does, either end included.
    fn contains(self, inner: Self) -> bool {
        let within = |start: usize, len: usize, inner_start: usize, inner_len: usize| {
            start <= inner_start
                && inner_start.saturating_add(inner_len) <= start.saturating_add(len)
        };
        within(self.old_start, self.old_len, inner.old_start, inner.old_len)
            && within(self.new_start, self.new_len, inner.new_start, inner.new_len)
    }
}

/// Parses `@@ -a[,b] +c[,d] @@`, with or without section text, into its
/// ranges and the byte offset just past the closing `@@`. A combined-diff
/// header (`@@@`) is not this shape and returns `None`.
#[cfg(any(feature = "headroom-compression", test))]
fn parse_hunk_header(line: &str) -> Option<(HunkRanges, usize)> {
    let (old, rest) = line.strip_prefix("@@ -")?.split_once(" +")?;
    let (new, section_text) = rest.split_once(" @@")?;
    // Git writes the end of the line or a space after the closing `@@`.
    if !(section_text.is_empty() || section_text.starts_with(' ')) {
        return None;
    }
    let (old_start, old_len) = parse_hunk_range(old)?;
    let (new_start, new_len) = parse_hunk_range(new)?;
    Some((
        HunkRanges {
            old_start,
            old_len,
            new_start,
            new_len,
        },
        line.len() - section_text.len(),
    ))
}

#[cfg(any(feature = "headroom-compression", test))]
fn parse_hunk_range(range: &str) -> Option<(usize, usize)> {
    // Digits only: `str::parse` would also take a leading `+`.
    let number = |digits: &str| {
        digits
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| digits.parse::<usize>().ok())?
    };
    let (start, len) = match range.split_once(',') {
        Some((start, len)) => (number(start)?, number(len)?),
        None => (number(range)?, 1),
    };
    // Git names the line before an empty range.
    let start = if len == 0 {
        start.checked_add(1)?
    } else {
        start
    };
    Some((start, len))
}

/// How one hunk line moves the old-side and new-side line numbers. `None`
/// for a line that is not hunk content.
#[cfg(any(feature = "headroom-compression", test))]
fn hunk_line_advance(line: &str) -> Option<(usize, usize)> {
    match line.as_bytes().first() {
        // Git writes an empty context line as a lone space, or as nothing
        // under `diff.suppressBlankEmpty`.
        Some(b' ') | None => Some((1, 1)),
        Some(b'-') => Some((1, 0)),
        Some(b'+') => Some((0, 1)),
        // `\ No newline at end of file` is on neither side.
        Some(b'\\') => Some((0, 0)),
        Some(_) => None,
    }
}

/// The leading `lines` that fill the old and new line counts of `ranges`.
/// `None` when they run out or stop being hunk content first, and for a hunk
/// of no lines, which would hold nothing to compare.
#[cfg(any(feature = "headroom-compression", test))]
fn hunk_lines<'a, 'b>(lines: &'a [&'b str], ranges: HunkRanges) -> Option<&'a [&'b str]> {
    let (mut old, mut new, mut taken) = (0, 0, 0);
    while old < ranges.old_len || new < ranges.new_len {
        let (old_step, new_step) = hunk_line_advance(lines.get(taken)?)?;
        old += old_step;
        new += new_step;
        taken += 1;
    }
    (taken > 0 && old == ranges.old_len && new == ranges.new_len).then(|| &lines[..taken])
}

#[cfg(test)]
mod tests {
    use super::{estimated_tokens, expand_parsed, parse_unified_diff, tag_body_hunks};

    /// A hunk over lines `first..=last` of a file whose line N reads
    /// `{text} line N`, replacing each line in `changed`. Every replacement
    /// is one line for one, so both sides share a start and a length.
    fn hunk(text: &str, first: usize, last: usize, changed: &[usize]) -> String {
        let length = last - first + 1;
        let mut hunk = format!("@@ -{first},{length} +{first},{length} @@\n");
        for line in first..=last {
            if changed.contains(&line) {
                hunk.push_str(&format!(
                    "-{text} line {line} old\n+{text} line {line} new\n"
                ));
            } else {
                hunk.push_str(&format!(" {text} line {line}\n"));
            }
        }
        hunk
    }

    fn file_section(path: &str, hunks: &[String]) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{}",
            hunks.concat()
        )
    }

    fn hunk_headers(diff: &str) -> Vec<&str> {
        diff.split('\n')
            .filter(|line| line.starts_with('@'))
            .collect()
    }

    #[test]
    fn body_hunks_split_from_one_source_hunk_carry_its_selector() {
        // Git's three lines of context join the changes at lines 4 and 10
        // into one hunk; trimming to two lines splits them again.
        let source = file_section(
            "a.txt",
            &[hunk("a", 1, 13, &[4, 10]), hunk("a", 40, 46, &[43])],
        );
        let parsed = parse_unified_diff(&source);
        let body = file_section(
            "a.txt",
            &[
                hunk("a", 2, 6, &[4]),
                hunk("a", 8, 12, &[10]),
                hunk("a", 41, 45, &[43]),
            ],
        ) + "[1 files changed, +3 -3 lines]";

        let tagged = tag_body_hunks(&body, &parsed);

        // The third body hunk is the second source hunk, not the third.
        assert_eq!(
            hunk_headers(&tagged),
            [
                "@@ -2,5 +2,5 @@ [a.txt#hunk-1]",
                "@@ -8,5 +8,5 @@ [a.txt#hunk-1]",
                "@@ -41,5 +41,5 @@ [a.txt#hunk-2]",
            ]
        );
        let first = expand_parsed(&parsed, "a.txt#hunk-1", None).expect("first source hunk");
        for changed in ["-a line 4 old", "+a line 4 new", "+a line 10 new"] {
            assert!(first.contains(changed), "{changed} not in {first}");
        }
        assert!(!first.contains("a line 43"));
        let second = expand_parsed(&parsed, "a.txt", Some(2)).expect("second source hunk");
        assert!(second.contains("-a line 43 old\n+a line 43 new\n"));
        // Only hunk headers change.
        let untagged = tagged
            .replace(" [a.txt#hunk-1]", "")
            .replace(" [a.txt#hunk-2]", "");
        assert_eq!(untagged, body);
    }

    #[test]
    fn body_hunk_tags_name_the_file_section_they_sit_in() {
        // Both files change the same lines to the same text, so only the
        // file section tells their hunks apart.
        let hunks = [hunk("same", 1, 7, &[4]), hunk("same", 20, 26, &[23])];
        let source = file_section("a.txt", &hunks) + &file_section("b.txt", &hunks);
        let parsed = parse_unified_diff(&source);
        let trimmed = [hunk("same", 2, 6, &[4]), hunk("same", 21, 25, &[23])];
        // The compressor may reorder files; the index order must not matter.
        let body = file_section("b.txt", &trimmed) + &file_section("a.txt", &trimmed);

        let tagged = tag_body_hunks(&body, &parsed);

        assert_eq!(
            hunk_headers(&tagged),
            [
                "@@ -2,5 +2,5 @@ [b.txt#hunk-1]",
                "@@ -21,5 +21,5 @@ [b.txt#hunk-2]",
                "@@ -2,5 +2,5 @@ [a.txt#hunk-1]",
                "@@ -21,5 +21,5 @@ [a.txt#hunk-2]",
            ]
        );
    }

    #[test]
    fn body_hunks_without_a_containing_source_hunk_stay_untagged() {
        let source = file_section("a.txt", &[hunk("a", 1, 7, &[4])]);
        let parsed = parse_unified_diff(&source);

        for (case, body) in [
            (
                "line numbers outside every source hunk",
                file_section("a.txt", &[hunk("a", 30, 34, &[32])]),
            ),
            (
                "line numbers that run past the source hunk",
                file_section("a.txt", &[hunk("a", 5, 9, &[6])]),
            ),
            (
                // What the compressor emits when it files another file's
                // hunk under this section: the numbers fit, the text is not
                // this file's.
                "another file's text at line numbers inside a source hunk",
                file_section("a.txt", &[hunk("other", 2, 6, &[4])]),
            ),
            (
                "a file the index does not list",
                file_section("missing.txt", &[hunk("a", 2, 6, &[4])]),
            ),
            ("a hunk before any file section", hunk("a", 2, 6, &[4])),
            (
                "a header whose counts its lines do not fill",
                file_section("a.txt", &["@@ -2,5 +2,5 @@\n a line 2\n".to_owned()]),
            ),
        ] {
            assert_eq!(tag_body_hunks(&body, &parsed), body, "{case}");
        }
        // The same cases must tag once the hunk does belong to the file.
        let tagged = tag_body_hunks(&file_section("a.txt", &[hunk("a", 2, 6, &[4])]), &parsed);
        assert_eq!(hunk_headers(&tagged), ["@@ -2,5 +2,5 @@ [a.txt#hunk-1]"]);
    }

    #[test]
    fn body_hunks_of_a_path_the_index_lists_twice_stay_untagged() {
        // A type change is a deletion and a creation of one path. Both index
        // entries render `a.txt#hunk-1`, and expansion opens the first.
        let source = "diff --git a/a.txt b/a.txt\ndeleted file mode 100644\n--- a/a.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n\
                      diff --git a/a.txt b/a.txt\nnew file mode 120000\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+new\n";
        let parsed = parse_unified_diff(source);
        assert_eq!(parsed.len(), 2);

        assert_eq!(tag_body_hunks(source, &parsed), source);
    }

    /// The preamble Git writes for a renamed file whose path it quotes.
    const QUOTED_RENAME: &str = "diff --git \"a/docs/r\\303\\251sum\\303\\251.md\" \"b/docs/r\\303\\251sum\\303\\251-old.md\"\n\
                                 similarity index 90%\n\
                                 rename from \"docs/r\\303\\251sum\\303\\251.md\"\n\
                                 rename to \"docs/r\\303\\251sum\\303\\251-old.md\"\n\
                                 --- \"a/docs/r\\303\\251sum\\303\\251.md\"\n\
                                 +++ \"b/docs/r\\303\\251sum\\303\\251-old.md\"\n";

    #[test]
    fn a_body_section_is_matched_by_its_diff_line_not_by_rename_lines_inside_it() {
        let own = [hunk("a", 1, 13, &[4, 10]), hunk("a", 40, 46, &[43])];
        let source = file_section("docs/a.md", &own) + QUOTED_RENAME + &hunk("r", 20, 26, &[23]);
        let parsed = parse_unified_diff(&source);
        assert_eq!(
            parsed
                .iter()
                .map(|file| file.public.path.as_str())
                .collect::<Vec<_>>(),
            ["docs/a.md", "docs/résumé-old.md"]
        );

        // The compressor does not read the quoted `diff --git` line as a file
        // header. It prints the renamed file's `similarity` and `rename`
        // lines right after the `diff --git` line of the file before it, and
        // past its hunk cap it sorts the renamed file's hunk in among that
        // file's own.
        let rename_lines = QUOTED_RENAME
            .lines()
            .filter(|line| line.starts_with("similarity ") || line.starts_with("rename "))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let body = format!(
            "diff --git a/docs/a.md b/docs/a.md\n{rename_lines}--- a/docs/a.md\n+++ b/docs/a.md\n{}{}{}{}",
            hunk("a", 2, 6, &[4]),
            hunk("a", 8, 12, &[10]),
            hunk("r", 21, 25, &[23]),
            hunk("a", 41, 45, &[43]),
        );

        assert_eq!(
            hunk_headers(&tag_body_hunks(&body, &parsed)),
            [
                "@@ -2,5 +2,5 @@ [docs/a.md#hunk-1]",
                "@@ -8,5 +8,5 @@ [docs/a.md#hunk-1]",
                // The renamed file's hunk, under a section that is not its own.
                "@@ -21,5 +21,5 @@",
                "@@ -41,5 +41,5 @@ [docs/a.md#hunk-2]",
            ]
        );

        // Under the cap the compressor keeps the quoted `diff --git` line,
        // as a line of the hunk before it. The renamed file's hunks then
        // follow their own section line and carry their own selector.
        let body = format!(
            "diff --git a/docs/a.md b/docs/a.md\n{rename_lines}--- a/docs/a.md\n+++ b/docs/a.md\n{}{}{QUOTED_RENAME}{}",
            hunk("a", 2, 6, &[4]),
            own[1],
            hunk("r", 21, 25, &[23]),
        );

        assert_eq!(
            hunk_headers(&tag_body_hunks(&body, &parsed)),
            [
                "@@ -2,5 +2,5 @@ [docs/a.md#hunk-1]",
                "@@ -40,7 +40,7 @@ [docs/a.md#hunk-2]",
                "@@ -21,5 +21,5 @@ [docs/résumé-old.md#hunk-1]",
            ]
        );
    }

    #[test]
    fn body_hunks_of_one_path_under_two_diff_lines_stay_untagged() {
        // Two index entries render `new.txt#hunk-1` and expansion opens the
        // first, so neither section's hunk may claim that selector, though
        // each `diff --git` line is in the index once.
        let renamed = "diff --git a/old.txt b/new.txt\nrename from old.txt\nrename to new.txt\n--- a/old.txt\n+++ b/new.txt\n"
            .to_owned()
            + &hunk("renamed", 1, 7, &[4]);
        let source = renamed + &file_section("new.txt", &[hunk("plain", 1, 7, &[4])]);
        let parsed = parse_unified_diff(&source);
        assert_eq!(
            parsed
                .iter()
                .map(|file| file.public.hunks[0].selector.as_str())
                .collect::<Vec<_>>(),
            ["new.txt#hunk-1", "new.txt#hunk-1"]
        );

        assert_eq!(tag_body_hunks(&source, &parsed), source);
    }

    #[test]
    fn body_hunks_under_a_diff_line_two_index_files_open_with_stay_untagged() {
        // Git does not write this: one `diff --git` line over two paths.
        // The line then names neither file, and the hunk is in both.
        let section = |path: &str| {
            format!(
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/{path}\n{}",
                hunk("same", 1, 7, &[4])
            )
        };
        let parsed = parse_unified_diff(&(section("one.txt") + &section("two.txt")));
        assert_eq!(
            parsed
                .iter()
                .map(|file| file.public.path.as_str())
                .collect::<Vec<_>>(),
            ["one.txt", "two.txt"]
        );
        let body = section("one.txt");

        assert_eq!(tag_body_hunks(&body, &parsed), body);
    }

    #[test]
    fn a_body_hunk_that_opens_on_a_no_newline_marker_is_tagged() {
        // The marker is on neither side, so the body hunk starts at the
        // numbers of the line after it: old line 3, new line 2.
        let source = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
                      @@ -1,3 +1,3 @@\n one\n-two\n\\ No newline at end of file\n+TWO\n three\n";
        let parsed = parse_unified_diff(source);
        let body = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
                    @@ -3 +2,2 @@\n\\ No newline at end of file\n+TWO\n three\n";

        assert_eq!(
            hunk_headers(&tag_body_hunks(body, &parsed)),
            ["@@ -3 +2,2 @@ [a.txt#hunk-1]"]
        );
        // The same lines after a different marker are not the source's.
        let other = body.replace("No newline at end of file", "Some other marker");
        assert_eq!(tag_body_hunks(&other, &parsed), other);
    }

    #[test]
    fn source_text_after_a_line_that_is_not_hunk_content_gives_no_tag() {
        let source = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
                      @@ -1,4 +1,4 @@\n one\nnot a hunk line\n two\n-three\n+THREE\n four\n";
        let parsed = parse_unified_diff(source);
        let section =
            |hunk: &str| format!("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n{hunk}");
        let after = section("@@ -2,3 +2,3 @@\n two\n-three\n+THREE\n four\n");

        assert_eq!(tag_body_hunks(&after, &parsed), after);
        assert_eq!(
            hunk_headers(&tag_body_hunks(&section("@@ -1 +1 @@\n one\n"), &parsed)),
            ["@@ -1 +1 @@ [a.txt#hunk-1]"]
        );
    }

    #[test]
    fn a_body_hunk_one_source_hunk_holds_at_two_places_is_tagged() {
        // Line numbers stop counting at `usize::MAX`, so both source lines
        // start at the same numbers and each holds the one-line body hunk.
        // That is still one source hunk.
        let last = usize::MAX;
        let section =
            |hunk: String| format!("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n{hunk}");
        let parsed = parse_unified_diff(&section(format!(
            "@@ -{last},2 +{last},2 @@\n same\n same\n"
        )));
        let body = section(format!("@@ -{last} +{last} @@\n same\n"));

        assert_eq!(
            hunk_headers(&tag_body_hunks(&body, &parsed)),
            [format!("@@ -{last} +{last} @@ [a.txt#hunk-1]").as_str()]
        );
    }

    #[test]
    fn a_body_hunk_claiming_a_blank_line_after_its_source_hunk_stays_untagged() {
        let source = file_section("a.txt", &[hunk("a", 1, 7, &[4])]);
        let parsed = parse_unified_diff(&source);
        // Under `diff.suppressBlankEmpty` a blank context line is an empty
        // line. This body claims one as line 8, after the last line of the
        // source hunk, where the source text has only its final newline.
        let body = file_section(
            "a.txt",
            &["@@ -3,6 +3,6 @@\n a line 3\n-a line 4 old\n+a line 4 new\n a line 5\n a line 6\n a line 7\n\n".to_owned()],
        );

        assert_eq!(tag_body_hunks(&body, &parsed), body);
        // Without the claimed line 8 the same hunk is the source hunk's.
        let within = file_section("a.txt", &[hunk("a", 3, 7, &[4])]);
        assert_eq!(
            hunk_headers(&tag_body_hunks(&within, &parsed)),
            ["@@ -3,5 +3,5 @@ [a.txt#hunk-1]"]
        );
    }

    #[test]
    fn a_body_hunk_past_the_lines_its_source_header_counts_stays_untagged() {
        // A source header that counts three lines a side, over text that
        // runs on for two more: lines 4 and 5 are not in the hunk the index
        // lists, whatever follows it in the source.
        let understated = file_section(
            "a.txt",
            &["@@ -1,3 +1,3 @@\n a line 1\n-a line 2 old\n+a line 2 new\n a line 3\n a line 4\n a line 5\n".to_owned()],
        );
        let parsed = parse_unified_diff(&understated);
        let body = file_section(
            "a.txt",
            &["@@ -4,2 +4,2 @@\n a line 4\n a line 5\n".to_owned()],
        );

        assert_eq!(tag_body_hunks(&body, &parsed), body);
        let within = file_section(
            "a.txt",
            &["@@ -2,2 +2,2 @@\n-a line 2 old\n+a line 2 new\n a line 3\n".to_owned()],
        );
        assert_eq!(
            hunk_headers(&tag_body_hunks(&within, &parsed)),
            ["@@ -2,2 +2,2 @@ [a.txt#hunk-1]"]
        );
    }

    #[test]
    fn every_piece_of_one_large_source_hunk_is_tagged_from_a_single_read_of_it() {
        // A regenerated lockfile: one source hunk the compressor cuts into
        // thousands of pieces. Piece `p` keeps lines `6p+1..=6p+5`, whose
        // third line changed; the compressor dropped line `6p+6`.
        //
        // Sized so that reading the source hunk again for each piece is
        // 20,000 passes over 120,000 lines: that took this test 109 seconds
        // in a debug build, against a fraction of a second for one pass. No
        // time is asserted; a test that runs for minutes is the signal.
        const PIECES: usize = 20_000;
        let lines = PIECES * 6;
        let mut source_hunk = format!("@@ -1,{lines} +1,{lines} @@\n");
        let mut pieces = String::new();
        for piece in 0..PIECES {
            let first = piece * 6 + 1;
            pieces.push_str(&format!("@@ -{first},5 +{first},5 @@\n"));
            for line in first..first + 6 {
                let text = if line == first + 2 {
                    format!("-lock line {line} old\n+lock line {line} new\n")
                } else {
                    format!(" lock line {line}\n")
                };
                source_hunk.push_str(&text);
                if line < first + 5 {
                    pieces.push_str(&text);
                }
            }
        }
        let parsed = parse_unified_diff(&file_section("Cargo.lock", &[source_hunk]));
        assert_eq!(parsed[0].public.hunks.len(), 1);
        let body = file_section("Cargo.lock", &[pieces]);

        let tagged = tag_body_hunks(&body, &parsed);

        let headers = hunk_headers(&tagged);
        assert_eq!(headers.len(), PIECES);
        assert_eq!(headers[0], "@@ -1,5 +1,5 @@ [Cargo.lock#hunk-1]");
        assert_eq!(
            headers[PIECES - 1],
            format!("@@ -{0},5 +{0},5 @@ [Cargo.lock#hunk-1]", lines - 5)
        );
        assert!(headers
            .iter()
            .all(|header| header.ends_with(" @@ [Cargo.lock#hunk-1]")));
        assert_eq!(tagged.replace(" [Cargo.lock#hunk-1]", ""), body);
    }

    #[test]
    fn a_body_hunk_two_source_hunks_hold_stays_untagged() {
        // Git never writes overlapping hunks; a diff that does gives a body
        // hunk two candidate selectors and no way to choose.
        let source = file_section("a.txt", &[hunk("a", 1, 7, &[4]), hunk("a", 1, 7, &[4])]);
        let parsed = parse_unified_diff(&source);
        assert_eq!(parsed[0].public.hunks.len(), 2);
        let body = file_section("a.txt", &[hunk("a", 2, 6, &[4])]);

        assert_eq!(tag_body_hunks(&body, &parsed), body);
    }

    #[test]
    fn body_hunk_tags_keep_the_escaped_selector_of_a_quoted_path() {
        for (header_path, selector, path) in [
            (
                r#""a/line\nbreak.txt" "b/line\nbreak.txt""#,
                r##""line\nbreak.txt"#hunk-1"##,
                "line\nbreak.txt",
            ),
            (
                r#""a/space name\tfile.txt" "b/space name\tfile.txt""#,
                r##""space name\tfile.txt"#hunk-1"##,
                "space name\tfile.txt",
            ),
            (
                r#""a/quote\"back\\slash.txt" "b/quote\"back\\slash.txt""#,
                r##""quote\"back\\slash.txt"#hunk-1"##,
                "quote\"back\\slash.txt",
            ),
            // A path Git does not quote can still hold the tag's own
            // delimiters; the tag is the index selector, byte for byte.
            (
                "a/odd] [name.txt b/odd] [name.txt",
                "odd] [name.txt#hunk-1",
                "odd] [name.txt",
            ),
        ] {
            let (old, new) = header_path.split_at(header_path.len() / 2);
            let section = |hunk: String| {
                format!(
                    "diff --git {header_path}\n--- {}\n+++ {}\n{hunk}",
                    old.trim(),
                    new.trim()
                )
            };
            let parsed = parse_unified_diff(&section(hunk("quoted", 1, 13, &[4, 10])));
            assert_eq!(parsed[0].public.path, path);
            assert_eq!(parsed[0].public.hunks[0].selector, selector);

            let tagged = tag_body_hunks(&section(hunk("quoted", 2, 6, &[4])), &parsed);

            let header = format!("@@ -2,5 +2,5 @@ [{selector}]");
            assert_eq!(hunk_headers(&tagged), [header.as_str()]);
            // The tag must stay on one line and open the hunk it names.
            let expanded = expand_parsed(&parsed, selector, None).expect("selector expansion");
            assert!(expanded.contains("-quoted line 4 old\n+quoted line 4 new\n"));
        }
    }

    #[test]
    fn combined_diff_hunk_headers_pass_through_untagged() {
        let source = file_section("a.txt", &[hunk("a", 1, 7, &[4])]);
        let parsed = parse_unified_diff(&source);
        let ordinary = file_section("a.txt", &[hunk("a", 2, 6, &[4])]);
        let combined = "diff --cc a.txt\nindex 1111111,2222222..3333333\n--- a/a.txt\n+++ b/a.txt\n\
                        @@@ -2,5 -2,5 +2,5 @@@\n  a line 2\n  a line 3\n- a line 4 old\n -a line 4 old\n++a line 4 new\n  a line 5\n  a line 6\n\
                        @@ -2,5 +2,5 @@\n a line 2\n a line 3\n-a line 4 old\n+a line 4 new\n a line 5\n a line 6\n";

        let tagged_ordinary = tag_body_hunks(&ordinary, &parsed);
        let tagged = tag_body_hunks(&format!("{ordinary}{combined}"), &parsed);

        // The index parses neither `diff --cc` nor `@@@`, so a combined file
        // section passes through whole: its `@@@` header, and a two-way
        // header that would have matched in the section before it.
        assert_eq!(
            hunk_headers(&tagged_ordinary),
            ["@@ -2,5 +2,5 @@ [a.txt#hunk-1]"]
        );
        assert_eq!(tagged, format!("{tagged_ordinary}{combined}"));

        // A combined header inside an ordinary section is skipped; the
        // two-way hunk after it is still tagged.
        let mixed = file_section(
            "a.txt",
            &[
                "@@@ -2,5 -2,5 +2,5 @@@\n  a line 2\n".to_owned(),
                hunk("a", 2, 6, &[4]),
            ],
        );
        assert_eq!(
            hunk_headers(&tag_body_hunks(&mixed, &parsed)),
            ["@@@ -2,5 -2,5 +2,5 @@@", "@@ -2,5 +2,5 @@ [a.txt#hunk-1]"]
        );
    }

    #[test]
    fn body_hunk_tags_read_short_and_empty_ranges_and_precede_section_text() {
        // A new file, a deleted file, and a one-line file in the short form.
        let source = "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n\
                      diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n\
                      diff --git a/tiny.txt b/tiny.txt\n--- a/tiny.txt\n+++ b/tiny.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let parsed = parse_unified_diff(source);

        // The body equals the source, as when the compressor trims nothing,
        // so each hunk maps to itself and the trailing newline survives.
        let tagged = tag_body_hunks(source, &parsed);

        assert_eq!(
            hunk_headers(&tagged),
            [
                "@@ -0,0 +1,2 @@ [new.txt#hunk-1]",
                "@@ -1,2 +0,0 @@ [gone.txt#hunk-1]",
                "@@ -1 +1 @@ [tiny.txt#hunk-1]",
            ]
        );
        assert!(tagged.ends_with("-old\n+new\n"));

        // An insertion split out of a larger hunk: `-9,0` is the gap after
        // old line 9, inside the source hunk's old lines 7..=12.
        let source = "diff --git a/fn.rs b/fn.rs\n--- a/fn.rs\n+++ b/fn.rs\n\
                      @@ -7,6 +7,7 @@ fn main() {\n l7\n l8\n l9\n+added\n l10\n l11\n l12\n";
        let parsed = parse_unified_diff(source);
        let body = "diff --git a/fn.rs b/fn.rs\n--- a/fn.rs\n+++ b/fn.rs\n\
                    @@ -9,0 +10 @@ fn main() {\n+added";

        assert_eq!(
            tag_body_hunks(body, &parsed),
            "diff --git a/fn.rs b/fn.rs\n--- a/fn.rs\n+++ b/fn.rs\n\
             @@ -9,0 +10 @@ [fn.rs#hunk-1] fn main() {\n+added"
        );
    }

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

    #[test]
    fn parser_decodes_all_git_c_style_control_escapes() {
        let parsed = parse_unified_diff(
            r#"diff --git "a/bell\a back\b form\f vertical\v.txt" "b/bell\a back\b form\f vertical\v.txt"
--- "a/bell\a back\b form\f vertical\v.txt"
+++ "b/bell\a back\b form\f vertical\v.txt"
@@ -1 +1 @@
-old
+new
"#,
        );

        assert_eq!(
            parsed[0].public.path,
            "bell\u{7} back\u{8} form\u{c} vertical\u{b}.txt"
        );
    }

    #[test]
    fn parser_uses_new_path_for_unquoted_header_only_rename() {
        let parsed = parse_unified_diff(
            "diff --git a/old name.txt b/new name.txt\nsimilarity index 100%\nrename from old name.txt\nrename to new name.txt\n",
        );

        assert_eq!(parsed[0].public.path, "new name.txt");
    }

    #[test]
    fn parser_decodes_utf8_octal_bytes_as_one_character() {
        let parsed = parse_unified_diff(
            r#"diff --git "a/\303\251.txt" "b/\303\251.txt"
--- "a/\303\251.txt"
+++ "b/\303\251.txt"
@@ -1 +1 @@
-old
+new
"#,
        );

        assert_eq!(parsed[0].public.path, "é.txt");
    }
}
