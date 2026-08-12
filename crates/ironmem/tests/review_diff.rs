use std::path::Path;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use ironmem::review_diff::{
    build_review_diff, expand_review_diff, ReviewDiffRequest, ReviewDiffSource,
};
use tempfile::TempDir;

static GIT_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct ScopedEnvVars {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl ScopedEnvVars {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let previous = values
            .iter()
            .map(|(key, value)| {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, previous)
            })
            .collect();
        Self { previous }
    }
}

impl Drop for ScopedEnvVars {
    fn drop(&mut self) {
        for (key, previous) in &self.previous {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

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

#[cfg(feature = "headroom-compression")]
fn minimal_fixture() -> DiffFixture {
    let tempdir = tempfile::tempdir().expect("minimal temp repo");
    git(tempdir.path(), ["init"]);
    git(
        tempdir.path(),
        ["config", "user.email", "review-diff@example.test"],
    );
    git(tempdir.path(), ["config", "user.name", "Review Diff Test"]);

    let path = tempdir.path().join("tiny.txt");
    std::fs::write(&path, "base\n").expect("write minimal base");
    git(tempdir.path(), ["add", "tiny.txt"]);
    git(tempdir.path(), ["commit", "-m", "base"]);
    let base = git_output(tempdir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(&path, "head\n").expect("write minimal head");
    git(tempdir.path(), ["add", "tiny.txt"]);
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
    let mut command = Command::new("git");
    scrub_git_environment(&mut command);
    let status = command
        .args(args)
        .current_dir(repo)
        .status()
        .expect("git should start");
    assert!(status.success(), "git fixture setup should succeed");
}

fn git_output(repo: &Path, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> String {
    let mut command = Command::new("git");
    scrub_git_environment(&mut command);
    let output = command
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

fn scrub_git_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
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

#[test]
fn review_diff_ignores_inherited_git_repository_and_config_overrides() {
    let _env = GIT_ENV_MUTEX.lock().expect("git env lock");
    let intended = fixture();
    let hostile = tempfile::tempdir().expect("hostile repo");
    git(hostile.path(), ["init"]);
    git(
        hostile.path(),
        ["config", "user.email", "hostile@example.test"],
    );
    git(hostile.path(), ["config", "user.name", "Hostile Git Test"]);
    std::fs::write(hostile.path().join("hostile.txt"), "hostile\n").expect("write hostile");
    git(hostile.path(), ["add", "."]);
    git(hostile.path(), ["commit", "-m", "hostile"]);
    let expected = intended.source();
    let hostile_git_dir = hostile.path().join(".git");
    let _overrides = ScopedEnvVars::set(&[
        ("GIT_DIR", hostile_git_dir.to_string_lossy().as_ref()),
        ("GIT_WORK_TREE", hostile.path().to_string_lossy().as_ref()),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "core.quotePath"),
        ("GIT_CONFIG_VALUE_0", "false"),
    ]);

    let expanded = expand_review_diff(&intended.request(), "alpha.txt", None)
        .expect("request repo must win over inherited Git overrides");
    assert!(expected.contains(&expanded));
    assert!(expanded.contains("alpha.txt section 1 head changed payload"));
    assert!(!expanded.contains("hostile"));
}

#[test]
fn range_diff_over_the_shared_checkout_is_blind_to_a_dirty_second_worktree() {
    // The #249 incident in test form: a read-only reviewer treated a mutating
    // lens's leftovers in a shared checkout as canonical, and reported a
    // finding that did not exist in the code under review. Task 12 gives
    // every mutating lens its own throwaway worktree cut from the same
    // repository instead of writing into the shared checkout; this pins that
    // the isolation actually holds at the review_diff layer — an uncommitted
    // change accumulated in a SEPARATE worktree must never appear in a
    // `Range` diff of the canonical range read from the shared checkout,
    // which is exactly what a read-only lens reads while a mutating lens runs
    // concurrently in its own worktree.
    let fixture = fixture();
    let worktree_root = tempfile::tempdir().expect("worktree parent");
    let worktree_path = worktree_root.path().join("mutating-lens-worktree");
    git(
        fixture.tempdir.path(),
        [
            "worktree",
            "add",
            "--detach",
            worktree_path.to_str().expect("worktree path is utf8"),
            &fixture.head,
        ],
    );

    // Accumulate a contaminating, uncommitted change in the second worktree —
    // both a tracked-file edit and an untracked leftover, exactly what a
    // mutating lens's test run or build would leave behind.
    let contaminant = "CONTAMINATION_FROM_A_MUTATING_LENS_LEFTOVER";
    std::fs::write(worktree_path.join("alpha.txt"), format!("{contaminant}\n"))
        .expect("write contaminating change in the second worktree");
    std::fs::write(
        worktree_path.join("untracked-leftover.txt"),
        format!("{contaminant}\n"),
    )
    .expect("write untracked leftover in the second worktree");

    // Read the canonical range from the SHARED checkout — the tree a
    // read-only reviewer is looking at while the mutating lens runs isolated
    // in its own worktree above.
    let request = fixture.request();
    let expanded = expand_review_diff(&request, "alpha.txt", None)
        .expect("shared-checkout expansion should succeed");
    assert!(
        !expanded.contains(contaminant),
        "a canonical-range diff of the shared checkout must not see a dirty \
         change made in a separate worktree: {expanded}"
    );

    let source = fixture.source();
    assert!(
        !source.contains(contaminant),
        "the raw source diff of the canonical range must not see the \
         separate worktree's dirty change either: {source}"
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
fn header_only_rename_with_spaces_uses_the_new_path() {
    let fixture = fixture();
    let old_path = "old name.txt";
    let new_path = "new name.txt";
    std::fs::write(
        fixture.tempdir.path().join(old_path),
        "same rename content\n",
    )
    .expect("write rename source");
    git(fixture.tempdir.path(), ["add", old_path]);
    git(
        fixture.tempdir.path(),
        ["commit", "-m", "add rename source"],
    );
    let old_head = git_output(fixture.tempdir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git(fixture.tempdir.path(), ["mv", old_path, new_path]);
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "rename"),
    )
    .expect("write enough range diff for compression");
    git(fixture.tempdir.path(), ["add", "alpha.txt"]);
    git(fixture.tempdir.path(), ["commit", "-m", "rename source"]);
    let request = ReviewDiffRequest::range(
        fixture.tempdir.path(),
        old_head,
        git_output(fixture.tempdir.path(), ["rev-parse", "HEAD"]).trim(),
    );
    let artifact = build_review_diff(&request).expect("artifact should compress");

    assert!(artifact.files.iter().any(|file| file.path == new_path));
    let expanded = artifact
        .expand(new_path, None)
        .expect("rename expansion by new path");
    assert!(expanded.starts_with("diff --git "));
    assert!(
        expanded.contains("old name.txt"),
        "rename expansion: {expanded}"
    );
    assert!(
        expanded.contains("new name.txt"),
        "rename expansion: {expanded}"
    );
    assert!(!expanded.contains("+++ "));
    assert!(expand_review_diff(&request, old_path, None).is_err());
}

#[cfg(feature = "headroom-compression")]
#[test]
fn header_only_rename_with_embedded_b_component_uses_new_path() {
    let fixture = fixture();
    let old_path = "foo b/old.txt";
    let new_path = "new name.txt";
    let old_file = fixture.tempdir.path().join(old_path);
    std::fs::create_dir_all(old_file.parent().expect("rename parent")).expect("make rename parent");
    std::fs::write(&old_file, "same embedded rename content\n").expect("write rename source");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), old_path.to_owned()],
    );
    git(
        fixture.tempdir.path(),
        ["commit", "-m", "add embedded rename source"],
    );
    let old_head = git_output(fixture.tempdir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git(fixture.tempdir.path(), ["mv", old_path, new_path]);
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "embedded-rename"),
    )
    .expect("write enough range diff for compression");
    git(fixture.tempdir.path(), ["add", "alpha.txt"]);
    git(
        fixture.tempdir.path(),
        ["commit", "-m", "rename embedded source"],
    );
    let request = ReviewDiffRequest::range(
        fixture.tempdir.path(),
        old_head,
        git_output(fixture.tempdir.path(), ["rev-parse", "HEAD"]).trim(),
    );
    let artifact = build_review_diff(&request).expect("artifact should compress");

    assert!(artifact.files.iter().any(|file| file.path == new_path));
    let expanded = artifact
        .expand(new_path, None)
        .expect("rename expansion by exact new path");
    assert!(expanded.starts_with("diff --git "));
    assert!(expanded.contains("rename to new name.txt"));
    assert!(!expanded.contains("+++ "));
    assert!(expand_review_diff(&request, old_path, None).is_err());
}

#[cfg(feature = "headroom-compression")]
#[test]
fn review_diff_disables_repository_textconv_drivers() {
    let fixture = fixture();
    let marker = fixture.tempdir.path().join("textconv-ran");
    let script = fixture.tempdir.path().join("hostile-textconv.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf invoked > '{}'\n", marker.display()),
    )
    .expect("write textconv script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("textconv metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make textconv executable");
    }
    std::fs::write(
        fixture.tempdir.path().join(".gitattributes"),
        "converted.txt diff=hostile\n",
    )
    .expect("write attributes");
    std::fs::write(
        fixture.tempdir.path().join("converted.txt"),
        "base converted content\n",
    )
    .expect("write converted base");
    git(
        fixture.tempdir.path(),
        ["add", ".gitattributes", "converted.txt"],
    );
    git(
        fixture.tempdir.path(),
        ["commit", "-m", "configure hostile textconv"],
    );
    git(
        fixture.tempdir.path(),
        [
            "config",
            "diff.hostile.textconv",
            script.to_string_lossy().as_ref(),
        ],
    );
    std::fs::write(
        fixture.tempdir.path().join("converted.txt"),
        "head converted content\n",
    )
    .expect("write converted head");
    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "textconv"),
    )
    .expect("write enough worktree diff for compression");
    let request = ReviewDiffRequest::worktree(fixture.tempdir.path());

    let artifact = build_review_diff(&request).expect("artifact should compress");
    let expanded = expand_review_diff(&request, "converted.txt", None)
        .expect("source expansion remains available without textconv");
    assert!(artifact
        .files
        .iter()
        .any(|file| file.path == "converted.txt"));
    assert!(expanded.contains("head converted content"));
    assert!(!marker.exists(), "repository textconv must not execute");
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
    let quoted_path = "\"line\\nbreak.txt\"";
    let file = artifact
        .expand(quoted_path, None)
        .expect("quoted file path should expand the full file section");
    assert!(file.starts_with("diff --git "));
    assert!(file.contains("head newline content"));
    let live_file = expand_review_diff(
        &ReviewDiffRequest::worktree(fixture.tempdir.path()),
        quoted_path,
        None,
    )
    .expect("live expansion should decode a quoted file path");
    assert_eq!(live_file, file);
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

