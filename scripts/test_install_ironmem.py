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
            self.assertTrue(
                (pathlib.Path(env["CLAUDE_SKILLS_DIR"]) / "iron-plan" / "SKILL.md").is_file()
            )
            self.assertTrue(
                (pathlib.Path(env["CODEX_SKILLS_DIR"]) / "iron-plan" / "SKILL.md").is_file()
            )

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
