//! Validate plugin metadata files for both Codex and Claude Code.
//!
//! Ensures required JSON fields are present and plugin versions stay in sync
//! with the crate version in Cargo.toml.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Walk up from CARGO_MANIFEST_DIR until we find the workspace root
/// (the directory containing a Cargo.toml with `[workspace]`).
/// This is resilient to crate restructuring.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            let content = std::fs::read_to_string(&toml)
                .unwrap_or_else(|_| panic!("Could not read {}", toml.display()));
            if content.contains("[workspace]") {
                return dir;
            }
        }
        dir = dir
            .parent()
            .expect("reached filesystem root without finding workspace Cargo.toml")
            .to_path_buf();
    }
}

fn read_json(rel_path: &str) -> serde_json::Value {
    let path = workspace_root().join(rel_path);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("Could not read {}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("Invalid JSON in {rel_path}: {e}"))
}

fn read_text(rel_path: &str) -> String {
    let path = workspace_root().join(rel_path);
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("Could not read {}", path.display()))
}

/// Extract the body of a `NAME=( ... )` bash array declared in
/// `scripts/install-ironmem.sh`, e.g. `installer_array("REQUIRED_CODEX_PROMPTS")`
/// returns the lines between `REQUIRED_CODEX_PROMPTS=(` and its closing `)`.
fn installer_array(name: &str) -> String {
    let installer = read_text("scripts/install-ironmem.sh");
    let marker = format!("{name}=(");
    installer
        .split(marker.as_str())
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("scripts/install-ironmem.sh: missing {name} array"))
        .to_owned()
}

#[test]
fn codex_plugin_json_has_required_fields() {
    let json = read_json(".codex-plugin/plugin.json");
    assert!(
        json["name"].is_string(),
        "codex plugin.json: missing 'name'"
    );
    assert!(
        json["version"].is_string(),
        "codex plugin.json: missing 'version'"
    );
    assert!(
        json["mcpServers"].is_object(),
        "codex plugin.json: missing 'mcpServers'"
    );
    assert!(
        json["hooks"].is_string(),
        "codex plugin.json: missing 'hooks' path"
    );
    let interface = &json["interface"];
    assert!(
        interface["displayName"].is_string(),
        "codex plugin.json: missing interface.displayName"
    );
    assert!(
        interface["shortDescription"].is_string(),
        "codex plugin.json: missing interface.shortDescription"
    );
    assert_eq!(
        interface["category"].as_str(),
        Some("Coding"),
        "codex plugin.json: interface.category must remain Coding"
    );
    assert_eq!(
        interface["brandColor"].as_str(),
        Some("#1C5D6B"),
        "codex plugin.json: interface.brandColor changed unexpectedly"
    );
    let capabilities = interface["capabilities"]
        .as_array()
        .expect("codex plugin.json: interface.capabilities must be an array");
    for capability in ["Interactive", "Read", "Write"] {
        assert!(
            capabilities
                .iter()
                .any(|value| value.as_str() == Some(capability)),
            "codex plugin.json: missing interface capability {capability}"
        );
    }
    assert!(
        interface["defaultPrompt"].is_null(),
        "codex plugin.json: defaultPrompt is forbidden under the canonical rules model; duplicating project rules or MEMORY_PROTOCOL here violates write-rules ownership"
    );
}

#[test]
fn codex_hooks_json_has_required_hooks() {
    let json = read_json(".codex-plugin/hooks.json");
    let hooks = &json["hooks"];
    assert!(
        hooks["SessionStart"].is_array(),
        "codex hooks.json: missing 'SessionStart'"
    );
    assert!(hooks["Stop"].is_array(), "codex hooks.json: missing 'Stop'");
    assert!(
        hooks["PreCompact"].is_array(),
        "codex hooks.json: missing 'PreCompact'"
    );
}