#[cfg(feature = "headroom-compression")]
#[test]
fn utf8_escaped_path_retains_unicode_identity_and_selector_expansion() {
    let fixture = fixture();
    let special_path = "é.txt";
    let special_file = fixture.tempdir.path().join(special_path);
    std::fs::write(&special_file, "base unicode content\n").expect("write unicode base");
    git(
        fixture.tempdir.path(),
        vec!["add".to_owned(), special_path.to_owned()],
    );
    git(fixture.tempdir.path(), ["commit", "-m", "add unicode path"]);
    std::fs::write(&special_file, "head unicode content\n").expect("write unicode head");
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
        .expect("unicode path should retain exact identity");
    let selector = &file.hunks[0].selector;

    assert_eq!(selector, "é.txt#hunk-1");
    assert!(artifact.rendered.contains(selector));
    assert!(artifact
        .expand(selector, None)
        .expect("unicode selector expansion")
        .contains("head unicode content"));
}

fn review_diff_command(fixture: &DiffFixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ironmem"));
    scrub_git_environment(&mut command);
    command
        .arg("review-diff")
        .arg("--repo")
        .arg(fixture.tempdir.path())
        .arg("--base")
        .arg(&fixture.base)
        .arg("--head")
        .arg(&fixture.head);
    command
}

