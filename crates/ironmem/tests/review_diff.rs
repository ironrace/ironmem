use std::path::Path;
use std::process::Command;

use ironmem::review_diff::{
    build_review_diff, expand_review_diff, ReviewDiffRequest, ReviewDiffSource,
};
use tempfile::TempDir;

struct DiffFixture {
    tempdir: TempDir,
    base: String,
    head: String,
}

impl DiffFixture {
    fn request(&self) -> ReviewDiffRequest {
        ReviewDiffRequest::range(self.tempdir.path(), &self.base, &self.head)
    }

    fn source(&self) -> String {
        git_output(
            self.tempdir.path(),
            ["diff", "--no-ext-diff", "--unified=3", &format!("{}...{}", self.base, self.head)],
        )
    }
}

fn fixture() -> DiffFixture {
    let tempdir = tempfile::tempdir().expect("temp repo");
    git(tempdir.path(), ["init"]);
    git(tempdir.path(), ["config", "user.email", "review-diff@example.test"]);
    git(tempdir.path(), ["config", "user.name", "Review Diff Test"]);

    for file in ["alpha.txt", "beta.txt"] {
        std::fs::write(tempdir.path().join(file), fixture_contents(file, "base"))
            .expect("write base fixture");
    }
    git(tempdir.path(), ["add", "."]);
    git(tempdir.path(), ["commit", "-m", "base"]);
    let base = git_output(tempdir.path(), ["rev-parse", "HEAD"]).trim().to_owned();

    for file in ["alpha.txt", "beta.txt"] {
        std::fs::write(tempdir.path().join(file), fixture_contents(file, "head"))
            .expect("write head fixture");
    }
    git(tempdir.path(), ["add", "."]);
    git(tempdir.path(), ["commit", "-m", "head"]);
    let head = git_output(tempdir.path(), ["rev-parse", "HEAD"]).trim().to_owned();

    DiffFixture { tempdir, base, head }
}

fn fixture_contents(file: &str, version: &str) -> String {
    (1..=12)
        .flat_map(|ordinal| {
            [
                format!("{file} section {ordinal} context one"),
                format!("{file} section {ordinal} context two"),
                format!("{file} section {ordinal} context three"),
                format!("{file} section {ordinal} {version} changed payload repeated repeated repeated"),
                format!("{file} section {ordinal} context four"),
                format!("{file} section {ordinal} context five"),
                format!("{file} section {ordinal} context six"),
                format!("{file} section {ordinal} separator"),
            ]
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git should start");
    assert!(status.success(), "git fixture setup should succeed");
}

fn git_output(
    repo: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git should start");
    assert!(output.status.success(), "git fixture command should succeed");
    String::from_utf8(output.stdout).expect("fixture git output should be UTF-8")
}

#[cfg(feature = "headroom-compression")]
#[test]
fn compressed_artifact_indexes_every_source_file_and_hunk() {
    let fixture = fixture();
    let artifact = build_review_diff(&fixture.request()).expect("artifact should compress");

    assert!(artifact.metrics.artifact_bytes < artifact.metrics.source_bytes);
    assert_eq!(artifact.files.len(), 2);
    assert_eq!(artifact.files.iter().map(|file| file.hunks.len()).sum::<usize>(), 24);
    assert!(artifact.rendered.contains("alpha.txt"));
    assert!(artifact.rendered.contains("beta.txt"));

    for file in &artifact.files {
        assert!(artifact.rendered.contains(&file.path));
        for hunk in &file.hunks {
            assert_eq!(hunk.selector, format!("{}#hunk-{}", file.path, hunk.ordinal));
            assert!(artifact.rendered.contains(&hunk.header));
            assert!(artifact.rendered.contains(&hunk.selector));
        }
    }
    assert!(artifact.rendered.contains("source_bytes="));
    assert!(artifact.rendered.contains("artifact_estimated_tokens="));
}

#[test]
fn expansions_preserve_original_file_sections_and_selected_hunks() {
    let fixture = fixture();
    let request = fixture.request();
    let source = fixture.source();

    let file = expand_review_diff(&request, "alpha.txt", None).expect("file expansion");
    assert!(file.starts_with("diff --git a/alpha.txt b/alpha.txt\n"));
    assert!(file.contains("alpha.txt section 1 base changed payload"));
    assert!(file.contains("alpha.txt section 12 head changed payload"));
    assert!(source.contains(&file));

    let hunk = expand_review_diff(&request, "beta.txt", Some(7)).expect("hunk expansion");
    assert!(hunk.starts_with("diff --git a/beta.txt b/beta.txt\n"));
    assert!(hunk.contains("@@ "));
    assert!(hunk.contains("beta.txt section 7 base changed payload"));
    assert!(hunk.contains("beta.txt section 7 head changed payload"));
    assert!(!hunk.contains("beta.txt section 8 head changed payload"));
    assert!(source.contains(&hunk));

    assert!(expand_review_diff(&request, "missing.txt", None).is_err());
    assert!(expand_review_diff(&request, "alpha.txt", Some(0)).is_err());
    assert!(expand_review_diff(&request, "alpha.txt", Some(99)).is_err());
}

#[cfg(not(feature = "headroom-compression"))]
#[test]
fn build_is_safely_unavailable_without_the_optional_feature() {
    let fixture = fixture();
    let error = build_review_diff(&fixture.request()).expect_err("feature should be required");
    assert!(error.to_string().contains("headroom-compression"));
}

#[test]
fn worktree_constructor_selects_the_worktree_source() {
    let fixture = fixture();
    let request = ReviewDiffRequest::worktree(fixture.tempdir.path());
    assert!(matches!(request.source, ReviewDiffSource::Worktree));
}