#[test]
fn codex_collab_command_shim_is_packaged() {
    let text = read_text(".codex-plugin/commands/collab.md");
    assert!(
        text.contains("~/.codex/prompts/<selected prompt>"),
        "codex /collab command must delegate to the selected installed phase prompt"
    );
    for prompt in [
        "collab-plan-draft.md",
        "collab-plan-review.md",
        "collab-global-review.md",
        "collab-recovery.md",
        "collab-batch-impl.md",
    ] {
        assert!(
            text.contains(prompt),
            "codex /collab command must route to {prompt}"
        );
    }
    assert!(
        text.contains("tool discovery for `ironmem collab`"),
        "codex /collab command must explain how to lazy-load IronMEM tools"
    );
    assert!(
        text.contains("collab_set_implementer"),
        "codex /collab command must preserve implementer handoff routing"
    );
    assert!(
        text.contains("collab_wait_my_turn(session_id, \"codex\", 60)"),
        "codex /collab command must preserve the one-shot handoff wait"
    );
}

/// Phase-specific Codex prompts are installed independently so background
/// collaboration turns receive only the context required for their phase.
#[test]
fn codex_phase_prompts_are_packaged_and_invocable() {
    const REQUIRED_CODEX_PHASE_PROMPTS: [&str; 10] = [
        "collab-plan-draft",
        "collab-plan-synthesis",
        "collab-plan-review",
        "collab-plan-finalize",
        "collab-task-list",
        "collab-batch-impl",
        "collab-global-review",
        "collab-review-local",
        "collab-final-review",
        "collab-recovery",
    ];
    const MAX_PROMPT_BYTES: usize = 42_832 / 3;

    // Close the registry<->filesystem loop: REQUIRED_CODEX_PHASE_PROMPTS is a
    // hardcoded allowlist, and without this check nothing forces it to match
    // what is actually on disk. An unregistered `.codex-plugin/prompts/collab-*.md`
    // file would otherwise pass both this test and the Python lint while
    // receiving no byte budget, no $ARGUMENTS check, and no
    // collab_wait_my_turn check from either gate.
    let dir = workspace_root().join(".codex-plugin/prompts");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|_| panic!("Could not read directory {}", dir.display()))
        .filter_map(|entry| {
            let file_name = entry.ok()?.file_name();
            file_name.to_str()?.strip_suffix(".md").map(str::to_owned)
        })
        .filter(|name| name.starts_with("collab-"))
        .collect();
    let registered: BTreeSet<String> = REQUIRED_CODEX_PHASE_PROMPTS
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        on_disk, registered,
        ".codex-plugin/prompts/collab-*.md and REQUIRED_CODEX_PHASE_PROMPTS have drifted; \
         an unregistered phase prompt gets no byte budget, no $ARGUMENTS check, and no \
         collab_wait_my_turn check on either gate. If the new file is not itself a phase \
         prompt (e.g. a shared include), name it without the `collab-` prefix instead of \
         adding it here — this test has no exemption list, only the filename convention"
    );

    let required_prompts = installer_array("REQUIRED_CODEX_PROMPTS");

    for prompt_name in REQUIRED_CODEX_PHASE_PROMPTS {
        assert!(
            required_prompts
                .lines()
                .any(|line| line.trim() == prompt_name),
            "scripts/install-ironmem.sh: REQUIRED_CODEX_PROMPTS must include {prompt_name}"
        );

        let rel = format!(".codex-plugin/prompts/{prompt_name}.md");
        let path = workspace_root().join(&rel);
        let raw = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("Codex phase prompt is missing: {}", path.display()));
        assert!(
            raw.len() <= MAX_PROMPT_BYTES,
            "{rel}: {} bytes exceeds the {MAX_PROMPT_BYTES}-byte phase-prompt budget",
            raw.len()
        );

        let text = std::str::from_utf8(&raw)
            .unwrap_or_else(|_| panic!("{rel}: prompt must be valid UTF-8"));
        assert_eq!(
            text.matches("$ARGUMENTS").count(),
            1,
            "{rel}: must contain exactly one $ARGUMENTS placeholder"
        );

        let invocation = text
            .find("$ARGUMENTS")
            .expect("placeholder count guarantees $ARGUMENTS is present");
        let last_h2 = text[..invocation]
            .lines()
            .rfind(|line| line.starts_with("## "));
        assert_eq!(
            last_h2,
            Some("## Invocation"),
            "{rel}: the last Markdown h2 before $ARGUMENTS must be `## Invocation`"
        );
        assert!(
            text.contains("collab_wait_my_turn"),
            "{rel}: join-capable Codex phase prompts must bridge the one-shot handoff race"
        );
    }

    assert!(
        read_text(".codex-plugin/commands/collab.md").contains("collab_set_implementer"),
        "codex /collab command must preserve implementer handoff routing"
    );
    assert!(
        read_text(".codex-plugin/prompts/collab-global-review.md").contains("task_list` is null"),
        "codex global-review prompt must preserve shortcut review recovery"
    );
    let recovery = read_text(".codex-plugin/prompts/collab-recovery.md");
    assert!(
        recovery.contains("topic `review_local`") && recovery.contains("topic `final_review`"),
        "codex recovery prompt must cover delegated local and final review completions"
    );

    // Under `pilot=claude`, Codex runs this prompt for the plan review turn
    // against the same SubmitReview handler and the same InvalidVerdictValue
    // exposure as the Claude-side collab-turn-plan-review.md check in
    // claude_copilot_turn_templates_are_packaged; a verdict rename must break
    // this gate too, not just the other harness's.
    let codex_review_rel = ".codex-plugin/prompts/collab-plan-review.md";
    let codex_review = read_text(codex_review_rel);
    let codex_offered = verdict_alternation(codex_review_rel, &codex_review);
    assert_eq!(
        codex_offered,
        accepted_verdicts(),
        "{codex_review_rel}: verdict alternation must match exactly the accepted set \
         validated by state_machine.rs's SubmitReview handler"
    );
}