#[cfg(feature = "headroom-compression")]
fn review_diff_default_repo_command(fixture: &DiffFixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ironmem"));
    scrub_git_environment(&mut command);
    command
        .arg("review-diff")
        .current_dir(fixture.tempdir.path());
    command
}

#[cfg(not(feature = "headroom-compression"))]
fn build_no_feature_review_diff_binary() -> (TempDir, std::path::PathBuf) {
    let target_dir = tempfile::tempdir().expect("isolated Cargo target directory");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    scrub_git_environment(&mut build);
    let output = build
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--package")
        .arg("ironmem")
        .arg("--bin")
        .arg("ironmem")
        .arg("--no-default-features")
        .env("CARGO_TARGET_DIR", target_dir.path())
        .output()
        .expect("isolated no-feature ironmem build should start");
    assert!(
        output.status.success(),
        "isolated no-feature ironmem build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let binary = target_dir.path().join("debug").join(if cfg!(windows) {
        "ironmem.exe"
    } else {
        "ironmem"
    });
    assert!(binary.is_file(), "isolated binary should exist: {binary:?}");
    (target_dir, binary)
}

#[cfg(not(feature = "headroom-compression"))]
fn no_feature_review_diff_command(binary: &Path, fixture: &DiffFixture) -> Command {
    let mut command = Command::new(binary);
    scrub_git_environment(&mut command);
    command
        .arg("review-diff")
        .arg("--repo")
        .arg(fixture.tempdir.path())
        .arg("--base")
        .arg(&fixture.base)
        .arg("--head")
        .arg(&fixture.head);
    command
}

