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
         collab_wait_my_turn check on either gate"
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

/// The copilot turn templates are what make role reversal real on the Claude
/// side: under `pilot=codex`, Claude runs the plan `review` and
/// `review_fix_global` turns. A template that exists in the repo but is
/// missing from `REQUIRED_CLAUDE_PROMPTS` is never installed into
/// `~/.claude/prompts/`, and the gap surfaces only mid-session as a missing
/// template at dispatch.
#[test]
fn claude_copilot_turn_templates_are_packaged() {
    const REQUIRED_CLAUDE_COPILOT_TEMPLATES: [&str; 2] = [
        "collab-turn-plan-review",
        "collab-turn-review-fix-global",
    ];

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
        let content = body(&raw);
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
    let review = read_text(".claude-plugin/prompts/collab-turn-plan-review.md");
    let accepted = accepted_verdicts();
    let anchor = "\"verdict\":\"";
    let alt_start = review
        .find(anchor)
        .map(|i| i + anchor.len())
        .unwrap_or_else(|| {
            panic!("collab-turn-plan-review.md: no `{anchor}...\"` verdict alternation found")
        });
    let alt_end = review[alt_start..]
        .find('"')
        .map(|i| alt_start + i)
        .unwrap_or_else(|| panic!("collab-turn-plan-review.md: unterminated verdict alternation"));
    let offered: BTreeSet<String> = review[alt_start..alt_end]
        .split('|')
        .map(|s| s.trim().to_owned())
        .collect();
    assert_eq!(
        offered, accepted,
        "collab-turn-plan-review.md: verdict alternation {offered:?} must match exactly the \
         accepted set {accepted:?} validated by state_machine.rs's SubmitReview handler"
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
}

/// Return the text between the first two `---` fences of a markdown file.
fn frontmatter(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Return the text after the closing `---` frontmatter fence, or the whole
/// file when there is no frontmatter block. Content checks for a *body*
/// property (a banner, a heading) must scan this, not the raw file — a token
/// stashed in frontmatter would otherwise satisfy a `contains` check without
/// the property it names ever appearing where a reader encounters it.
fn body(raw: &str) -> &str {
    match frontmatter(raw) {
        Some(front) => &raw["---".len() + front.len() + "\n---".len()..],
        None => raw,
    }
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