/// Parse the accepted verdict literals out of the `SubmitReview` match arm in
/// `state_machine/mod.rs`, rather than hardcoding a copy that could drift out
/// of sync with the server's actual `InvalidVerdictValue` validation. Mirrors
/// `parse_wire_names()` in `scripts/check_collab_turn_templates.py`, which
/// applies the same discipline to phase names: every failure to parse is a
/// loud error, because a check that silently finds zero verdicts would pass
/// everything.
fn accepted_verdicts() -> BTreeSet<String> {
    let rel = "crates/ironmem/src/collab/state_machine/mod.rs";
    let text = read_text(rel);
    let arm = text
        .split_once("verdict.as_str(),")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once(')').map(|(before, _)| before))
        .unwrap_or_else(|| {
            panic!(
                "{rel}: no `verdict.as_str(), \"...\" | \"...\"` match arm found — the \
                 plan-review verdict cross-check would pass vacuously, so it fails instead"
            )
        });
    let verdicts: BTreeSet<String> = arm
        .split('|')
        .filter_map(|s| {
            let s = s.trim();
            s.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_owned)
        })
        .collect();
    assert!(
        !verdicts.is_empty(),
        "{rel}: no verdict literals parsed out of the SubmitReview match arm — the \
         plan-review verdict cross-check would pass vacuously, so it fails instead"
    );
    verdicts
}

/// Extract the verdict set a review-turn template offers from its
/// `"verdict":"a|b|c"` example payload, tolerating optional whitespace after
/// the colon (a pretty-printed `"verdict": "..."` must not defeat the
/// anchor). Asserts the `"verdict":` marker occurs exactly once: a template
/// that gained a second example payload elsewhere (e.g. `{"verdict":"approve",...}`
/// illustrating a clean pass) would otherwise silently bind to the wrong
/// occurrence, and the resulting failure would blame the alternation for a
/// problem that is really a duplicate anchor.
fn verdict_alternation(rel: &str, text: &str) -> BTreeSet<String> {
    let marker = "\"verdict\":";
    let count = text.matches(marker).count();
    assert_eq!(
        count, 1,
        "{rel}: found {count} occurrences of `{marker}`, not 1 — the verdict cross-check \
         cannot tell which is the alternation; a second example payload elsewhere in the \
         template is the likely cause, not the alternation itself"
    );
    let after_marker =
        &text[text.find(marker).expect("count == 1 guarantees a match") + marker.len()..];
    let quoted = after_marker
        .trim_start()
        .strip_prefix('"')
        .unwrap_or_else(|| panic!("{rel}: `{marker}` must be followed by a quoted string value"));
    let end = quoted
        .find('"')
        .unwrap_or_else(|| panic!("{rel}: unterminated verdict value after `{marker}`"));
    quoted[..end]
        .split('|')
        .map(|s| s.trim().to_owned())
        .collect()
}

