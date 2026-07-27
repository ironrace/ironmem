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
            [
                "diff",
                "--no-ext-diff",
                "--unified=3",
                &format!("{}...{}", self.base, self.head),
            ],
        )
    }
}

fn fixture() -> DiffFixture {
    let tempdir = tempfile::tempdir().expect("temp repo");
    git(tempdir.path(), ["init"]);
    git(
        tempdir.path(),
        ["config", "user.email", "review-diff@example.test"],
    );
    git(tempdir.path(), ["config", "user.name", "Review Diff Test"]);

    for file in ["alpha.txt", "beta.txt"] {
        std::fs::write(tempdir.path().join(file), fixture_contents(file, "base"))
            .expect("write base fixture");
    }
    git(tempdir.path(), ["add", "."]);
    git(tempdir.path(), ["commit", "-m", "base"]);
    let base = git_output(tempdir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    for file in ["alpha.txt", "beta.txt"] {
        std::fs::write(tempdir.path().join(file), fixture_contents(file, "head"))
            .expect("write head fixture");
    }
    git(tempdir.path(), ["add", "."]);
    git(tempdir.path(), ["commit", "-m", "head"]);
    let head = git_output(tempdir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    DiffFixture {
        tempdir,
        base,
        head,
    }
}

fn fixture_contents(file: &str, version: &str) -> String {
    (1..=12)
        .flat_map(|ordinal| {
            [
                format!("{file} section {ordinal} context one"),
                format!("{file} section {ordinal} context two"),
                format!("{file} section {ordinal} context three"),
                format!(
                    "{file} section {ordinal} {version} changed payload repeated repeated repeated"
                ),
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

fn git_output(repo: &Path, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git should start");
    assert!(
        output.status.success(),
        "git fixture command should succeed"
    );
    String::from_utf8(output.stdout).expect("fixture git output should be UTF-8")
}

#[cfg(feature = "headroom-compression")]
#[test]
fn compressed_artifact_indexes_every_source_file_and_hunk() {
    let fixture = fixture();
    let artifact = build_review_diff(&fixture.request()).expect("artifact should compress");

    assert!(artifact.metrics.artifact_bytes < artifact.metrics.source_bytes);
    assert_eq!(artifact.files.len(), 2);
    assert_eq!(
        artifact
            .files
            .iter()
            .map(|file| file.hunks.len())
            .sum::<usize>(),
        24
    );
    assert!(artifact.rendered.contains("alpha.txt"));
    assert!(artifact.rendered.contains("beta.txt"));

    for file in &artifact.files {
        assert!(artifact.rendered.contains(&file.path));
        for hunk in &file.hunks {
            assert_eq!(
                hunk.selector,
                format!("{}#hunk-{}", file.path, hunk.ordinal)
            );
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
    let raw_hunk = hunk
        .find("@@ ")
        .map(|start| &hunk[start..])
        .expect("expansion contains original hunk header");
    assert!(source.contains(raw_hunk));

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

#[test]
fn range_rejects_option_like_revisions_before_git_executes() {
    let fixture = fixture();
    let output_path = fixture.tempdir.path().join("review-diff-pwned");
    let request = ReviewDiffRequest::range(
        fixture.tempdir.path(),
        format!("--output={}", output_path.display()),
        &fixture.head,
    );

    let error = expand_review_diff(&request, "alpha.txt", None)
        .expect_err("option-like revision must be rejected");
    assert!(error.to_string().contains("revision"));
    assert!(
        !output_path.exists(),
        "git must not receive an output option"
    );
}

#[cfg(feature = "headroom-compression")]
#[test]
fn artifact_expansion_uses_the_immutable_worktree_snapshot() {
    let fixture = fixture();
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "snapshot"),
    )
    .expect("write dirty snapshot");
    let request = ReviewDiffRequest::worktree(fixture.tempdir.path());
    let artifact = build_review_diff(&request).expect("artifact should compress");

    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "later"),
    )
    .expect("mutate worktree after artifact");
    let hunk = artifact
        .expand("alpha.txt", Some(1))
        .expect("snapshot hunk expansion");

    assert!(hunk.contains("alpha.txt section 1 snapshot changed payload"));
    assert!(!hunk.contains("alpha.txt section 1 later changed payload"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn space_and_tab_paths_have_exact_stable_selectors_and_expansions() {
    let fixture = fixture();
    let special_path = "space name\tfile.txt";
    let special_file = fixture.tempdir.path().join(special_path);
    std::fs::write(&special_file, "base special content\n").expect("write special base");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), special_path.to_owned()],
    );
    git(fixture.tempdir.path(), ["commit", "-m", "add special path"]);
    std::fs::write(&special_file, "head special content\n").expect("write special head");
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "worktree"),
    )
    .expect("write enough worktree diff for compression");
    let request = ReviewDiffRequest::worktree(fixture.tempdir.path());
    let artifact = build_review_diff(&request).expect("artifact should compress");
    let expected_selector = "\"space name\\tfile.txt\"#hunk-1";

    let file = artifact
        .files
        .iter()
        .find(|file| file.path == special_path)
        .expect("special path should be indexed exactly");
    assert_eq!(file.hunks[0].selector, expected_selector);
    assert!(artifact.rendered.contains(expected_selector));

    let artifact_hunk = artifact
        .expand(special_path, Some(1))
        .expect("artifact expansion by exact special path");
    let request_hunk = expand_review_diff(&request, special_path, Some(1))
        .expect("request expansion by exact special path");
    assert!(artifact_hunk.contains("head special content"));
    assert!(request_hunk.contains("head special content"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn binary_header_path_with_b_component_remains_exact_without_markers() {
    let fixture = fixture();
    let binary_path = "foo b/bar.png";
    let binary_file = fixture.tempdir.path().join(binary_path);
    std::fs::create_dir_all(binary_file.parent().expect("binary parent"))
        .expect("make binary parent");
    std::fs::write(&binary_file, [0, 1, 2, 3]).expect("write binary base");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), binary_path.to_owned()],
    );
    git(fixture.tempdir.path(), ["commit", "-m", "add binary path"]);
    std::fs::write(&binary_file, [0, 9, 2, 3]).expect("write binary head");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), binary_path.to_owned()],
    );
    git(
        fixture.tempdir.path(),
        ["commit", "-m", "change binary path"],
    );
    let request = ReviewDiffRequest::range(
        fixture.tempdir.path(),
        &fixture.base,
        git_output(fixture.tempdir.path(), ["rev-parse", "HEAD"]).trim(),
    );
    let artifact = build_review_diff(&request).expect("artifact should compress");

    assert!(artifact.files.iter().any(|file| file.path == binary_path));
    let expanded = artifact
        .expand(binary_path, None)
        .expect("binary expansion by exact path");
    assert!(expanded.starts_with("diff --git a/foo b/bar.png b/foo b/bar.png\n"));
    assert!(!expanded.contains("+++ "));
    assert_eq!(
        expanded,
        expand_review_diff(&request, binary_path, None).expect("live expansion")
    );
}

