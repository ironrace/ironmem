#!/usr/bin/env python3
"""Self-test for the MCP registrations performed by install-ironmem.sh.

The installer must put Claude's direct binary registration into trusted mode,
and a rerun must repair an older registration that is missing the mode. An
explicit user-selected mode remains untouched.

Muse Code shares Claude's object-shaped `mcpServers`, so it shares the writer,
but carries two things Claude does not: its settings file lives under
XDG_CONFIG_HOME rather than $HOME, and its entry needs `"mode": "optional"` --
without it Muse treats the server as required and aborts the whole session when
the command is unavailable. Both are pinned here.
"""
from __future__ import annotations

import json
import os
import pathlib
import shlex
import shutil
import stat
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts" / "install-ironmem.sh"
RELEASE_BINARY = ROOT / "target" / "release" / "ironmem"
PLUGIN_HOOKS_JSON = ROOT / ".claude-plugin" / "hooks" / "hooks.json"


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

    def setUp(self) -> None:
        # Defensive restore: test_missing_packaged_muse_skill_fails_the_run
        # renames a real packaged file aside and back in try/finally, but a
        # SIGKILL mid-test would leave the parked copy behind and break every
        # later run confusingly. Never overwrite a real file.
        skill_file = ROOT / ".muse-plugin" / "skills" / "iron-tdd" / "SKILL.md"
        parked = skill_file.with_suffix(".md.parked-for-test")
        if parked.is_file() and not skill_file.is_file():
            parked.rename(skill_file)

    def run_installer(
        self,
        home: pathlib.Path,
        claude_config: pathlib.Path,
        *,
        skip_skills: bool = True,
        skip_wiring: bool = False,
        extra_env: dict[str, str] | None = None,
        expected_returncode: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        codex_home = home / ".codex"
        install_dir = home / ".ironrace" / "bin"
        command = ["bash", str(INSTALLER), "--skip-build"]
        if skip_skills:
            command.append("--skip-skills")
        if skip_wiring:
            command.append("--skip-wiring")
        env = {
            **os.environ,
            "HOME": str(home),
            "IRONMEM_INSTALL_DIR": str(install_dir),
            "CLAUDE_CONFIG_JSON": str(claude_config),
            "CODEX_HOME": str(codex_home),
            "CODEX_CONFIG_TOML": str(codex_home / "config.toml"),
            # Pinned, not inherited: the installer now writes a Muse config,
            # and an ambient XDG_CONFIG_HOME would send it to the developer's
            # real ~/.config/muse instead of this temp home.
            "XDG_CONFIG_HOME": str(home / ".config"),
            # Pinned, not inherited: the installer now installs Muse skills
            # through the `muse` CLI, and a developer machine with Muse on
            # PATH would otherwise perform real managed-store installs on
            # every full-install test. Tests that pin the Muse argv override
            # MUSE_BIN with a recording shim.
            "MUSE_BIN": str(home / ".no-muse-here"),
            # Pinned, not inherited: the installer now writes Claude hooks
            # into $CLAUDE_HOME/settings.json.
            "CLAUDE_HOME": str(home / ".claude"),
            **(extra_env or {}),
        }
        # Never inherited: a developer's own CLAUDE_SETTINGS_JSON would send the
        # hook wiring outside the temp home. Defaults to $CLAUDE_HOME/settings.json;
        # a test can still set it through extra_env.
        if extra_env is None or "CLAUDE_SETTINGS_JSON" not in extra_env:
            env.pop("CLAUDE_SETTINGS_JSON", None)
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=env,
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
            # A pre-daemon entry must also reach the proxy command; leaving it
            # bare makes doctor report "wired with the legacy bare `serve`
            # command" right after a successful install.
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])
            self.assertEqual(len(server.get("args", [])), 3)

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

    def read_muse_server(self, config: pathlib.Path) -> dict[str, object]:
        payload = json.loads(config.read_text(encoding="utf-8"))
        return payload["mcpServers"]["ironmem"]

    def test_fresh_muse_registration_is_trusted_and_optional(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            muse_config = home / ".config" / "muse" / "settings.json"

            self.run_installer(home, home / ".claude.json")

            server = self.read_muse_server(muse_config)
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")
            # A required server whose command is unavailable aborts the whole
            # Muse session; ironmem's entry must opt out of that.
            self.assertEqual(server.get("mode"), "optional")
            self.assertEqual(server.get("command"), str(home / ".ironrace" / "bin" / "ironmem"))
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])

    def test_fresh_muse_file_carries_the_measured_schema_envelope(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            muse_config = home / ".config" / "muse" / "settings.json"

            self.run_installer(home, home / ".claude.json")

            payload = json.loads(muse_config.read_text(encoding="utf-8"))
            self.assertEqual(payload.get("schema_version"), 1)

    def test_muse_registration_honors_xdg_config_home(self) -> None:
        # Muse consults $XDG_CONFIG_HOME/muse/settings.json when the variable is
        # set and ~/.config/muse only when it is not -- never both -- so writing
        # the wrong one registers nothing at all.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            xdg = home / "xdg-elsewhere"

            self.run_installer(
                home, home / ".claude.json", extra_env={"XDG_CONFIG_HOME": str(xdg)}
            )

            self.assertTrue((xdg / "muse" / "settings.json").is_file())
            self.assertFalse((home / ".config" / "muse" / "settings.json").exists())

    def test_empty_xdg_config_home_falls_back_to_dot_config(self) -> None:
        # An empty value is not a location. The script ignores it the way the
        # Rust path table does, rather than writing to "/muse/settings.json".
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)

            self.run_installer(home, home / ".claude.json", extra_env={"XDG_CONFIG_HOME": ""})

            self.assertTrue((home / ".config" / "muse" / "settings.json").is_file())

    def test_existing_muse_entry_without_mode_gains_optional(self) -> None:
        # The hazard this repairs: an entry written before `mode` existed (by
        # hand, or by an older `ironmem muse` upgrading a bare serve line) is
        # `required` by default, so a stale command kills every Muse session.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            muse_config = home / ".config" / "muse" / "settings.json"
            muse_config.parent.mkdir(parents=True, exist_ok=True)
            muse_config.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "provider": "anthropic",
                        "mcpServers": {
                            "ironmem": {
                                "command": str(home / ".ironrace" / "bin" / "ironmem"),
                                "args": ["serve"],
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, home / ".claude.json")

            server = self.read_muse_server(muse_config)
            self.assertEqual(server.get("mode"), "optional")
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")
            # The same entry is still on the pre-daemon `["serve"]` args, so the
            # install must upgrade those too -- otherwise doctor sends the user
            # to `ironmem muse` for an upgrade the installer should have done.
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])
            self.assertEqual(len(server.get("args", [])), 3)
            # Unrelated settings survive the edit.
            payload = json.loads(muse_config.read_text(encoding="utf-8"))
            self.assertEqual(payload.get("provider"), "anthropic")

    def test_explicit_muse_mode_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            muse_config = home / ".config" / "muse" / "settings.json"
            muse_config.parent.mkdir(parents=True, exist_ok=True)
            muse_config.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "mcpServers": {
                            "ironmem": {
                                "command": str(home / ".ironrace" / "bin" / "ironmem"),
                                "args": ["serve"],
                                "mode": "required",
                                "env": {"IRONMEM_MCP_MODE": "read-only"},
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, home / ".claude.json")

            server = self.read_muse_server(muse_config)
            self.assertEqual(server["mode"], "required")
            self.assertEqual(server["env"]["IRONMEM_MCP_MODE"], "read-only")
            # Preserving deliberate settings does not mean skipping the repair
            # the entry actually needs.
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])

    def test_skip_wiring_leaves_muse_untouched(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)

            self.run_installer(home, home / ".claude.json", skip_wiring=True)

            self.assertFalse((home / ".config" / "muse" / "settings.json").exists())

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

    def test_non_directory_target_root_fails_the_install(self) -> None:
        # An obstructed target root means an entire kind of file — every Claude
        # command, or the workflow — installs nowhere. Reporting it while
        # exiting 0 is the worst of both: the summary names the skip, and any
        # automated caller gating on the exit code proceeds as though the
        # install succeeded, with /ultrareview-local absent at runtime. The
        # obstructing file must still be left alone.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            workflows_target = pathlib.Path(env["CLAUDE_WORKFLOWS_DIR"])
            workflows_target.parent.mkdir(parents=True)
            workflows_target.write_text("obstruction -- not a directory\n", encoding="utf-8")

            result = self.run_installer(
                home,
                home / ".claude.json",
                skip_skills=False,
                extra_env=env,
                expected_returncode=1,
            )

            self.assertTrue(
                workflows_target.is_file(),
                "an obstructing file must be left alone, not replaced with a directory",
            )
            self.assertEqual(
                workflows_target.read_text(encoding="utf-8"), "obstruction -- not a directory\n"
            )
            self.assertIn(
                "exists but is not a directory",
                result.stderr,
                "the obstructed path must be named on the way out",
            )
            self.assertNotIn(
                "==> Done",
                result.stdout,
                "a run that installed nothing for a kind must not report completion",
            )

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

    def _write_muse_shim(self, directory: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
        directory.mkdir(parents=True, exist_ok=True)
        log = directory / "muse-argv.log"
        shim = directory / "muse"
        shim.write_text(
            "#!/bin/sh\n"
            'echo "$@" >> "$MUSE_SHIM_LOG"\n'
            "exit ${MUSE_SHIM_EXIT:-0}\n",
            encoding="utf-8",
        )
        shim.chmod(shim.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        return shim, log

    def test_muse_skills_install_through_the_managed_store(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            shim, log = self._write_muse_shim(home / "bin")
            env["MUSE_BIN"] = str(shim)
            env["MUSE_SHIM_LOG"] = str(log)

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            source = ROOT / ".muse-plugin" / "skills"
            self.assertEqual(
                log.read_text(encoding="utf-8").splitlines(),
                [
                    f"skills install {source / skill} --scope user --force"
                    for skill in ("iron-spec", "iron-plan", "iron-build", "iron-tdd")
                ],
            )

    def test_missing_muse_binary_warns_and_installs_everything_else(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)

            result = self.run_installer(
                home, home / ".claude.json", skip_skills=False, extra_env=env
            )

            self.assertIn("skipping Muse skill registration", result.stderr)
            self.assertIn("skills install", result.stderr)
            self.assertIn(".muse-plugin/skills/iron-build", result.stderr)
            # The recovery hint must carry --force: without it the commands
            # fail skill-already-installed on any machine whose store already
            # holds the skills.
            self.assertIn("--force", result.stderr)
            self.assertTrue(
                (pathlib.Path(env["CLAUDE_SKILLS_DIR"]) / "iron-plan" / "SKILL.md").is_file()
            )
            self.assertTrue(
                (pathlib.Path(env["CODEX_SKILLS_DIR"]) / "iron-plan" / "SKILL.md").is_file()
            )
            # The warn path skips the Muse skills, not the wiring: the Muse MCP
            # entry must still land.
            muse_config = home / ".config" / "muse" / "settings.json"
            payload = json.loads(muse_config.read_text(encoding="utf-8"))
            self.assertIn("ironmem", payload["mcpServers"])
            # The skipped skills must also reach the end-of-run summary: a
            # lone mid-output WARN is easy to scroll past on a green run.
            self.assertIn("left unchanged", result.stdout)
            self.assertIn("Muse skills", result.stdout)

    def test_skip_skills_never_invokes_muse(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            shim, log = self._write_muse_shim(home / "bin")

            self.run_installer(
                home,
                home / ".claude.json",
                extra_env={"MUSE_BIN": str(shim), "MUSE_SHIM_LOG": str(log)},
            )

            self.assertFalse(log.exists())

    def test_failing_muse_install_fails_the_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            shim, log = self._write_muse_shim(home / "bin")
            env["MUSE_BIN"] = str(shim)
            env["MUSE_SHIM_LOG"] = str(log)
            env["MUSE_SHIM_EXIT"] = "1"

            result = self.run_installer(
                home,
                home / ".claude.json",
                skip_skills=False,
                extra_env=env,
                expected_returncode=1,
            )

            self.assertIn("failed to install Muse skill iron-spec", result.stderr)
            self.assertIn("Retry with:", result.stderr)
            # Muse runs last in the skills block, so every other harness's
            # file installs already landed -- but MCP wiring below it must
            # not run for a kind that installed nothing.
            agents_dir = pathlib.Path(env["CLAUDE_AGENTS_DIR"])
            self.assertTrue(
                (agents_dir / "code-reviewer.md").is_file(),
                "a Muse failure must not skip other harnesses' file installs",
            )
            self.assertFalse(
                (home / ".claude.json").exists(),
                "a Muse failure must fail before MCP wiring runs",
            )

    def test_missing_packaged_muse_skill_fails_the_run(self) -> None:
        # validate_packaged_skills is the preflight for the managed-store
        # installs: with a SKILL.md missing from the packaged tree the run
        # must die naming the skill before any managed-store install runs,
        # not after three skills landed. (Other harnesses' file installs land
        # first -- Muse runs last -- so this guards the store, not the run.)
        skill_file = ROOT / ".muse-plugin" / "skills" / "iron-tdd" / "SKILL.md"
        parked = skill_file.with_suffix(".md.parked-for-test")
        skill_file.rename(parked)
        try:
            with tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                env = self._full_install_env(home)
                shim, log = self._write_muse_shim(home / "bin")
                env["MUSE_BIN"] = str(shim)
                env["MUSE_SHIM_LOG"] = str(log)

                result = self.run_installer(
                    home,
                    home / ".claude.json",
                    skip_skills=False,
                    extra_env=env,
                    expected_returncode=1,
                )

                self.assertIn("bundled Muse skill missing", result.stderr)
                self.assertIn("iron-tdd", result.stderr)
                self.assertFalse(
                    log.exists(), "no managed-store install may run on a broken package"
                )
        finally:
            parked.rename(skill_file)
        self.assertTrue(skill_file.is_file(), "packaged tree was not restored")

    def test_muse_binary_resolves_from_path_by_default(self) -> None:
        # Every other Muse test pins MUSE_BIN, so the production default --
        # a bare `muse` resolved from PATH -- would otherwise run nowhere.
        # An empty MUSE_BIN falls back to `muse` exactly like an unset one
        # (`${MUSE_BIN:-muse}`), which is what makes this expressible as an
        # env override at all.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            shim, log = self._write_muse_shim(home / "bin")
            env["MUSE_BIN"] = ""
            env["MUSE_SHIM_LOG"] = str(log)
            env["PATH"] = str(home / "bin") + os.pathsep + os.environ["PATH"]

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            self.assertEqual(len(log.read_text(encoding="utf-8").splitlines()), 4)

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

    def claude_settings(self, home: pathlib.Path) -> dict[str, object]:
        return json.loads((home / ".claude" / "settings.json").read_text(encoding="utf-8"))

    @staticmethod
    def ironmem_hook_commands(settings: dict, event: str) -> list[str]:
        return [
            hook.get("command", "")
            for group in settings.get("hooks", {}).get(event, [])
            for hook in group.get("hooks", [])
            if "ironmem" in hook.get("command", "")
        ]

    @staticmethod
    def plugin_hook_names() -> dict[str, str]:
        plugin = json.loads(PLUGIN_HOOKS_JSON.read_text(encoding="utf-8"))["hooks"]
        return {event: groups[0]["hooks"][0]["command"].split()[-1] for event, groups in plugin.items()}

    def test_fresh_install_wires_the_plugin_hooks_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            result = self.run_installer(home, home / ".claude.json")

            settings = self.claude_settings(home)
            target = home / ".ironrace" / "bin" / "ironmem"
            names = self.plugin_hook_names()
            self.assertEqual(set(settings["hooks"]), set(names), "same events as the plugin")
            for event, name in names.items():
                self.assertEqual(
                    self.ironmem_hook_commands(settings, event),
                    [f"{target} hook {name} --harness claude-code"],
                )
            self.assertNotIn("IRONMEM_MCP_MODE", json.dumps(settings), "decided: hooks stay read-only")
            self.assertIn("Wired Claude hooks", result.stdout)
            self.assertIn("skip mining", result.stdout)

    def test_rerun_does_not_duplicate_hooks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            self.run_installer(home, home / ".claude.json")
            result = self.run_installer(home, home / ".claude.json")

            settings = self.claude_settings(home)
            for event in self.plugin_hook_names():
                self.assertEqual(len(self.ironmem_hook_commands(settings, event)), 1, event)
            self.assertIn("already wired", result.stdout)

    def assert_hooks_run_from_an_unsafe_install_dir(self, dir_name: str) -> None:
        """Install into a directory whose path the shell would split or unquote,
        then check each written hook command is one argv that runs the binary."""
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            install_dir = home / dir_name / "bin"
            env = {"IRONMEM_INSTALL_DIR": str(install_dir)}
            first = self.run_installer(home, home / ".claude.json", extra_env=env)
            self.assertIn("Wired Claude hooks", first.stdout)

            target = install_dir / "ironmem"
            # A recording stub in the installed binary's place, so running a
            # hook command proves what argv the shell hands the binary.
            record = home / "argv.txt"
            target.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > {shlex.quote(str(record))}\n', encoding="utf-8")
            target.chmod(0o755)

            settings = self.claude_settings(home)
            for event, name in self.plugin_hook_names().items():
                with self.subTest(event=event):
                    commands = self.ironmem_hook_commands(settings, event)
                    self.assertEqual(len(commands), 1, commands)
                    expected = ["hook", name, "--harness", "claude-code"]
                    self.assertEqual(shlex.split(commands[0]), [str(target), *expected])
                    record.unlink(missing_ok=True)
                    ran = subprocess.run(["sh", "-c", commands[0]], capture_output=True, text=True)
                    self.assertEqual(ran.returncode, 0, ran.stderr)
                    self.assertEqual(record.read_text(encoding="utf-8").splitlines(), expected)

            second = self.run_installer(home, home / ".claude.json", extra_env=env)
            self.assertEqual(self.claude_settings(home), settings, "a re-run adds nothing")
            self.assertNotIn("Wired Claude hooks", second.stdout)
            self.assertIn(
                "Claude hooks already wired, left as they are: SessionStart UserPromptSubmit Stop PreCompact",
                second.stdout,
            )

    def test_hooks_run_from_an_install_dir_with_a_space(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("Jo Smith")

    def test_hooks_run_from_an_install_dir_with_a_single_quote(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("it's here")

    def test_without_jq_the_printed_hook_command_is_quoted(self) -> None:
        # A space needs shell quoting; a `"` or `\` also needs JSON escaping.
        for dir_name in ("Jo Smith", 'Jo "JJ" \\ Smith'):
            with self.subTest(dir_name=dir_name), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                path = self.path_without_jq(home / "no-jq-bin")
                install_dir = home / dir_name / "bin"
                env = {"PATH": path, "IRONMEM_INSTALL_DIR": str(install_dir)}

                result = self.run_installer(home, home / ".claude.json", extra_env=env)

                [line] = [l for l in result.stderr.splitlines() if '"<Event>"' in l]
                command = json.loads("{" + line.strip() + "}")["<Event>"][0]["hooks"][0]["command"]
                self.assertEqual(
                    shlex.split(command),
                    [
                        str(install_dir / "ironmem"),
                        "hook",
                        "<session-start|user-prompt-submit|stop|precompact>",
                        "--harness",
                        "claude-code",
                    ],
                )

    def test_existing_hooks_and_keys_are_kept(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            settings_path = home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True)
            settings_path.write_text(
                json.dumps(
                    {
                        "model": "opus",
                        "hooks": {
                            "SessionStart": [
                                {"hooks": [{"type": "command", "command": "~/.ironrace/bin/ironmem hook session-start --harness claude-code"}]}
                            ],
                            "Stop": [{"hooks": [{"type": "command", "command": "~/.claude/scripts/reply-rules.sh"}]}],
                            "Notification": [{"hooks": [{"type": "command", "command": "say hi"}]}],
                        },
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, home / ".claude.json")

            settings = self.claude_settings(home)
            self.assertEqual(settings["model"], "opus")
            self.assertEqual(settings["hooks"]["Notification"], [{"hooks": [{"type": "command", "command": "say hi"}]}])
            # The hand-wired SessionStart (different path) is recognized, not duplicated.
            self.assertEqual(
                self.ironmem_hook_commands(settings, "SessionStart"),
                ["~/.ironrace/bin/ironmem hook session-start --harness claude-code"],
            )
            stop_commands = [h["command"] for g in settings["hooks"]["Stop"] for h in g["hooks"]]
            self.assertEqual(stop_commands[0], "~/.claude/scripts/reply-rules.sh")
            self.assertEqual(len(self.ironmem_hook_commands(settings, "Stop")), 1)

    def test_enabled_plugin_gets_no_hooks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            settings_path = home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True)
            original = json.dumps({"enabledPlugins": {"ironmem@ironrace": True}})
            settings_path.write_text(original, encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(settings_path.read_text(encoding="utf-8"), original)
            self.assertIn("enabled ironmem plugin", result.stdout)

    def test_malformed_hooks_are_left_alone_with_a_warning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            settings_path = home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True)
            original = json.dumps({"hooks": {"Stop": {"not": "an array"}}})
            settings_path.write_text(original, encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(settings_path.read_text(encoding="utf-8"), original)
            self.assertIn("wired no Claude hooks", result.stderr)

    def test_symlinked_settings_stays_a_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            real = home / "dotfiles" / "settings.json"
            real.parent.mkdir(parents=True)
            real.write_text("{}", encoding="utf-8")
            link = home / ".claude" / "settings.json"
            link.parent.mkdir(parents=True)
            link.symlink_to(real)

            self.run_installer(home, home / ".claude.json")

            self.assertTrue(link.is_symlink(), "a dotfiles symlink must survive the install")
            self.assertIn("hooks", json.loads(real.read_text(encoding="utf-8")))

    def test_skip_wiring_leaves_claude_settings_absent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            self.run_installer(home, home / ".claude.json", skip_wiring=True)
            self.assertFalse((home / ".claude" / "settings.json").exists())

    def stop_commands_after_install(self, existing: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            settings_path = home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True)
            settings_path.write_text(
                json.dumps({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": existing}]}]}}),
                encoding="utf-8",
            )
            self.run_installer(home, home / ".claude.json")
            target = str(home / ".ironrace" / "bin" / "ironmem")
            commands = self.ironmem_hook_commands(self.claude_settings(home), "Stop")
            return [command.replace(target, "<target>") for command in commands]

    def test_quoted_program_path_counts_as_wired(self) -> None:
        for command in (
            '"$HOME/.ironrace/bin/ironmem" hook stop --harness claude-code',
            "'/opt/ironmem' hook stop --harness claude-code",
            '"ironmem" hook stop --harness claude-code',
            "'ironmem' hook stop --harness claude-code",
            'bash -c "ironmem hook stop"',
            "bash -c 'ironmem hook stop'",
            'bash -c "ironmem-hook.sh stop"',
        ):
            with self.subTest(command=command):
                self.assertEqual(self.stop_commands_after_install(command), [command])

    def test_shell_terminator_after_the_hook_name_counts_as_wired(self) -> None:
        for tail in ("; true", " && true", " || true", " | cat", " &", ")", '"', "'", ">/dev/null", "</dev/null"):
            command = f"~/.ironrace/bin/ironmem hook stop{tail}"
            with self.subTest(command=command):
                self.assertEqual(self.stop_commands_after_install(command), [command])
        wrapper = "bash /p/hooks/ironmem-hook.sh stop; true"
        self.assertEqual(self.stop_commands_after_install(wrapper), [wrapper])

    def test_a_longer_hook_name_does_not_count_as_wired(self) -> None:
        for command in (
            "~/.ironrace/bin/ironmem hook stopx --harness claude-code",
            "~/.ironrace/bin/ironmem hook stop-foo --harness claude-code",
            "notironmem hook stop",
            "my-ironmem hook stop",
            "bash /x/notironmem-hook.sh stop",
        ):
            with self.subTest(command=command):
                own = "<target> hook stop --harness claude-code"
                self.assertEqual(self.stop_commands_after_install(command), [command, own])

    def settings_with_stop(self, home: pathlib.Path, stop: object) -> pathlib.Path:
        path = home / ".claude" / "settings.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"hooks": {"Stop": stop}}), encoding="utf-8")
        return path

    @staticmethod
    def command_stub(home: pathlib.Path, name: str, fail_when: str, on_match: str) -> dict[str, str]:
        """PATH env whose `name` runs `on_match` when the joined argv contains every
        `|`-separated literal in `fail_when`, and otherwise runs the real `name`."""
        bin_dir = home / "stub-bin"
        bin_dir.mkdir(exist_ok=True)
        checks = " && ".join(f'[[ "$*" == *{shlex.quote(word)}* ]]' for word in fail_when.split("|"))
        stub = bin_dir / name
        stub.write_text(
            f'#!/usr/bin/env bash\nif {checks}; then {on_match}; fi\nexec {shutil.which(name)} "$@"\n',
            encoding="utf-8",
        )
        stub.chmod(0o755)
        return {"PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}"}

    def jq_stub(self, home: pathlib.Path, fail_when: str) -> dict[str, str]:
        """PATH env whose `jq` exits 5 on the calls `fail_when` picks out (see
        command_stub) and otherwise runs the real jq."""
        return self.command_stub(home, "jq", fail_when, 'echo "stub jq failure" >&2; exit 5')

    @unittest.skipIf(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        "precondition: root writes through mode 0444, so a read-only file cannot be made here",
    )
    def test_read_only_settings_warns_and_the_install_continues(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            path.chmod(0o444)

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(path.read_text(encoding="utf-8"), before)
            self.assertIn("WARN", result.stderr)
            self.assertIn("wired no Claude hooks", result.stderr)
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_a_jq_failure_while_appending_writes_nothing_and_continues(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            env = self.jq_stub(home, "[{hooks|UserPromptSubmit")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_text(encoding="utf-8"), before, "written at most once, after all events")
            self.assertIn("wired no Claude hooks", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)
            self.assertTrue((home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_a_jq_error_in_the_matcher_is_not_read_as_not_wired(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            # Only the Stop matcher fails, after SessionStart and UserPromptSubmit
            # were already appended to the temp copy: nothing may reach the file.
            env = self.jq_stub(home, "--arg ev Stop --arg name stop ")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_text(encoding="utf-8"), before)
            self.assertIn(f"WARN: jq failed reading the Stop hooks in {path}; wired no Claude hooks.", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_a_jq_error_reading_enabled_plugins_is_not_read_as_disabled(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            env = self.jq_stub(home, "enabledPlugins")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_text(encoding="utf-8"), before)
            self.assertIn(f"WARN: jq failed reading enabledPlugins in {path}; wired no Claude hooks.", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_a_failed_temp_file_move_wires_nothing_and_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            env = self.command_stub(home, "mv", "ironmem-hooks", 'echo "stub mv failure" >&2; exit 1')

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_text(encoding="utf-8"), before)
            self.assertIn(f"WARN: could not stage the SessionStart hook for {path}; wired no Claude hooks.", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_a_failed_final_write_names_the_file_it_may_have_cut_short(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            # Writes three bytes of the merged file, then fails, like a full disk.
            env = self.command_stub(home, "cat", "ironmem-hooks", 'head -c 3 "$1"; exit 1')

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            warning = f"WARN: the write to {path} did not finish, so it may be incomplete. Check it, then re-run this script."
            self.assertIn(warning, result.stderr)
            # Some bytes may have landed, so it must not claim nothing was wired.
            line = next(line for line in result.stderr.splitlines() if "did not finish" in line)
            self.assertNotIn("wired no Claude hooks", line)
            self.assertNotIn("wired no Claude hooks", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_a_failed_dirname_creates_no_settings_and_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            # Fails only for the settings path. It still prints the real answer,
            # so the dirname status check, not the mkdir after it, has to catch it.
            real = shutil.which("dirname")
            env = self.command_stub(home, "dirname", str(path), f'{real} "$@"; exit 1')

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn(f"WARN: cannot create {path}; wired no Claude hooks.", result.stderr)
            self.assertFalse(path.exists())
            self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_a_failed_temp_cleanup_warns_and_the_install_continues(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            # TMPDIR inside the temp home, so the directory rm leaves is cleaned up.
            env = {
                **self.command_stub(home, "rm", "ironmem-hooks", 'echo "stub rm failure" >&2; exit 1'),
                "TMPDIR": str(home / "tmp"),
            }
            (home / "tmp").mkdir()

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn(f"WARN: could not remove the temp directory {home / 'tmp'}/ironmem-hooks.", result.stderr)
            self.assertEqual(set(self.claude_settings(home)["hooks"]), set(self.plugin_hook_names()))
            self.assertIn("Wired Claude hooks", result.stdout)
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_junk_entries_neither_abort_nor_multiply_hooks(self) -> None:
        junk_shapes = (
            "oops",
            {"matcher": "x"},
            {"hooks": [{"type": "command", "command": 5}]},
        )
        for junk in junk_shapes:
            with self.subTest(junk=junk), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                self.settings_with_stop(home, [junk])

                self.run_installer(home, home / ".claude.json")
                self.run_installer(home, home / ".claude.json")

                stop = self.claude_settings(home)["hooks"]["Stop"]
                self.assertEqual(stop[0], junk, "junk left untouched")
                self.assertEqual(len(stop), 2, stop)
                self.assertIn("hook stop --harness claude-code", json.dumps(stop[1]))

    def test_dangling_settings_symlink_warns_and_the_install_continues(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            link = home / ".claude" / "settings.json"
            link.parent.mkdir(parents=True)
            link.symlink_to(home / "no-such-dir" / "settings.json")

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn("wired no Claude hooks", result.stderr)
            self.assertFalse((home / "no-such-dir").exists())

    def test_a_disabled_plugin_still_gets_hooks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"enabledPlugins": {"ironmem@ironrace": False}}), encoding="utf-8")

            self.run_installer(home, home / ".claude.json")

            self.assertEqual(set(self.claude_settings(home)["hooks"]), set(self.plugin_hook_names()))

    def test_skip_mining_note_only_when_a_mining_hook_was_added(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            wrapper = "bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh"
            hooks = {
                event: [{"hooks": [{"type": "command", "command": f"{wrapper} {name}"}]}]
                for event, name in self.plugin_hook_names().items()
                if event != "SessionStart"
            }
            path.write_text(json.dumps({"hooks": hooks}), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn("Wired Claude hooks", result.stdout)
            self.assertIn("SessionStart", result.stdout)
            self.assertNotIn("skip mining", result.stdout)

    @staticmethod
    def path_without_jq(bin_dir: pathlib.Path) -> str:
        """A PATH of one directory linking every executable on the real PATH but jq,
        each name resolving to the same program it does now."""
        bin_dir.mkdir()
        for entry in os.environ["PATH"].split(os.pathsep):
            try:
                programs = sorted(pathlib.Path(entry).iterdir()) if entry else []
            except OSError:
                continue
            for program in programs:
                link = bin_dir / program.name
                if program.name == "jq" or link.is_symlink():
                    continue
                try:
                    runnable = program.is_file() and os.access(program, os.X_OK)
                except OSError:
                    continue
                if runnable:
                    link.symlink_to(program)
        return str(bin_dir)

    def test_without_jq_the_hooks_are_printed_not_wired(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.path_without_jq(home / "no-jq-bin")
            self.assertIsNone(shutil.which("jq", path=path), "jq must be unreachable")

            result = self.run_installer(home, home / ".claude.json", extra_env={"PATH": path})

            target = home / ".ironrace" / "bin" / "ironmem"
            self.assertIn("jq not installed", result.stderr)
            self.assertIn(f"Claude Code hooks were not wired either. Add these four to {home / '.claude' / 'settings.json'}", result.stderr)
            self.assertIn(
                f'"<Event>": [ {{ "hooks": [ {{ "type": "command", "command": "{target} hook '
                '<session-start|user-prompt-submit|stop|precompact> --harness claude-code" } ] } ]',
                result.stderr,
            )
            self.assertFalse((home / ".claude" / "settings.json").exists())


if __name__ == "__main__":
    unittest.main()