/// The copilot turn templates are what make role reversal real on the Claude
/// side: under `pilot=codex`, Claude runs the plan `review` and
/// `review_fix_global` turns. A template that exists in the repo but is
/// missing from `REQUIRED_CLAUDE_PROMPTS` is never installed into
/// `~/.claude/prompts/`, and the gap surfaces only mid-session as a missing
/// template at dispatch.
#[test]
fn claude_copilot_turn_templates_are_packaged() {
    const REQUIRED_CLAUDE_COPILOT_TEMPLATES: [&str; 2] =
        ["collab-turn-plan-review", "collab-turn-review-fix-global"];

    let required_prompts = installer_array("REQUIRED_CLAUDE_PROMPTS");

    for name in REQUIRED_CLAUDE_COPILOT_TEMPLATES {
        assert!(
            required_prompts.lines().any(|line| line.trim() == name),
            "scripts/install-ironmem.sh: REQUIRED_CLAUDE_PROMPTS must include {name}"
        );

        let rel = format!(".claude-plugin/prompts/{name}.md");
        let path = workspace_root().join(&rel);
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "Claude copilot turn template is missing: {}",
                path.display()
            )
        });
        let content = body(&rel, &raw);
        assert!(
            content.contains("ANTI-PUPPETEERING"),
            "{rel}: copilot template body must carry the anti-puppeteering preamble \
             (frontmatter doesn't count)"
        );
        assert!(
            content.contains("## Verdict"),
            "{rel}: copilot template body must carry a `## Verdict` section (the 3-line \
             result/ref/blocker contract itself is enforced by \
             scripts/check_collab_turn_templates.py)"
        );
    }

    // The verdict vocabulary the template offers must match exactly the set
    // state_machine.rs actually validates: this fails both when the template
    // drops a verdict the server still accepts, and when the server adds one
    // the template never mentions.
    let review_rel = ".claude-plugin/prompts/collab-turn-plan-review.md";
    let review = read_text(review_rel);
    let accepted = accepted_verdicts();
    let offered = verdict_alternation(review_rel, &review);
    assert_eq!(
        offered, accepted,
        "{review_rel}: verdict alternation {offered:?} must match exactly the accepted set \
         {accepted:?} validated by state_machine.rs's SubmitReview handler"
    );

    let fix_global = read_text(".claude-plugin/prompts/collab-turn-review-fix-global.md");
    assert!(
        fix_global.contains("/ultrareview-local"),
        "collab-turn-review-fix-global.md: must name Claude's own finding pass"
    );
    assert!(
        !fix_global.contains("pr-review-toolkit"),
        "collab-turn-review-fix-global.md: must not name Codex's tooling"
    );
}

/// Assert one Claude turn template sends only under the pilot-dependent
/// `$SENDER` identity.
///
/// `sites` lists one distinguishing substring per `collab_send` call site,
/// each starting at `collab_send(sender="$SENDER"` so the sender is part of
/// what is proven. Checking the sites individually is what a
/// `matches(...).count() == N` lock cannot do: the count is site-blind, so a
/// fifth send added *without* the parameterization keeps it at N and passes,
/// while a legitimately parameterized fifth send fails it as a false alarm.
/// The count is therefore asserted only as a floor.
fn assert_sender_parameterized(rel: &str, sites: &[&str]) {
    // A template that is not installed is never dispatched, whatever it
    // contains — so the packaging claim is asserted, not assumed.
    let name = rel
        .strip_prefix(".claude-plugin/prompts/")
        .and_then(|n| n.strip_suffix(".md"))
        .unwrap_or_else(|| panic!("{rel}: expected a .claude-plugin/prompts/*.md path"));
    assert!(
        installer_array("REQUIRED_CLAUDE_PROMPTS")
            .lines()
            .any(|line| line.trim() == name),
        "scripts/install-ironmem.sh: REQUIRED_CLAUDE_PROMPTS must include {name} — an \
         uninstalled template is never copied to ~/.claude/prompts/ and fails at dispatch"
    );

    let content = read_text(rel);
    assert!(
        !content.contains("sender=\"claude\""),
        "{rel}: must not regress to a literal sender=\"claude\" — this breaks pilot=codex \
         sessions, whose sends are rejected when the sender does not match \
         collab_status.current_owner"
    );
    for site in sites {
        assert!(
            content.contains(site),
            "{rel}: missing parameterized collab_send call site `{site}` — every send in \
             this template must go out as sender=\"$SENDER\""
        );
    }
    let call_sites = content.matches("sender=\"$SENDER\"").count();
    assert!(
        call_sites >= sites.len(),
        "{rel}: expected at least {} sender=\"$SENDER\" collab_send call sites, found \
         {call_sites}",
        sites.len()
    );
}