#[cfg(feature = "headroom-compression")]
#[test]
fn newline_path_selectors_are_escaped_and_reversible() {
    let fixture = fixture();
    let special_path = "line\nbreak.txt";
    let special_file = fixture.tempdir.path().join(special_path);
    std::fs::write(&special_file, "base newline content\n").expect("write newline base");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), special_path.to_owned()],
    );
    git(fixture.tempdir.path(), ["commit", "-m", "add newline path"]);
    std::fs::write(&special_file, "head newline content\n").expect("write newline head");
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "worktree"),
    )
    .expect("write enough worktree diff for compression");
    let artifact = build_review_diff(&ReviewDiffRequest::worktree(fixture.tempdir.path()))
        .expect("artifact should compress");
    let file = artifact
        .files
        .iter()
        .find(|file| file.path == special_path)
        .expect("newline path should retain exact identity");
    let selector = &file.hunks[0].selector;

    assert_eq!(selector, "\"line\\nbreak.txt\"#hunk-1");
    assert!(artifact.rendered.contains(selector));
    assert!(!artifact
        .rendered
        .contains(&format!("{special_path}#hunk-1")));
    let expanded = artifact
        .expand(selector, None)
        .expect("rendered selector should select the original hunk");
    assert!(expanded.contains("head newline content"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn control_character_path_is_escaped_and_expands_from_its_selector() {
    let fixture = fixture();
    let special_path = "bell\u{7}file.txt";
    let special_file = fixture.tempdir.path().join(special_path);
    std::fs::write(&special_file, "base bell content\n").expect("write bell base");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), special_path.to_owned()],
    );
    git(fixture.tempdir.path(), ["commit", "-m", "add bell path"]);
    std::fs::write(&special_file, "head bell content\n").expect("write bell head");
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "worktree"),
    )
    .expect("write enough worktree diff for compression");
    let artifact = build_review_diff(&ReviewDiffRequest::worktree(fixture.tempdir.path()))
        .expect("artifact should compress");
    let file = artifact
        .files
        .iter()
        .find(|file| file.path == special_path)
        .expect("bell path should retain exact identity");
    let selector = &file.hunks[0].selector;

    assert_eq!(selector, "\"bell\\x07file.txt\"#hunk-1");
    assert!(artifact.rendered.contains(selector));
    assert!(artifact
        .expand(selector, None)
        .expect("control selector expansion")
        .contains("head bell content"));
}