#[cfg(feature = "headroom-compression")]
#[test]
fn cli_review_diff_renders_the_compressed_range_artifact() {
    let fixture = fixture();
    let output = review_diff_command(&fixture)
        .output()
        .expect("review-diff CLI should start");

    assert!(
        output.status.success(),
        "review-diff CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.contains("review-diff source index"));
    assert!(stdout.contains("alpha.txt#hunk-1"));
    assert!(stdout.contains("review-diff metrics"));
    assert!(stdout.contains("source_bytes="));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn cli_review_diff_uses_the_current_fixture_repo_by_default() {
    let fixture = fixture();
    let range = review_diff_default_repo_command(&fixture)
        .arg("--base")
        .arg(&fixture.base)
        .arg("--head")
        .arg(&fixture.head)
        .output()
        .expect("review-diff CLI should start");
    assert!(
        range.status.success(),
        "default-repo range failed: {}",
        String::from_utf8_lossy(&range.stderr)
    );
    assert!(String::from_utf8_lossy(&range.stdout).contains("alpha.txt#hunk-1"));

    std::fs::write(
        fixture.tempdir.path().join("alpha.txt"),
        fixture_contents("alpha.txt", "worktree"),
    )
    .expect("write worktree fixture");
    let worktree = review_diff_default_repo_command(&fixture)
        .arg("--worktree")
        .output()
        .expect("review-diff CLI should start");
    assert!(
        worktree.status.success(),
        "default-repo worktree failed: {}",
        String::from_utf8_lossy(&worktree.stderr)
    );
    assert!(String::from_utf8_lossy(&worktree.stdout).contains("alpha.txt#hunk-1"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn cli_review_diff_expands_the_requested_hunk_from_its_new_artifact() {
    let fixture = fixture();
    let output = review_diff_command(&fixture)
        .arg("--expand-file")
        .arg("beta.txt")
        .arg("--hunk")
        .arg("7")
        .output()
        .expect("review-diff CLI should start");

    assert!(
        output.status.success(),
        "review-diff CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.starts_with("diff --git a/beta.txt b/beta.txt\n"));
    assert!(stdout.contains("beta.txt section 7 head changed payload"));
    assert!(!stdout.contains("beta.txt section 8 head changed payload"));
}

#[cfg(feature = "headroom-compression")]
#[test]
fn cli_review_diff_expands_a_noncompressible_range_without_building_an_artifact() {
    let fixture = minimal_fixture();
    let normal = review_diff_command(&fixture)
        .output()
        .expect("review-diff CLI should start");
    assert!(!normal.status.success());
    assert!(String::from_utf8_lossy(&normal.stderr).contains("did not reduce ingestion size"));

    let file = review_diff_command(&fixture)
        .arg("--expand-file")
        .arg("tiny.txt")
        .output()
        .expect("review-diff file expansion should start");
    assert!(
        file.status.success(),
        "feature-on file expansion failed: {}",
        String::from_utf8_lossy(&file.stderr)
    );
    let stdout = String::from_utf8(file.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.starts_with("diff --git a/tiny.txt b/tiny.txt\n"));
    assert!(stdout.contains("+head\n"));

    let hunk = review_diff_command(&fixture)
        .arg("--expand-file")
        .arg("tiny.txt")
        .arg("--hunk")
        .arg("1")
        .output()
        .expect("review-diff hunk expansion should start");
    assert!(
        hunk.status.success(),
        "feature-on hunk expansion failed: {}",
        String::from_utf8_lossy(&hunk.stderr)
    );
    let stdout = String::from_utf8(hunk.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.contains("+head\n"));
}

#[test]
fn cli_review_diff_rejects_incomplete_or_conflicting_sources() {
    let fixture = fixture();

    let incomplete = Command::new(env!("CARGO_BIN_EXE_ironmem"))
        .arg("review-diff")
        .arg("--repo")
        .arg(fixture.tempdir.path())
        .arg("--base")
        .arg(&fixture.base)
        .output()
        .expect("review-diff CLI should start");
    assert!(!incomplete.status.success());
    assert!(String::from_utf8_lossy(&incomplete.stderr).contains("head"));

    let conflicting = review_diff_command(&fixture)
        .arg("--worktree")
        .output()
        .expect("review-diff CLI should start");
    assert!(!conflicting.status.success());
    let stderr = String::from_utf8_lossy(&conflicting.stderr);
    assert!(stderr.contains("worktree") && stderr.contains("base"));
}

#[test]
fn cli_review_diff_requires_an_expansion_file_and_positive_hunk_ordinal() {
    let fixture = fixture();

    let missing_file = review_diff_command(&fixture)
        .arg("--hunk")
        .arg("1")
        .output()
        .expect("review-diff CLI should start");
    assert!(!missing_file.status.success());
    assert!(String::from_utf8_lossy(&missing_file.stderr).contains("expand-file"));

    let zero_hunk = review_diff_command(&fixture)
        .arg("--expand-file")
        .arg("alpha.txt")
        .arg("--hunk")
        .arg("0")
        .output()
        .expect("review-diff CLI should start");
    assert!(!zero_hunk.status.success());
    assert!(String::from_utf8_lossy(&zero_hunk.stderr).contains("one-based"));
}

#[cfg(not(feature = "headroom-compression"))]
#[test]
fn cli_review_diff_without_compression_supports_expansion_only() {
    let fixture = fixture();
    let (_target_dir, binary) = build_no_feature_review_diff_binary();
    let output = no_feature_review_diff_command(&binary, &fixture)
        .output()
        .expect("review-diff CLI should start");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("headroom-compression"));

    let file = no_feature_review_diff_command(&binary, &fixture)
        .arg("--expand-file")
        .arg("alpha.txt")
        .output()
        .expect("review-diff file expansion should start");
    assert!(
        file.status.success(),
        "feature-off file expansion failed: {}",
        String::from_utf8_lossy(&file.stderr)
    );
    let stdout = String::from_utf8(file.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.starts_with("diff --git a/alpha.txt b/alpha.txt\n"));
    assert!(stdout.contains("alpha.txt section 12 head changed payload"));

    let hunk = no_feature_review_diff_command(&binary, &fixture)
        .arg("--expand-file")
        .arg("beta.txt")
        .arg("--hunk")
        .arg("7")
        .output()
        .expect("review-diff hunk expansion should start");
    assert!(
        hunk.status.success(),
        "feature-off hunk expansion failed: {}",
        String::from_utf8_lossy(&hunk.stderr)
    );
    let stdout = String::from_utf8(hunk.stdout).expect("CLI stdout should be UTF-8");
    assert!(stdout.contains("beta.txt section 7 head changed payload"));
    assert!(!stdout.contains("beta.txt section 8 head changed payload"));
}