/// `collab-turn-submit.md` sends collab protocol messages (`final`,
/// `final_review`, `failure_report`) under a pilot-dependent identity:
/// `sender="$SENDER"`, verified against `collab_status.current_owner` before
/// every send. A regression back to a literal `sender="claude"` here silently
/// breaks `pilot=codex` sessions — the server rejects the send as coming from
/// the wrong owner and the session stalls in
/// `CodeReviewFinalPending`/`PlanClaudeFinalizePending` (the wire names
/// `collab_status` emits for `CodeReviewFinalPending`/`PlanFinalizePending`,
/// per `crates/ironmem/src/collab/phase.rs`), exactly the failure this plan
/// fixed.
///
/// What this test proves, precisely: the repo file's content contract, plus
/// its registration in `REQUIRED_CLAUDE_PROMPTS`. It reads the same repo
/// bytes `scripts/check_collab_turn_templates.py` reads — it is a second,
/// independent runner over one protocol-critical property, not a check of the
/// installed copy, and it detects no packaging drift beyond that installer
/// array.
#[test]
fn collab_turn_submit_template_is_sender_parameterized() {
    // The pr_create_failed site was originally missed — a senderless
    // `failure_report` there strands exactly the pilot=codex PR-creation
    // failure this template exists to fix.
    assert_sender_parameterized(
        ".claude-plugin/prompts/collab-turn-submit.md",
        &[
            "collab_send(sender=\"$SENDER\", topic=\"final_review\",",
            "collab_send(sender=\"$SENDER\", topic=\"final\",",
            "collab_send(sender=\"$SENDER\",\n  topic=\"failure_report\",\n  \
             content=<JSON {\"coding_failure\":\"pr_create_failed:",
            "collab_send(sender=\"$SENDER\",\n  topic=\"failure_report\", content=<JSON \
             {\"coding_failure\":\n  \"approved_artifact_unfetchable:",
        ],
    );
}

/// `collab-turn-task-list.md` has the same exposure for the `PlanLocked`
/// bridge: `PublishFinal` does not reassign ownership
/// (`collab/state_machine/mod.rs`), so under `pilot=codex` this turn is
/// entered with `current_owner == codex` while `SubmitTaskList` requires
/// `pilot(session)`. `PlanLocked` is not `Phase::is_coding_active()`, so a
/// rejected send has no `failure_report` escape either — a hardcoded
/// `sender="claude"` here dead-ends the session outright.
#[test]
fn collab_turn_task_list_template_is_sender_parameterized() {
    assert_sender_parameterized(
        ".claude-plugin/prompts/collab-turn-task-list.md",
        &["collab_send(sender=\"$SENDER\", topic=\"task_list\","],
    );
}

