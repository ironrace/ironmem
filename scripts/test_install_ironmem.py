#!/usr/bin/env python3
"""Self-test for Claude MCP registration performed by install-ironmem.sh.

The installer must put Claude's direct binary registration into trusted mode,
and a rerun must repair an older registration that is missing the mode. An
explicit user-selected mode remains untouched.
"""
from __future__ import annotations

import json
import os
import pathlib
import shutil
import stat
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install-ironmem.sh"
RELEASE_BINARY = ROOT / "target" / "release" / "ironmem"


class InstallIronmemSelfTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if shutil.which("jq") is None:
            raise unittest.SkipTest("jq is required by install-ironmem.sh")

        cls.created_fixture = False
        if not RELEASE_BINARY.exists():
            RELEASE_BINARY.parent.mkdir(parents=True, exist_ok=True)
            RELEASE_BINARY.write_text(
                "#!/bin/sh\n"
                "if [ \"$1\" = \"--version\" ]; then\n"
                "  echo 'ironmem test fixture'\n"
                "fi\n",
                encoding="utf-8",
            )
            RELEASE_BINARY.chmod(
                RELEASE_BINARY.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
            )
            cls.created_fixture = True

    @classmethod
    def tearDownClass(cls) -> None:
        if cls.created_fixture:
            RELEASE_BINARY.unlink(missing_ok=True)

    def run_installer(
        self,
        home: pathlib.Path,
        claude_config: pathlib.Path,
        *,
        skip_skills: bool = True,
        extra_env: dict[str, str] | None = None,
        expected_returncode: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        codex_home = home / ".codex"
        install_dir = home / ".ironrace" / "bin"
        command = ["bash", str(INSTALLER), "--skip-build"]
        if skip_skills:
            command.append("--skip-skills")
        result = subprocess.run(
            command,
            cwd=ROOT,
            env={
                **os.environ,
                "HOME": str(home),
                "IRONMEM_INSTALL_DIR": str(install_dir),
                "CLAUDE_CONFIG_JSON": str(claude_config),
                "CODEX_HOME": str(codex_home),
                "CODEX_CONFIG_TOML": str(codex_home / "config.toml"),
                **(extra_env or {}),
            },
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            result.returncode,
            expected_returncode,
            f"installer failed:\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        return result

    def read_claude_server(self, config: pathlib.Path) -> dict[str, object]:
        payload = json.loads(config.read_text(encoding="utf-8"))
        return payload["mcpServers"]["ironmem"]

    def test_fresh_claude_registration_uses_trusted_mode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            config = home / ".claude.json"

            self.run_installer(home, config)

            server = self.read_claude_server(config)
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")

    def test_existing_matching_registration_gets_trusted_mode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            config = home / ".claude.json"
            install_dir = home / ".ironrace" / "bin"
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "ironmem": {
                                "command": str(install_dir / "ironmem"),
                                "args": ["serve"],
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, config)

            server = self.read_claude_server(config)
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")

    def test_explicit_claude_mode_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            config = home / ".claude.json"
            install_dir = home / ".ironrace" / "bin"
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "ironmem": {
                                "command": str(install_dir / "ironmem"),
                                "args": ["serve"],
                                "env": {"IRONMEM_MCP_MODE": "read-only"},
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, config)

            server = self.read_claude_server(config)
            self.assertEqual(server["env"]["IRONMEM_MCP_MODE"], "read-only")

    def test_full_install_moves_bases_outside_discovery_roots(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            claude_home = home / "claude-home"
            codex_home = home / "codex-home"
            claude_dirs = {
                "CLAUDE_SKILLS_DIR": home / "claude-discovery" / "skills",
                "CLAUDE_AGENTS_DIR": home / "claude-discovery" / "agents",
                "CLAUDE_COMMANDS_DIR": home / "claude-discovery" / "commands",
                "CLAUDE_PROMPTS_DIR": home / "claude-discovery" / "prompts",
            }
            codex_dirs = {
                "CODEX_SKILLS_DIR": home / "codex-discovery" / "skills",
                "CODEX_COMMANDS_DIR": home / "codex-discovery" / "commands",
                "CODEX_PROMPTS_DIR": home / "codex-discovery" / "prompts",
            }
            claude_commands_dir = claude_dirs["CLAUDE_COMMANDS_DIR"]
            legacy_base = claude_commands_dir / ".ironmem-bases"
            legacy_base.mkdir(parents=True)
            (legacy_base / "collab.md").write_text("legacy merge base\n", encoding="utf-8")
            command_source = ROOT / ".claude-plugin" / "commands" / "collab.md"
            relocated_base = claude_home / ".ironmem-bases" / "commands" / "collab.md"
            relocated_base.parent.mkdir(parents=True)
            relocated_base.write_text(command_source.read_text(encoding="utf-8"), encoding="utf-8")
            (claude_commands_dir / "collab.md").write_text(
                "local command changes\n", encoding="utf-8"
            )

            self.run_installer(
                home,
                home / ".claude.json",
                skip_skills=False,
                extra_env={
                    "CLAUDE_HOME": str(claude_home),
                    "CODEX_HOME": str(codex_home),
                    **{name: str(path) for name, path in claude_dirs.items()},
                    **{name: str(path) for name, path in codex_dirs.items()},
                },
            )

            self.assertTrue((claude_commands_dir / "collab.md").is_file())
            for discovery_root in (*claude_dirs.values(), *codex_dirs.values()):
                self.assertFalse((discovery_root / ".ironmem-bases").exists())
            self.assertTrue(
                relocated_base.is_file()
            )
            self.assertEqual(relocated_base.read_text(encoding="utf-8"), command_source.read_text())
            self.assertFalse(legacy_base.exists())

    def test_full_install_keeps_legacy_bases_when_migration_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            claude_home = home / "claude-home"
            claude_commands_dir = home / "claude-discovery" / "commands"
            legacy_base = claude_commands_dir / ".ironmem-bases"
            unreadable_base = legacy_base / "collab.md"
            legacy_base.mkdir(parents=True)
            unreadable_base.write_text("legacy merge base\n", encoding="utf-8")
            unreadable_base.chmod(0)

            try:
                result = self.run_installer(
                    home,
                    home / ".claude.json",
                    skip_skills=False,
                    extra_env={
                        "CLAUDE_HOME": str(claude_home),
                        "CLAUDE_COMMANDS_DIR": str(claude_commands_dir),
                    },
                    expected_returncode=1,
                )
            finally:
                unreadable_base.chmod(stat.S_IRUSR | stat.S_IWUSR)

            self.assertIn("failed to migrate legacy install bases", result.stderr)
            self.assertTrue(legacy_base.exists())
            self.assertFalse((claude_home / ".ironmem-bases" / "commands" / "collab.md").exists())

    def _full_install_env(self, home: pathlib.Path) -> dict[str, str]:
        claude_home = home / "claude-home"
        codex_home = home / "codex-home"
        return {
            "CLAUDE_HOME": str(claude_home),
            "CODEX_HOME": str(codex_home),
            "CLAUDE_SKILLS_DIR": str(home / "claude-discovery" / "skills"),
            "CLAUDE_AGENTS_DIR": str(home / "claude-discovery" / "agents"),
            "CLAUDE_COMMANDS_DIR": str(home / "claude-discovery" / "commands"),
            "CLAUDE_PROMPTS_DIR": str(home / "claude-discovery" / "prompts"),
            "CODEX_SKILLS_DIR": str(home / "codex-discovery" / "skills"),
            "CODEX_COMMANDS_DIR": str(home / "codex-discovery" / "commands"),
            "CODEX_PROMPTS_DIR": str(home / "codex-discovery" / "prompts"),
            "CLAUDE_WORKFLOWS_DIR": str(home / "claude-discovery" / "workflows"),
        }

    def test_install_places_the_four_iron_skills(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            for root_key in ("CLAUDE_SKILLS_DIR", "CODEX_SKILLS_DIR"):
                root = pathlib.Path(env[root_key])
                for skill in ("iron-spec", "iron-plan", "iron-build", "iron-tdd"):
                    self.assertTrue(
                        (root / skill / "SKILL.md").is_file(),
                        f"{skill} missing from {root_key}",
                    )

    def test_install_places_the_ultrareview_workflow(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            installed = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"]) / "ultrareview.js"
            self.assertTrue(installed.is_file(), "ultrareview.js was not installed")
            self.assertEqual(
                installed.read_text(encoding="utf-8"),
                (ROOT / ".claude-plugin" / "workflows" / "ultrareview.js").read_text(encoding="utf-8"),
                "installed workflow drifted from the packaged copy",
            )

            snapshot = (
                pathlib.Path(env["CLAUDE_HOME"]) / ".ironmem-bases" / "workflows" / "ultrareview.js"
            )
            self.assertTrue(snapshot.is_file(), "no base snapshot was written for the workflow")

    def test_locally_modified_workflow_is_not_clobbered(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            installed = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"]) / "ultrareview.js"
            installed.write_text("// the user's own edit\n", encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            self.assertIn("the user's own edit", installed.read_text(encoding="utf-8"))

    def test_workflow_installs_to_default_path_when_dir_not_overridden(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            del env["CLAUDE_WORKFLOWS_DIR"]

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            installed = pathlib.Path(env["CLAUDE_HOME"]) / "workflows" / "ultrareview.js"
            self.assertTrue(
                installed.is_file(),
                "ultrareview.js did not land at the CLAUDE_HOME/workflows default",
            )
            self.assertEqual(
                installed.read_text(encoding="utf-8"),
                (ROOT / ".claude-plugin" / "workflows" / "ultrareview.js").read_text(encoding="utf-8"),
            )

    def test_workflow_merge_conflict_leaves_local_file_intact_with_sidecars(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            installed = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"]) / "ultrareview.js"
            base_snapshot = (
                pathlib.Path(env["CLAUDE_HOME"]) / ".ironmem-bases" / "workflows" / "ultrareview.js"
            )
            packaged = (ROOT / ".claude-plugin" / "workflows" / "ultrareview.js").read_text(
                encoding="utf-8"
            )
            lines = packaged.splitlines(keepends=True)
            self.assertGreater(len(lines), 1, "packaged workflow is too short to force a conflict")

            # Rewind the base snapshot so it diverges from the packaged copy on
            # the same line the "local edit" below touches. Without this, the
            # base still equals the packaged source and install_file_with_merge
            # takes the early "packaged copy unchanged" return -- never
            # reaching git merge-file at all.
            conflicting_base = lines.copy()
            conflicting_base[0] = "// base snapshot rewound for the test\n"
            base_snapshot.write_text("".join(conflicting_base), encoding="utf-8")

            local_edit = lines.copy()
            local_edit[0] = "// the user's own edit -- must survive intact\n"
            installed.write_text("".join(local_edit), encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            # Exact equality, not a substring check: git merge-file's conflict
            # draft embeds the local side verbatim inside a <<<<<<< block, so
            # a naive assertIn("the user's own edit", ...) would still pass
            # even if the local file got clobbered with that draft. The
            # conflict-marker check below pins the same thing from the other
            # direction.
            self.assertEqual(
                installed.read_text(encoding="utf-8"),
                "".join(local_edit),
                "a merge conflict must never touch the local file",
            )
            self.assertNotIn("<<<<<<<", installed.read_text(encoding="utf-8"))
            conflict_sidecar = installed.parent / (installed.name + ".ironmem-merge-conflict")
            packaged_sidecar = installed.parent / (installed.name + ".ironmem-packaged")
            self.assertTrue(conflict_sidecar.is_file(), "no merge-conflict sidecar was written")
            self.assertTrue(packaged_sidecar.is_file(), "no packaged-copy sidecar was written")

    def test_preexisting_user_workflow_with_no_base_snapshot_is_left_alone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            workflows_dir = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"])
            workflows_dir.mkdir(parents=True)
            theirs = workflows_dir / "ultrareview.js"
            theirs.write_text("// the user's own pre-existing workflow\n", encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", skip_skills=False, extra_env=env
            )

            self.assertEqual(result.returncode, 0)
            self.assertEqual(
                theirs.read_text(encoding="utf-8"),
                "// the user's own pre-existing workflow\n",
                "installer modified a workflow it did not install",
            )
            packaged_sidecar = theirs.parent / (theirs.name + ".ironmem-packaged")
            self.assertTrue(packaged_sidecar.is_file(), "no packaged sidecar was written")

    def test_non_directory_target_root_skips_the_kind_and_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            workflows_target = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"])
            workflows_target.parent.mkdir(parents=True)
            workflows_target.write_text("obstruction -- not a directory\n", encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", skip_skills=False, extra_env=env
            )

            self.assertTrue(
                workflows_target.is_file(),
                "an obstructing file must be left alone, not replaced with a directory",
            )
            self.assertEqual(
                workflows_target.read_text(encoding="utf-8"), "obstruction -- not a directory\n"
            )
            self.assertIn(
                "target is not a directory; nothing installed for this kind",
                result.stdout,
                "a skipped kind must be named in the end-of-run summary, not just a buried WARN",
            )
            self.assertIn("Claude workflow set", result.stdout)

    def test_superseded_skill_is_removed_when_a_base_snapshot_proves_ours(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CLAUDE_SKILLS_DIR"])
            base_dir = pathlib.Path(env["CLAUDE_HOME"]) / ".ironmem-bases" / "skills"

            installed = skills_dir / "writing-plans"
            installed.mkdir(parents=True)
            (installed / "SKILL.md").write_text("ours\n", encoding="utf-8")
            snapshot = base_dir / "writing-plans"
            snapshot.mkdir(parents=True)
            (snapshot / "SKILL.md").write_text("ours\n", encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            self.assertFalse(installed.exists(), "superseded skill was not removed")
            self.assertFalse(snapshot.exists(), "base snapshot was not removed")

    def test_user_owned_skill_is_kept_when_no_base_snapshot_exists(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CLAUDE_SKILLS_DIR"])

            theirs = skills_dir / "writing-plans"
            theirs.mkdir(parents=True)
            (theirs / "SKILL.md").write_text("the user's own copy\n", encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", skip_skills=False, extra_env=env
            )

            self.assertTrue(theirs.is_file() or theirs.is_dir())
            self.assertEqual(
                (theirs / "SKILL.md").read_text(encoding="utf-8"),
                "the user's own copy\n",
                "installer modified a skill it did not install",
            )
            self.assertIn("writing-plans", result.stderr)
            self.assertIn("no ironmem base snapshot", result.stderr)

    def test_cleanup_covers_the_codex_side_too(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CODEX_SKILLS_DIR"])
            base_dir = pathlib.Path(env["CODEX_HOME"]) / ".ironmem-bases" / "skills"

            for name in ("using-superpowers", "executing-plans"):
                (skills_dir / name).mkdir(parents=True)
                (skills_dir / name / "SKILL.md").write_text("ours\n", encoding="utf-8")
                (base_dir / name).mkdir(parents=True)
                (base_dir / name / "SKILL.md").write_text("ours\n", encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            for name in ("using-superpowers", "executing-plans"):
                self.assertFalse((skills_dir / name).exists(), f"{name} survived on the Codex side")
            self.assertTrue((skills_dir / "pr-review-toolkit" / "SKILL.md").is_file())


if __name__ == "__main__":
    unittest.main()