/// The `<HEAD>` placeholder is what actually caused issue #284: a template
/// spelling the field `head_sha:<HEAD>` reads as "write the string HEAD" often
/// enough that agents did, and the server then stored a revision expression as
/// the session's fixed drift-detection point. The seed-site shape checks refuse
/// that now, and the reported-head guard refuses it as a **Terminal**
/// `branch_drift:` — so the placeholder no longer merely degrades a session, it
/// ends one.
///
/// This pins the templates against regressing to it. Substituting a sha is the
/// agent's job, and `<sha>` says so where `<HEAD>` invites the literal. The
/// scan is over every prompt on both plugins rather than the five that were
/// fixed, because a new turn template is exactly where this comes back.
#[test]
fn no_turn_template_spells_head_sha_with_a_head_placeholder() {
    let mut offenders = Vec::new();
    for dir in [".claude-plugin/prompts", ".codex-plugin/prompts"] {
        let root = workspace_root().join(dir);
        for entry in std::fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("Could not list {}: {e}", root.display()))
        {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("readable template");
            for (idx, line) in body.lines().enumerate() {
                // Both spellings the field appears in: the JSON payload
                // (`"head_sha":"<HEAD>"`) and the verdict ref line
                // (`head_sha:<HEAD>`). `<current HEAD>` is prose describing
                // which commit to read, not a placeholder to copy, and is
                // left alone deliberately.
                if line.contains("head_sha\":\"<HEAD>") || line.contains("head_sha:<HEAD>") {
                    offenders.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "turn templates must not spell head_sha with a bare <HEAD> placeholder — \
         an agent that copies it literally gets a Terminal branch_drift: refusal. \
         Use <sha> and tell the worker to run `git rev-parse HEAD`:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn claude_plugin_json_has_required_fields() {
    let json = read_json(".claude-plugin/plugin.json");
    assert!(
        json["name"].is_string(),
        "claude plugin.json: missing 'name'"
    );
    assert!(
        json["version"].is_string(),
        "claude plugin.json: missing 'version'"
    );
    assert!(
        json["mcpServers"].is_object(),
        "claude plugin.json: missing 'mcpServers'"
    );
}

#[test]
fn claude_mcp_json_has_required_fields() {
    let json = read_json(".claude-plugin/.mcp.json");
    let server = &json["ironmem"];
    assert!(
        server.is_object(),
        "claude .mcp.json: missing 'ironmem' server entry"
    );
    assert!(
        server["command"].is_string(),
        "claude .mcp.json: missing 'command'"
    );
    assert!(
        server["args"].is_array(),
        "claude .mcp.json: missing 'args'"
    );
}

#[test]
fn plugin_versions_match_cargo_toml() {
    let cargo_version = env!("CARGO_PKG_VERSION");

    let codex = read_json(".codex-plugin/plugin.json");
    let codex_version = codex["version"].as_str().unwrap_or("");
    assert_eq!(
        codex_version, cargo_version,
        "codex plugin.json version ({codex_version}) must match Cargo.toml ({cargo_version})"
    );

    let claude = read_json(".claude-plugin/plugin.json");
    let claude_version = claude["version"].as_str().unwrap_or("");
    assert_eq!(
        claude_version, cargo_version,
        "claude plugin.json version ({claude_version}) must match Cargo.toml ({cargo_version})"
    );

    // The generated wrapper (`scripts/sync_mcp_wrappers.py`) hard-fails with
    // exit 1 when plugin.json's version and the binary's `--version` differ,
    // so an unguarded plugin.json silently breaks that harness's MCP startup
    // on the next Cargo version bump.
    let muse = read_json(".muse-plugin/plugin.json");
    let muse_version = muse["version"].as_str().unwrap_or("");
    assert_eq!(
        muse_version, cargo_version,
        "muse plugin.json version ({muse_version}) must match Cargo.toml ({cargo_version})"
    );
}

/// Return the text between the first two `---` fences of a markdown file.
fn frontmatter(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Return the text after the closing `---` frontmatter fence. Content checks
/// for a *body* property (a banner, a heading) must scan this, not the raw
/// file — a token stashed in frontmatter would otherwise satisfy a `contains`
/// check without the property it names ever appearing where a reader
/// encounters it. Panics rather than falling back to the raw file when the
/// frontmatter block is missing or unterminated: silently scanning the whole
/// file in that case would defeat the property this helper exists to
/// enforce (mirrors the explicit panic in
/// `claude_review_agents_advertise_lean_profile` for the same failure).
fn body<'a>(rel: &str, raw: &'a str) -> &'a str {
    let front = frontmatter(raw)
        .unwrap_or_else(|| panic!("{rel}: missing or unterminated YAML frontmatter block"));
    // Locate `front`'s end within `raw` by pointer arithmetic rather than
    // assuming a fixed "---".len() + front.len() offset: that fixed-offset
    // assumption would silently misplace the body boundary if frontmatter()
    // ever trimmed or normalized the slice it returns.
    let front_end = front.as_ptr() as usize - raw.as_ptr() as usize + front.len();
    let close = raw[front_end..].find("\n---").unwrap_or_else(|| {
        panic!("{rel}: frontmatter() found a closing fence but body() could not relocate it")
    });
    &raw[front_end + close + "\n---".len()..]
}

/// Extract the full `tools:` value from YAML frontmatter, including any
/// block-style continuation lines (indented `- item` list entries or a
/// wrapped flow array). Returns `None` when there is no `tools:` key.
///
/// Collecting the whole value — not just the key line — matters: a reformat
/// from inline flow style (`tools: ["Read", "Bash"]`) to block style
/// (`tools:` then indented `- item` lines) is valid YAML, and a single-line
/// check would let a memory tool on a following line slip past the exclusion
/// assertion below — the exact drift this guard exists to catch.
fn tools_value(front: &str) -> Option<String> {
    let mut lines = front.lines();
    let key_line = lines
        .by_ref()
        .find(|l| l.trim_start().starts_with("tools:"))?;
    let key_indent = key_line.len() - key_line.trim_start().len();
    let mut value = key_line.trim_start()["tools:".len()..].to_string();
    // A YAML value continues onto lines indented deeper than its key; a line
    // at or below the key's indentation (or a blank line) ends the block.
    for line in lines {
        if line.trim().is_empty() {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= key_indent {
            break;
        }
        value.push('\n');
        value.push_str(line);
    }
    Some(value)
}

/// Read-only Claude review sub-agents must advertise an explicit lean tool
/// allowlist that excludes every ironmem MCP tool (issue #189). A missing
/// `tools:` key means the agent inherits the full MCP surface — including
/// memory tools — which is exactly the drift this guards against.
#[test]
fn claude_review_agents_advertise_lean_profile() {
    let review_agents = [
        "code-reviewer",
        "architect",
        "doc-reviewer",
        "security-reviewer",
    ];
    for agent in review_agents {
        let rel = format!(".claude-plugin/agents/{agent}.md");
        let path = workspace_root().join(&rel);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("Could not read {}", path.display()));
        let front =
            frontmatter(&raw).unwrap_or_else(|| panic!("{rel}: missing YAML frontmatter block"));
        let tools = tools_value(front).unwrap_or_else(|| {
            panic!(
                "{rel}: review agent must declare an explicit `tools:` allowlist \
                 so it does not inherit the full MCP surface (issue #189)"
            )
        });
        assert!(
            !tools.contains("ironmem"),
            "{rel}: review agent `tools:` must not list any ironmem MCP tool (found: {tools})"
        );
    }
}

/// `tools_value` must capture the whole `tools:` value in both YAML styles so
/// the exclusion check cannot be evaded by reformatting. A block-style list
/// with a memory tool on a continuation line must be surfaced, and a following
/// top-level key must not bleed into the captured value.
#[test]
fn tools_value_captures_flow_and_block_styles() {
    let flow = "name: x\ntools: [\"Read\", \"Bash\"]\nmodel: y";
    assert_eq!(
        tools_value(flow).as_deref(),
        Some(" [\"Read\", \"Bash\"]"),
        "flow-style value should be captured verbatim"
    );

    let block = "name: x\ntools:\n  - Read\n  - mcp__ironmem__search\nmodel: y";
    let captured = tools_value(block).expect("block-style tools: should be found");
    assert!(
        captured.contains("mcp__ironmem__search"),
        "block-style continuation lines must be captured, got: {captured}"
    );
    assert!(
        !captured.contains("model:"),
        "the next top-level key must not bleed into the tools value, got: {captured}"
    );

    assert_eq!(
        tools_value("name: x\nmodel: y"),
        None,
        "a missing tools: key must return None"
    );
}
