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

Antigravity (agy) gets an MCP entry and a global `ironmem` hook group, and only
when `agy` is on PATH. Every agy test resolves a stub `agy` from PATH that
records any invocation, and asserts it was never run: the installer only looks
agy up. Every installer run pins AGY_CONFIG_JSON, AGY_HOOKS_JSON and AGY_BIN so
no test can reach a developer's real ~/.gemini or real agy.
"""
from __future__ import annotations

import json
import os
import pathlib
import re
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


# Stop hook entries, shell and exec form, and whether each runs `ironmem hook
# stop`. Doctor's unit tests (crates/ironmem/src/doctor/hooks.rs,
# `stop_fixtures`) read the same file, so the installer's
# `_claude_hook_is_wired` and doctor are pinned to one answer per row.
HOOK_STOP_FIXTURES = ROOT / "scripts" / "fixtures" / "hook_stop_fixtures.json"


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
        skip_hooks: bool = False,
        force_wiring: bool = False,
        extra_env: dict[str, str] | None = None,
        expected_returncode: int = 0,
        cwd: pathlib.Path = ROOT,
    ) -> subprocess.CompletedProcess[str]:
        codex_home = home / ".codex"
        install_dir = home / ".ironrace" / "bin"
        command = ["bash", str(INSTALLER), "--skip-build"]
        if skip_skills:
            command.append("--skip-skills")
        if skip_wiring:
            command.append("--skip-wiring")
        if force_wiring:
            command.append("--force-wiring")
        if skip_hooks:
            command.append("--skip-hooks")
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
            # Pinned, not inherited: an exported AGY_CONFIG_JSON or
            # AGY_HOOKS_JSON would send every run's agy wiring to the
            # developer's real, token-bearing ~/.gemini/config files.
            "AGY_CONFIG_JSON": str(home / ".gemini" / "config" / "mcp_config.json"),
            "AGY_HOOKS_JSON": str(home / ".gemini" / "config" / "hooks.json"),
            # Pinned to a path that does not exist, so agy reads as absent
            # even on a machine that has it. Tests that wire agy use
            # agy_env(), which resolves a stub from PATH instead.
            "AGY_BIN": str(home / ".no-agy-here"),
            **(extra_env or {}),
        }
        # Never inherited: a developer's own CLAUDE_SETTINGS_JSON would send the
        # hook wiring outside the temp home. Defaults to $CLAUDE_HOME/settings.json;
        # a test can still set it through extra_env.
        if extra_env is None or "CLAUDE_SETTINGS_JSON" not in extra_env:
            env.pop("CLAUDE_SETTINGS_JSON", None)
        result = subprocess.run(
            command,
            cwd=cwd,
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

    def read_agy_server(self, config: pathlib.Path) -> dict[str, object]:
        payload = json.loads(config.read_text(encoding="utf-8"))
        return payload["mcpServers"]["ironmem"]

    def agy_env(self, home: pathlib.Path) -> dict[str, str]:
        """Env that puts a stub `agy` first on PATH, resolved by name.

        AGY_BIN is emptied so the installer falls back to a bare `agy`, which
        must come from PATH. The stub records any invocation; the installer
        only looks agy up, so assert_agy_never_ran checks that log stays absent.
        """
        stub_dir = home / "agy-stub-bin"
        stub_dir.mkdir(parents=True, exist_ok=True)
        stub = stub_dir / "agy"
        stub.write_text(
            "#!/bin/sh\n"
            f"echo \"$@\" >> '{home / 'agy-stub-ran.log'}'\n"
            "exit 97\n",
            encoding="utf-8",
        )
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        return {"AGY_BIN": "", "PATH": str(stub_dir) + os.pathsep + os.environ["PATH"]}

    def assert_agy_never_ran(self, home: pathlib.Path) -> None:
        self.assertFalse(
            (home / "agy-stub-ran.log").exists(), "the installer must never run agy"
        )

    def agy_paths(self, home: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
        config_dir = home / ".gemini" / "config"
        return config_dir / "mcp_config.json", config_dir / "hooks.json"

    def expected_hook_command(self, home: pathlib.Path, mode: str, subcommand: str) -> str:
        target = home / ".ironrace" / "bin" / "ironmem"
        return f"IRONMEM_MCP_MODE={mode} {target} hook {subcommand} --harness agy"

    def ironmem_hook_group(self, home: pathlib.Path, mode: str = "trusted") -> dict[str, object]:
        return {
            "SessionStart": [
                {
                    "type": "command",
                    "command": self.expected_hook_command(home, mode, "session-start"),
                    "timeout": 120,
                }
            ],
            "PreInvocation": [
                {
                    "type": "command",
                    "command": self.expected_hook_command(home, mode, "user-prompt-submit"),
                }
            ],
            "Stop": [
                {
                    "type": "command",
                    "command": self.expected_hook_command(home, mode, "stop"),
                    "timeout": 120,
                }
            ],
        }

    def stray_files(self, directory: pathlib.Path, *keep: str) -> list[str]:
        return sorted(p.name for p in directory.iterdir() if p.name not in keep)

    def test_fresh_agy_registration_uses_trusted_mode_and_ignores_xdg(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            xdg = home / "xdg-elsewhere"
            agy_config, _ = self.agy_paths(home)

            self.run_installer(
                home,
                home / ".claude.json",
                extra_env={**self.agy_env(home), "XDG_CONFIG_HOME": str(xdg)},
            )

            self.assertTrue(agy_config.is_file())
            server = self.read_agy_server(agy_config)
            self.assertEqual(server.get("command"), str(home / ".ironrace" / "bin" / "ironmem"))
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])
            self.assertEqual(len(server.get("args", [])), 3)
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")
            self.assertNotIn("mode", server)
            self.assert_agy_never_ran(home)

    def test_agy_absent_from_path_skips_agy_wiring(self) -> None:
        # The production lookup: a bare `agy` on PATH. Every directory that
        # holds an `agy` (a developer's real install included) is dropped from
        # PATH, so this run sees a machine without Antigravity.
        path = os.pathsep.join(
            d
            for d in os.environ["PATH"].split(os.pathsep)
            if d and not (pathlib.Path(d) / "agy").exists()
        )
        if shutil.which("jq", path=path) is None or shutil.which("bash", path=path) is None:
            self.skipTest("jq or bash lives only beside agy on this PATH")
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)

            result = self.run_installer(
                home, home / ".claude.json", extra_env={"AGY_BIN": "", "PATH": path}
            )

            self.assertIn("Antigravity (agy) not found on PATH", result.stdout)
            self.assertIn("skipping Antigravity MCP and hook registration", result.stdout)
            # Not just the two files: no ~/.gemini tree at all for a machine
            # that never ran agy.
            self.assertFalse((home / ".gemini").exists())
            # Everything else is still wired.
            self.assertIn("ironmem", json.loads((home / ".claude.json").read_text())["mcpServers"])
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text())

    def test_agy_on_path_wires_both_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)

            result = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertNotIn("not found on PATH", result.stdout)
            self.assertIn("ironmem", json.loads(agy_config.read_text())["mcpServers"])
            self.assertEqual(
                json.loads(hooks_path.read_text()), {"ironmem": self.ironmem_hook_group(home)}
            )
            self.assert_agy_never_ran(home)

    def test_existing_agy_bare_serve_is_upgraded_preserving_siblings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, _ = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True, exist_ok=True)
            agy_config.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "other": {"command": "/bin/other", "args": ["run"]},
                            "ironmem": {
                                "command": str(home / ".ironrace" / "bin" / "ironmem"),
                                "args": ["serve"],
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            payload = json.loads(agy_config.read_text(encoding="utf-8"))
            self.assertEqual(payload["mcpServers"]["other"]["command"], "/bin/other")
            server = payload["mcpServers"]["ironmem"]
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])
            self.assertEqual(server.get("env", {}).get("IRONMEM_MCP_MODE"), "trusted")

    def test_unrelated_agy_mcp_entries_survive_every_write_path(self) -> None:
        # mcp_config.json holds other servers' credentials. Whichever path the
        # installer takes -- fresh add, in-place repair, or --force-wiring over
        # a divergent entry -- everything except mcpServers.ironmem must come
        # back equal in value, the other server's env token included.
        siblings = {
            "theme": "dark",
            "mcpServers": {
                "github": {
                    "command": "/opt/github-mcp",
                    "args": ["stdio", "--read-only"],
                    "env": {"GITHUB_TOKEN": "ghp_fixture_token_value", "LOG": "1"},
                }
            },
        }
        cases = {
            "fresh add": (None, False),
            "repair": ({"command": "{target}", "args": ["serve"]}, False),
            "force over divergent": ({"command": "/elsewhere/ironmem", "args": ["x"]}, True),
        }
        for name, (ironmem_entry, force) in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                target = str(home / ".ironrace" / "bin" / "ironmem")
                agy_config, _ = self.agy_paths(home)
                agy_config.parent.mkdir(parents=True)
                before = json.loads(json.dumps(siblings))
                if ironmem_entry is not None:
                    entry = {k: (target if v == "{target}" else v) for k, v in ironmem_entry.items()}
                    before["mcpServers"]["ironmem"] = entry
                agy_config.write_text(json.dumps(before, indent=2), encoding="utf-8")

                self.run_installer(
                    home, home / ".claude.json", force_wiring=force, extra_env=self.agy_env(home)
                )

                after = json.loads(agy_config.read_text(encoding="utf-8"))
                server = after["mcpServers"].pop("ironmem")
                self.assertEqual(server["command"], target)
                self.assertEqual(after, siblings)

    def test_divergent_agy_mcp_command_is_kept_without_force_wiring(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, _ = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            original = json.dumps(
                {"mcpServers": {"ironmem": {"command": "/elsewhere/ironmem", "args": ["serve"]}}}
            )
            agy_config.write_text(original, encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertEqual(agy_config.read_text(encoding="utf-8"), original)
            self.assertIn("command=/elsewhere/ironmem", result.stderr)
            self.assertIn("--force-wiring", result.stderr)

    def test_explicit_agy_mode_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, _ = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True, exist_ok=True)
            agy_config.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "ironmem": {
                                "command": str(home / ".ironrace" / "bin" / "ironmem"),
                                "args": ["serve"],
                                "env": {"IRONMEM_MCP_MODE": "read-only"},
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            server = self.read_agy_server(agy_config)
            self.assertEqual(server["env"]["IRONMEM_MCP_MODE"], "read-only")
            self.assertEqual(server.get("args", [])[:2], ["serve", "--connect"])

    def test_restricted_agy_mcp_mode_flows_into_the_hooks(self) -> None:
        # A restricted agy MCP entry exists to keep stored drawer content out
        # of Antigravity. Trusted-mode hooks would inject recall excerpts on
        # every PreInvocation and mine on every Stop regardless.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            agy_config.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "ironmem": {
                                "command": str(home / ".ironrace" / "bin" / "ironmem"),
                                "args": ["serve"],
                                "env": {"IRONMEM_MCP_MODE": "restricted"},
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            self.run_installer(
                home,
                home / ".claude.json",
                # An ambient trusted must not beat the entry's explicit mode.
                extra_env={**self.agy_env(home), "IRONMEM_MCP_MODE": "trusted"},
            )

            group = json.loads(hooks_path.read_text(encoding="utf-8"))["ironmem"]
            self.assertEqual(group, self.ironmem_hook_group(home, mode="restricted"))
            for event in ("SessionStart", "PreInvocation", "Stop"):
                self.assertNotIn("trusted", group[event][0]["command"])

    def test_unreadable_agy_mcp_config_leaves_hooks_on_the_ambient_mode(self) -> None:
        # No readable mode on the agy MCP entry: the hooks defer to the mode in
        # agy's environment when they run, else trusted -- never a hard-coded
        # trusted that overrides an ambient restricted.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            agy_config.write_text("[1, 2]\n", encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertEqual(agy_config.read_text(encoding="utf-8"), "[1, 2]\n")
            self.assertIn(f"{agy_config} is not a single JSON object", result.stderr)
            group = json.loads(hooks_path.read_text(encoding="utf-8"))["ironmem"]
            self.assertEqual(
                group,
                self.ironmem_hook_group(home, mode='"${IRONMEM_MCP_MODE:-trusted}"'),
            )
            # What that prefix does once agy's `sh -c` runs it.
            command = group["PreInvocation"][0]["command"].replace(
                str(home / ".ironrace" / "bin" / "ironmem"), "printenv IRONMEM_MCP_MODE #"
            )
            for ambient, expected in ((None, "trusted"), ("restricted", "restricted")):
                env = {k: v for k, v in os.environ.items() if k != "IRONMEM_MCP_MODE"}
                if ambient is not None:
                    env["IRONMEM_MCP_MODE"] = ambient
                ran = subprocess.run(
                    ["sh", "-c", command], env=env, capture_output=True, text=True, check=True
                )
                self.assertEqual(ran.stdout.strip(), expected)

    def test_hook_command_quotes_an_install_path_with_shell_metacharacters(self) -> None:
        # agy runs the hook command through `sh -c`; a `$`, backtick, quote or
        # space in the install path must reach the binary verbatim.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            install_dir = home / "odd dir $HOME `id` it's" / "bin"
            _, hooks_path = self.agy_paths(home)

            self.run_installer(
                home,
                home / ".claude.json",
                extra_env={**self.agy_env(home), "IRONMEM_INSTALL_DIR": str(install_dir)},
            )

            command = json.loads(hooks_path.read_text(encoding="utf-8"))["ironmem"]["Stop"][0][
                "command"
            ]
            ran = subprocess.run(
                ["sh", "-c", f'set -- {command}; printf "%s\\n" "$@"'],
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(
                ran.stdout.splitlines(),
                ["IRONMEM_MCP_MODE=trusted", str(install_dir / "ironmem"), "hook", "stop",
                 "--harness", "agy"],
            )

    def test_skip_wiring_leaves_agy_untouched(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)

            self.run_installer(
                home, home / ".claude.json", skip_wiring=True, extra_env=self.agy_env(home)
            )

            self.assertFalse((home / ".gemini").exists())

    def test_fresh_agy_hooks_registration_and_idempotency(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            _, hooks_path = self.agy_paths(home)
            hooks_path.parent.mkdir(parents=True, exist_ok=True)
            # Pre-existing sibling hook must survive untouched.
            hooks_path.write_text(
                json.dumps(
                    {
                        "lint-checker": {
                            "Stop": [{"type": "command", "command": "./lint.sh"}]
                        }
                    }
                ),
                encoding="utf-8",
            )

            first = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertIn("Registering 'ironmem' lifecycle hooks", first.stdout)
            payload = json.loads(hooks_path.read_text(encoding="utf-8"))
            self.assertEqual(
                payload["lint-checker"], {"Stop": [{"type": "command", "command": "./lint.sh"}]}
            )
            ironmem = payload["ironmem"]
            target = str(home / ".ironrace" / "bin" / "ironmem")
            for event, subcmd in (
                ("SessionStart", "session-start"),
                ("PreInvocation", "user-prompt-submit"),
                ("Stop", "stop"),
            ):
                cmd = ironmem[event][0]["command"]
                self.assertIn(target, cmd)
                self.assertIn(f"hook {subcmd} --harness agy", cmd)
                self.assertIn("IRONMEM_MCP_MODE=trusted", cmd)
            # agy kills a hook at 30 s by default; session-start and Stop can
            # mine the workspace, so they carry a longer explicit budget.
            self.assertEqual(ironmem["SessionStart"][0]["timeout"], 120)
            self.assertEqual(ironmem["Stop"][0]["timeout"], 120)
            self.assertNotIn("timeout", ironmem["PreInvocation"][0])

            # Second run is idempotent. The MCP writer prints its own "already
            # registered for Antigravity" line on this run too, so match the
            # hooks' line exactly and rule out every hooks write path.
            second = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))
            self.assertIn("'ironmem' lifecycle hooks already registered for Antigravity", second.stdout)
            self.assertNotIn("Registering 'ironmem' lifecycle hooks", second.stdout)
            self.assertNotIn("Antigravity hook group", second.stdout)
            self.assertNotIn("Replacing divergent 'ironmem' Antigravity", second.stdout)
            self.assertEqual(json.loads(hooks_path.read_text(encoding="utf-8")), payload)
            self.assert_agy_never_ran(home)

    def test_zero_byte_agy_configs_are_seeded_and_wired(self) -> None:
        # agy creates mcp_config.json as a 0-byte file on first run; jq reads
        # that as zero documents, so it must be seeded like a missing file.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            agy_config.write_bytes(b"")
            hooks_path.write_bytes(b"")

            self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            server = self.read_agy_server(agy_config)
            self.assertEqual(server["command"], str(home / ".ironrace" / "bin" / "ironmem"))
            self.assertEqual(
                json.loads(hooks_path.read_text(encoding="utf-8")),
                {"ironmem": self.ironmem_hook_group(home)},
            )

    def test_unparseable_agy_hooks_file_warns_and_the_install_continues(self) -> None:
        # agy reads HuJSON, so a trailing comma or a comment is legal there and
        # a parse error to jq. That must cost the agy hooks only: no abort, no
        # rewrite, no temp file left beside it, and every later step still runs.
        for name, content in (
            ("trailing comma", '{"lint": {"Stop": []},}\n'),
            ("comment", '// my hooks\n{"lint": {"Stop": []}}\n'),
            ("array", "[]\n"),
            ("two documents", "{} {}\n"),
        ):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                agy_config, hooks_path = self.agy_paths(home)
                hooks_path.parent.mkdir(parents=True)
                hooks_path.write_text(content, encoding="utf-8")

                result = self.run_installer(
                    home, home / ".claude.json", extra_env=self.agy_env(home)
                )

                self.assertEqual(hooks_path.read_text(encoding="utf-8"), content)
                self.assertIn(f"WARN: {hooks_path} is not a single JSON object", result.stderr)
                self.assertIn("skipping Antigravity hook registration", result.stderr)
                self.assertEqual(
                    self.stray_files(hooks_path.parent, "hooks.json", "mcp_config.json"), []
                )
                # The rest of the install still ran.
                self.assertIn("ironmem", json.loads(agy_config.read_text())["mcpServers"])
                self.assertIn(
                    "[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text()
                )
                self.assertIn("==> Done", result.stdout)

    def test_symlinked_agy_configs_keep_the_link_and_update_the_target(self) -> None:
        # Dotfiles are often symlinked in from a repo. The rewrite must land in
        # the file the link names -- through a relative link and a two-hop
        # chain alike -- leave both links in place, keep the target's other
        # entries, and leave no temp file beside the link or the target.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            dotfiles = home / "dotfiles"
            dotfiles.mkdir()
            mcp_target = dotfiles / "agy-mcp.json"
            mcp_target.write_text(
                json.dumps(
                    {
                        "mcpServers": {
                            "github": {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "t0k"}}
                        }
                    }
                ),
                encoding="utf-8",
            )
            hooks_target = dotfiles / "agy-hooks.json"
            hooks_target.write_text(
                json.dumps({"lint": {"Stop": [{"command": "./lint.sh"}]}}), encoding="utf-8"
            )
            mcp_link = os.path.relpath(mcp_target, agy_config.parent)
            agy_config.symlink_to(mcp_link)
            hop = dotfiles / "hooks-hop.json"
            hop.symlink_to(hooks_target)
            hooks_path.symlink_to(hop)

            self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertTrue(agy_config.is_symlink())
            self.assertEqual(os.readlink(agy_config), mcp_link)
            self.assertTrue(hooks_path.is_symlink())
            self.assertEqual(os.readlink(hooks_path), str(hop))
            self.assertEqual(os.readlink(hop), str(hooks_target))
            mcp = json.loads(mcp_target.read_text(encoding="utf-8"))
            self.assertEqual(
                mcp["mcpServers"]["github"], {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "t0k"}}
            )
            self.assertEqual(
                mcp["mcpServers"]["ironmem"]["command"],
                str(home / ".ironrace" / "bin" / "ironmem"),
            )
            hooks = json.loads(hooks_target.read_text(encoding="utf-8"))
            self.assertEqual(hooks["lint"], {"Stop": [{"command": "./lint.sh"}]})
            self.assertEqual(hooks["ironmem"], self.ironmem_hook_group(home))
            self.assertEqual(self.stray_files(agy_config.parent, "hooks.json", "mcp_config.json"), [])
            self.assertEqual(
                self.stray_files(
                    dotfiles, "agy-mcp.json", "agy-hooks.json", "hooks-hop.json"
                ),
                [],
            )

    def test_dangling_symlinked_agy_configs_warn_and_are_left_alone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            missing_mcp = home / "dotfiles" / "gone-mcp.json"
            missing_hooks = home / "dotfiles" / "gone-hooks.json"
            agy_config.symlink_to(missing_mcp)
            hooks_path.symlink_to(missing_hooks)

            result = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            for link, missing in ((agy_config, missing_mcp), (hooks_path, missing_hooks)):
                self.assertTrue(link.is_symlink())
                self.assertEqual(os.readlink(link), str(missing))
                self.assertFalse(missing.exists(), "a dangling link's target must not be created")
                self.assertIn(f"WARN: {link} is a symlink to {missing}", result.stderr)
            self.assertFalse((home / "dotfiles").exists())
            # The rest of the install still ran.
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text())
            self.assertIn("==> Done", result.stdout)

    def test_codex_config_symlink_that_leads_nowhere_skips_codex_wiring(self) -> None:
        # Appending through a dangling link would create the missing target; a
        # loop would fail the append and abort the install. Either way: warn,
        # leave the link alone, skip Codex, and finish everything else.
        for name in ("dangling", "loop"):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                codex_toml = home / ".codex" / "config.toml"
                codex_toml.parent.mkdir(parents=True)
                if name == "dangling":
                    pointee = home / "dotfiles" / "codex.toml"
                    codex_toml.symlink_to(pointee)
                else:
                    pointee = home / ".codex" / "loop-b.toml"
                    pointee.symlink_to(codex_toml)
                    codex_toml.symlink_to(pointee)

                result = self.run_installer(home, home / ".claude.json")

                self.assertTrue(codex_toml.is_symlink())
                self.assertEqual(os.readlink(codex_toml), str(pointee))
                if name == "dangling":
                    self.assertFalse(pointee.exists(), "a dangling link's target must not be created")
                    self.assertFalse((home / "dotfiles").exists())
                self.assertIn(f"WARN: {codex_toml} is a symlink to {pointee}", result.stderr)
                self.assertIn("skipping Codex MCP registration", result.stderr)
                self.assertNotIn("Appending [mcp_servers.ironmem]", result.stdout)
                # Every other harness is still wired and the run completes.
                self.assertIn(
                    "ironmem", json.loads((home / ".claude.json").read_text())["mcpServers"]
                )
                self.assertIn("==> Done", result.stdout)

    def test_symlinked_codex_config_is_appended_through_the_link(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            codex_toml = home / ".codex" / "config.toml"
            codex_toml.parent.mkdir(parents=True)
            real = home / "dotfiles" / "codex.toml"
            real.parent.mkdir()
            real.write_text('model = "o3"\n', encoding="utf-8")
            codex_toml.symlink_to(real)

            self.run_installer(home, home / ".claude.json")

            self.assertTrue(codex_toml.is_symlink())
            self.assertEqual(os.readlink(codex_toml), str(real))
            content = real.read_text(encoding="utf-8")
            self.assertTrue(content.startswith('model = "o3"\n'))
            self.assertIn("[mcp_servers.ironmem]", content)

    def test_legacy_codex_check_reads_the_configured_codex_toml(self) -> None:
        # run_installer's default pin equals $HOME/.codex/config.toml, which is
        # exactly what a hard-coded check reads; only an override pointing
        # elsewhere tells the two apart. A decoy at the default path carries a
        # legacy section the configured file does not, and vice versa.
        legacy = '[mcp_servers.ironrace_memory]\ncommand = "/old/ironrace-memory"\n'
        for skip_wiring in (False, True):
            for legacy_in in ("override", "default"):
                name = f"legacy in {legacy_in}, skip_wiring={skip_wiring}"
                with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                    home = pathlib.Path(directory)
                    override = home / "elsewhere" / "codex.toml"
                    default = home / ".codex" / "config.toml"
                    for path in (override, default):
                        path.parent.mkdir(parents=True, exist_ok=True)
                    (override if legacy_in == "override" else default).write_text(
                        legacy, encoding="utf-8"
                    )
                    (default if legacy_in == "override" else override).write_text(
                        'model = "o3"\n', encoding="utf-8"
                    )

                    result = self.run_installer(
                        home,
                        home / ".claude.json",
                        skip_wiring=skip_wiring,
                        extra_env={"CODEX_CONFIG_TOML": str(override)},
                    )

                    if legacy_in == "override":
                        self.assertIn(
                            f"Codex ({override}) has an [mcp_servers.ironrace_memory] section",
                            result.stdout,
                        )
                    else:
                        self.assertNotIn("Legacy MCP registrations detected", result.stdout)
                        self.assertNotIn(str(default), result.stdout)

    def test_legacy_claude_check_reads_the_configured_claude_json(self) -> None:
        # The Claude twin of the Codex test above: an override pointing away
        # from $HOME/.claude.json, with a legacy server in exactly one of the
        # two files. Only the configured file may be reported.
        legacy = {"mcpServers": {"ironrace-memory": {"command": "/old/ironrace-memory"}}}
        clean = {"theme": "dark"}
        for skip_wiring in (False, True):
            for legacy_in in ("override", "default"):
                name = f"legacy in {legacy_in}, skip_wiring={skip_wiring}"
                with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                    home = pathlib.Path(directory)
                    override = home / "elsewhere" / "claude.json"
                    default = home / ".claude.json"
                    override.parent.mkdir(parents=True)
                    override.write_text(
                        json.dumps(legacy if legacy_in == "override" else clean), encoding="utf-8"
                    )
                    default.write_text(
                        json.dumps(legacy if legacy_in == "default" else clean), encoding="utf-8"
                    )

                    result = self.run_installer(home, override, skip_wiring=skip_wiring)

                    if legacy_in == "override":
                        self.assertIn(
                            f"Claude Code ({override}) has an 'ironrace-memory' server",
                            result.stdout,
                        )
                    else:
                        self.assertNotIn("Legacy MCP registrations detected", result.stdout)
                        self.assertNotIn(str(default), result.stdout)
                    # The check only reports; the legacy entry is never removed.
                    holder = override if legacy_in == "override" else default
                    self.assertIn(
                        "ironrace-memory", json.loads(holder.read_text())["mcpServers"]
                    )

    def test_rewrite_keeps_an_existing_config_mode(self) -> None:
        # mktemp creates its file 0600; renaming it over a deliberately shared
        # 0644 config must not narrow it, and a 0600 one must never widen.
        for mode in (0o644, 0o600, 0o640):
            with self.subTest(oct(mode)), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                agy_config, hooks_path = self.agy_paths(home)
                agy_config.parent.mkdir(parents=True)
                claude = home / ".claude.json"
                for path, content in (
                    (agy_config, {"mcpServers": {"other": {"command": "x"}}}),
                    (hooks_path, {"lint": {"Stop": []}}),
                    (claude, {"theme": "dark"}),
                ):
                    path.write_text(json.dumps(content), encoding="utf-8")
                    path.chmod(mode)

                self.run_installer(home, claude, extra_env=self.agy_env(home))

                for path in (agy_config, hooks_path, claude):
                    self.assertIn("ironmem", path.read_text(encoding="utf-8"), path)
                    self.assertEqual(stat.S_IMODE(path.stat().st_mode), mode, path)

    def test_blank_existing_config_keeps_its_mode_when_seeded(self) -> None:
        # agy creates mcp_config.json 0-byte and 0644 on first run; seeding it
        # is a rewrite of an existing file, not a new one.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, _ = self.agy_paths(home)
            agy_config.parent.mkdir(parents=True)
            agy_config.write_bytes(b"")
            agy_config.chmod(0o644)

            self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertIn("ironmem", self.read_agy_server(agy_config)["command"])
            self.assertEqual(stat.S_IMODE(agy_config.stat().st_mode), 0o644)

    def test_newly_created_configs_are_owner_only(self) -> None:
        # MCP configs collect other servers' env tokens, so a file this script
        # creates starts 0600 whatever the umask -- as the Rust writer does.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            agy_config, hooks_path = self.agy_paths(home)
            claude = home / ".claude.json"
            muse = home / ".config" / "muse" / "settings.json"
            codex = home / ".codex" / "config.toml"

            previous = os.umask(0o022)
            try:
                self.run_installer(home, claude, extra_env=self.agy_env(home))
            finally:
                os.umask(previous)

            for path in (agy_config, hooks_path, claude, muse, codex):
                self.assertTrue(path.is_file(), path)
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600, path)

    def test_partial_ironmem_hook_group_is_repaired_in_place(self) -> None:
        # Only SessionStart is ours (an older install, or PreInvocation/Stop
        # deleted by hand). The gaps are filled; the user's extra SessionStart
        # handler and `enabled` key are kept, and our SessionStart handler
        # gains the timeout this version sets.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            _, hooks_path = self.agy_paths(home)
            hooks_path.parent.mkdir(parents=True)
            start = {
                "type": "command",
                "command": self.expected_hook_command(home, "trusted", "session-start"),
            }
            extra = {"type": "command", "command": "./my-session-note.sh", "timeout": 5}
            hooks_path.write_text(
                json.dumps({"ironmem": {"enabled": True, "SessionStart": [start, extra]}}),
                encoding="utf-8",
            )

            result = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertIn("Adding the missing", result.stdout)
            self.assertIn("PreInvocation hook", result.stdout)
            self.assertIn("Stop hook", result.stdout)
            self.assertIn("SessionStart timeout", result.stdout)
            want = self.ironmem_hook_group(home)
            group = json.loads(hooks_path.read_text(encoding="utf-8"))["ironmem"]
            self.assertEqual(group["enabled"], True)
            self.assertEqual(group["SessionStart"], [{**start, "timeout": 120}, extra])
            self.assertEqual(group["PreInvocation"], want["PreInvocation"])
            self.assertEqual(group["Stop"], want["Stop"])

    def test_divergent_ironmem_hook_group_is_kept_without_force_wiring(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            _, hooks_path = self.agy_paths(home)
            hooks_path.parent.mkdir(parents=True)
            group = self.ironmem_hook_group(home)
            group["Stop"] = [{"type": "command", "command": "/opt/fork/ironmem hook stop"}]
            group["enabled"] = False
            group["PostToolUse"] = [{"matcher": "*", "hooks": [{"command": "./audit.sh"}]}]
            original = json.dumps({"ironmem": group, "lint": {"Stop": []}})
            hooks_path.write_text(original, encoding="utf-8")

            kept = self.run_installer(home, home / ".claude.json", extra_env=self.agy_env(home))

            self.assertEqual(hooks_path.read_text(encoding="utf-8"), original)
            self.assertIn("Stop command=/opt/fork/ironmem hook stop", kept.stderr)
            self.assertIn("--force-wiring", kept.stderr)

            forced = self.run_installer(
                home, home / ".claude.json", force_wiring=True, extra_env=self.agy_env(home)
            )

            self.assertIn("Replacing divergent 'ironmem' Antigravity hooks entry", forced.stdout)
            payload = json.loads(hooks_path.read_text(encoding="utf-8"))
            self.assertEqual(payload["lint"], {"Stop": []})
            # --force-wiring replaces our three events; the user's disable
            # switch and their other events stay.
            self.assertEqual(
                payload["ironmem"],
                {
                    **self.ironmem_hook_group(home),
                    "enabled": False,
                    "PostToolUse": group["PostToolUse"],
                },
            )
            self.assertIn("left disabled", forced.stdout)

    def test_user_owned_ironmem_hook_groups_are_not_clobbered(self) -> None:
        cases = {
            # Ours and complete, but switched off with agy's documented
            # `enabled: false`: nothing to write, and it stays off.
            "our group, disabled": lambda home: {
                "ironmem": {"enabled": False, **self.ironmem_hook_group(home)}
            },
            # Someone else's group that happens to be named `ironmem`, holding
            # none of our events: not ours to fill in.
            "foreign group": lambda home: {
                "ironmem": {
                    "enabled": False,
                    "PreToolUse": [{"matcher": "run_command", "hooks": [{"command": "./x.sh"}]}],
                }
            },
        }
        for name, payload_for in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                _, hooks_path = self.agy_paths(home)
                hooks_path.parent.mkdir(parents=True)
                original = json.dumps(payload_for(home))
                hooks_path.write_text(original, encoding="utf-8")

                result = self.run_installer(
                    home, home / ".claude.json", extra_env=self.agy_env(home)
                )

                self.assertEqual(hooks_path.read_text(encoding="utf-8"), original)
                if name == "our group, disabled":
                    self.assertIn(
                        "'ironmem' lifecycle hooks already registered for Antigravity",
                        result.stdout,
                    )
                    self.assertIn("left disabled", result.stdout)
                else:
                    self.assertIn("none of its lifecycle events", result.stderr)
                    self.assertIn("PreToolUse", result.stderr)

    def test_missing_jq_prints_the_agy_hooks_snippet(self) -> None:
        # A PATH with every tool the installer needs except jq: a farm of
        # symlinks to the real PATH's executables, minus jq and agy.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            farm = home / "path-without-jq"
            farm.mkdir()
            for entry in os.environ["PATH"].split(os.pathsep):
                if not entry or not os.path.isdir(entry):
                    continue
                for tool in os.listdir(entry):
                    link = farm / tool
                    source = pathlib.Path(entry) / tool
                    if tool in ("jq", "agy") or os.path.lexists(link):
                        continue
                    try:
                        runnable = source.is_file() and os.access(source, os.X_OK)
                    except OSError:  # e.g. a root-only /usr/sbin entry
                        continue
                    if runnable:
                        link.symlink_to(source)
            self.assertIsNone(shutil.which("jq", path=str(farm)))
            _, hooks_path = self.agy_paths(home)

            without_agy = self.run_installer(
                home, home / ".claude.json", extra_env={"PATH": str(farm)}
            )
            self.assertIn("jq not installed; skipping Claude and Muse", without_agy.stderr)
            self.assertNotIn("hooks.json", without_agy.stderr)

            with_agy = self.run_installer(
                home,
                home / ".claude.json",
                extra_env={**self.agy_env(home), "PATH": str(home / "agy-stub-bin") + os.pathsep + str(farm)},
            )

            self.assertIn("Claude, Muse, and Antigravity", with_agy.stderr)
            self.assertIn(f"hook group to {hooks_path}", with_agy.stderr)
            snippet = next(
                line.strip() for line in with_agy.stderr.splitlines() if '"PreInvocation"' in line
            )
            self.assertEqual(json.loads(snippet), {"ironmem": self.ironmem_hook_group(home)})
            self.assertFalse(hooks_path.exists())
            self.assert_agy_never_ran(home)

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
            # Nothing in the path was run as code, by the installer or the hooks.
            self.assertEqual(list(home.rglob("PWNED")), [])
            self.assertFalse((ROOT / "PWNED").exists())
            self.assertEqual(self.claude_settings(home), settings, "a re-run adds nothing")
            self.assertNotIn("Wired Claude hooks", second.stdout)
            # Its own commands name this install's program: no other-program WARN.
            self.assertNotIn("is already wired, so this install left it", second.stderr)
            self.assertIn(
                "Claude hooks already wired, left as they are: SessionStart UserPromptSubmit Stop PreCompact",
                second.stdout,
            )

    def test_hooks_run_from_an_install_dir_with_a_space(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("Jo Smith")

    def test_hooks_run_from_an_install_dir_with_a_single_quote(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("it's here")

    def test_hooks_run_from_an_install_dir_with_a_name_quote(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("O'Brien")

    def test_hooks_run_from_an_install_dir_with_regex_characters(self) -> None:
        self.assert_hooks_run_from_an_unsafe_install_dir("a+b(c)")

    def test_hooks_run_from_an_install_dir_with_shell_characters(self) -> None:
        for dir_name in ("a$(touch PWNED)b", "a`touch PWNED`b", "a\\b", 'a"b', "a;b", "a*b", "a!b", "a{x,y}b", "a&b"):
            with self.subTest(dir_name=dir_name):
                self.assert_hooks_run_from_an_unsafe_install_dir(dir_name)

    def test_a_relative_install_dir_is_made_absolute(self) -> None:
        # A hook or MCP client starts in a directory of its own choosing, so a
        # relative program path would not find the installed binary.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            work = home / "work"
            work.mkdir()

            self.run_installer(home, home / ".claude.json", extra_env={"IRONMEM_INSTALL_DIR": "relbin"}, cwd=work)

            # The shell's $PWD spells the directory physically (/private/var on macOS).
            target = work.resolve() / "relbin" / "ironmem"
            self.assertTrue(target.is_file())
            self.assertEqual(self.read_claude_server(home / ".claude.json")["command"], str(target))
            self.assertEqual(
                self.ironmem_hook_commands(self.claude_settings(home), "Stop"),
                [f"{target} hook stop --harness claude-code"],
            )

    def test_the_binary_is_renamed_into_place_from_a_temp_name(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            install_dir = home / ".ironrace" / "bin"
            install_dir.mkdir(parents=True)
            target = install_dir / "ironmem"
            target.write_text("old", encoding="utf-8")
            old_inode = target.stat().st_ino

            self.run_installer(home, home / ".claude.json", skip_wiring=True)

            self.assertEqual(sorted(p.name for p in install_dir.iterdir()), ["ironmem"])
            self.assertNotEqual(target.stat().st_ino, old_inode, "a new inode, not the old file rewritten")
            self.assertEqual(target.read_bytes(), RELEASE_BINARY.read_bytes())
            self.assertTrue(os.access(target, os.X_OK))

    def test_a_failed_rename_of_the_binary_fails_and_leaves_no_temp_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            install_dir = home / ".ironrace" / "bin"
            env = self.command_stub(home, "mv", "ironmem.new.", 'echo "stub mv failure" >&2; exit 1')

            result = self.run_installer(home, home / ".claude.json", extra_env=env, expected_returncode=1)

            self.assertIn(f"ERROR: could not install {RELEASE_BINARY} as {install_dir / 'ironmem'}", result.stderr)
            self.assertEqual(list(install_dir.iterdir()), [])

    def test_without_jq_the_printed_hook_command_is_quoted(self) -> None:
        # A space needs shell quoting; a `"`, `\`, newline, tab or other
        # control character also needs JSON escaping.
        for dir_name in ("Jo Smith", 'Jo "JJ" \\ Smith', "Jo\nnew\tline", "Jo\x01\x1bctl\x7f é"):
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

    def test_skip_hooks_registers_mcp_but_wires_no_hooks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            result = self.run_installer(home, home / ".claude.json", skip_hooks=True)

            self.assertFalse((home / ".claude" / "settings.json").exists())
            self.assertEqual(self.read_claude_server(home / ".claude.json")["command"], str(home / ".ironrace" / "bin" / "ironmem"))
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))
            self.assertIn("==> Skipping Claude Code hook wiring (--skip-hooks)", result.stdout)
            self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_skip_hooks_leaves_a_settings_file_with_hooks_removed_as_it_is(self) -> None:
        # Without --skip-hooks a re-run adds back a deleted hook; with it, not.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_hooks=True)

            self.assertEqual(path.read_text(encoding="utf-8"), before)

            self.run_installer(home, home / ".claude.json")

            self.assertEqual(len(self.ironmem_hook_commands(self.claude_settings(home), "Stop")), 1)

    def test_skip_hooks_without_jq_prints_no_hook_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.path_without_jq(home / "no-jq-bin")

            result = self.run_installer(home, home / ".claude.json", skip_hooks=True, extra_env={"PATH": path})

            self.assertIn("jq not installed", result.stderr)
            self.assertNotIn("Claude Code hooks were not wired", result.stderr)
            self.assertNotIn('"<Event>"', result.stderr)

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

    def test_shared_stop_fixtures_are_matched_as_doctor_reads_them(self) -> None:
        rows = json.loads(HOOK_STOP_FIXTURES.read_text(encoding="utf-8"))["rows"]
        self.assertGreater(len(rows), 30)
        names = self.plugin_hook_names()
        for row in rows:
            entry = {key: value for key, value in row.items() if key not in ("wired", "note")}
            with self.subTest(entry=entry, note=row["note"]), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                group = {"hooks": [{"type": "command", **entry}]}
                self.settings_with_stop(home, [group])

                result = self.run_installer(home, home / ".claude.json")

                settings = self.claude_settings(home)
                stop = settings["hooks"]["Stop"]
                self.assertEqual(stop[0], group, "the existing entry is left as it is")
                # The file was written: the other three events were appended.
                target = home / ".ironrace" / "bin" / "ironmem"
                for event in ("SessionStart", "UserPromptSubmit", "PreCompact"):
                    self.assertEqual(
                        self.ironmem_hook_commands(settings, event),
                        [f"{target} hook {names[event]} --harness claude-code"],
                        event,
                    )
                if row["wired"]:
                    self.assertEqual(len(stop), 1, f"an existing copy is already wired: {stop}")
                    self.assertNotIn("wired no Claude hooks", result.stderr)
                    self.assertIn("Claude hooks already wired, left as they are: Stop\n", result.stdout)
                else:
                    self.assertEqual(len(stop), 2, stop)
                    self.assertIn("hook stop --harness claude-code", json.dumps(stop[1]))
                    self.assertNotIn("already wired", result.stdout)

    @staticmethod
    def hook_jq_defs() -> str:
        text = INSTALLER.read_text(encoding="utf-8")
        match = re.search(r"^_CLAUDE_HOOK_JQ_DEFS='([^']*)'$", text, re.M)
        assert match, "no _CLAUDE_HOOK_JQ_DEFS in the installer"
        return match.group(1)

    def test_shared_fixtures_are_matched_for_every_event(self) -> None:
        # The fixture rows are Stop entries; with `stop` swapped for each hook's
        # name they must give the same answer for all four, hyphenated names
        # included. Runs the installer's own runs_hook, without an install.
        rows = json.loads(HOOK_STOP_FIXTURES.read_text(encoding="utf-8"))["rows"]
        for event, name in self.plugin_hook_names().items():
            entries = [
                json.loads(json.dumps({"type": "command", **{k: v for k, v in row.items() if k not in ("wired", "note")}}).replace("stop", name))
                for row in rows
            ]
            ran = subprocess.run(
                ["jq", "-c", "--arg", "name", name, self.hook_jq_defs() + "[.[] | runs_hook($name)]"],
                input=json.dumps(entries),
                capture_output=True,
                text=True,
            )
            self.assertEqual(ran.returncode, 0, ran.stderr)
            for row, entry, wired in zip(rows, entries, json.loads(ran.stdout)):
                with self.subTest(event=event, entry=entry, note=row["note"]):
                    self.assertEqual(wired, row["wired"])

    def test_a_trusted_prefix_counts_as_wired(self) -> None:
        # The prefix doctor's hint tells users to add to a read-only hook.
        command = "IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem hook stop --harness claude-code"
        self.assertEqual(self.stop_commands_after_install(command), [command])

    def assert_settings_refused(self, contents: str, warning: str, detail: str | None = None) -> None:
        """Install over a settings.json holding `contents`; the file must be left
        byte for byte, with `warning` (the text before "; wired no Claude hooks.")
        on stderr and, when given, a "jq said:" line starting with `detail`."""
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(contents, encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(path.read_text(encoding="utf-8"), contents)
            self.assertIn(f"WARN: {warning.format(path=path)}; wired no Claude hooks.", result.stderr)
            said = [line.strip() for line in result.stderr.splitlines() if "jq said:" in line]
            if detail is None:
                self.assertEqual(said, [], result.stderr)
            else:
                self.assertEqual(len(said), 1, result.stderr)
                self.assertTrue(said[0].startswith(f"jq said: {detail}"), said[0])

    def test_invalid_json_is_reported_as_unparseable(self) -> None:
        self.assert_settings_refused(
            '{"model": "opus",}', "jq could not read {path} as JSON", "jq: parse error:"
        )

    def test_an_empty_settings_file_is_reported_as_empty(self) -> None:
        for contents in ("", "  \n"):
            with self.subTest(contents=contents):
                self.assert_settings_refused(
                    contents, "{path} holds no JSON value (it is empty or only whitespace)"
                )

    def test_several_json_values_are_refused(self) -> None:
        # jq without -s would read each value and write each back, merged.
        self.assert_settings_refused('{"a": 1}\n{"b": 2}\n', "{path} holds more than one JSON value")

    def test_a_top_level_array_is_reported_as_not_an_object(self) -> None:
        self.assert_settings_refused("[1]", "{path} is not a JSON object with array-valued hooks")

    def test_hooks_false_is_refused_as_a_shape_problem(self) -> None:
        # doctor reports `hooks is not an object` and `hooks.Stop is not an
        # array` for these files; the installer agrees instead of failing in its
        # matcher or replacing the false.
        for contents in ('{"hooks": false}', '{"hooks": {"Stop": false}}'):
            with self.subTest(contents=contents):
                self.assert_settings_refused(
                    contents, "{path} is not a JSON object with array-valued hooks"
                )

    def installer_jq_programs(self) -> list[str]:
        """The hook jq definitions and every jq program the installer passes
        inline, read from its code with comment lines dropped."""
        code = "\n".join(
            line for line in INSTALLER.read_text(encoding="utf-8").splitlines() if not line.lstrip().startswith("#")
        )
        inline = re.findall(r"(?:^|\$\(|[|;&!]|\bif|\bthen)\s*jq\s(?:[^'\n]|\\\n)*'([^']*)'", code, re.M)
        self.assertGreater(len(inline), 10, inline)
        self.assertTrue(any("disableAllHooks" in program for program in inline), inline)
        return [self.hook_jq_defs(), *inline]

    def test_the_hook_jq_uses_no_builtin_newer_than_jq_1_5(self) -> None:
        # jq 1.5, still packaged on some distributions, lacks these (jq's
        # NEWS.md): IN and INDEX (the SQL-style builtins), walk, isempty,
        # halt_error, utf8bytelength, $ENV and ?// came in 1.6; pick, abs and
        # `if` without `else` in 1.7; trim, ltrim, rtrim, trimstr, toboolean
        # and skip in 1.8. One of them in the shared defs makes every hook jq
        # call fail to compile, so no hook would be wired.
        newer = re.compile(
            r"\b(?:IN|INDEX|walk|isempty|pick|trim|ltrim|rtrim|trimstr|toboolean|skip|halt_error|utf8bytelength)\("
            r"|\babs\b|\$ENV\b|\?//"
        )
        for program in self.installer_jq_programs():
            with self.subTest(program=program[:60]):
                self.assertIsNone(newer.search(program))
                # Every `if` has an `else` (jq 1.5 has no `if` without one).
                # String literals are dropped first, so a regex holding "end"
                # cannot count.
                words = re.findall(r"\b(?:if|else|end)\b", re.sub(r'"(?:[^"\\]|\\.)*"', '""', program))
                open_ifs: list[bool] = []
                for word in words:
                    if word == "if":
                        open_ifs.append(False)
                    elif word == "else":
                        self.assertTrue(open_ifs, "else without if")
                        open_ifs[-1] = True
                    else:
                        self.assertTrue(open_ifs and open_ifs.pop(), "an if without an else")
                self.assertEqual(open_ifs, [])

    def test_a_refused_file_doctor_does_not_read_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / "elsewhere" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text("[1]", encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", extra_env={"CLAUDE_SETTINGS_JSON": str(path)}
            )

            self.assertIn(
                "Fix that, then re-run this script. `ironmem doctor` reads only "
                f"{home / '.claude' / 'settings.json'}, not this file.",
                result.stderr,
            )
            self.assertNotIn("shows what is missing", result.stderr)

    def other_program_warning(self, home: pathlib.Path, event: str, program: str, line: str) -> str:
        # Doctor warns only when no other run of the event starts, and the WARN
        # names only the program path, never the whole entry.
        path = home / ".claude" / "settings.json"
        target = home / ".ironrace" / "bin" / "ironmem"
        return (
            f"    WARN: {event} in {path} is already wired, so this install left it as it is, but "
            f"the program it names by path is not {target}, the program this install put in place. It names:\n"
            f"            {program}\n"
            f"          If no other command for {event} starts and that program is not an executable file, "
            f"`ironmem doctor` warns on its {line} line.\n"
        )

    def test_an_event_wired_to_another_program_is_named_in_a_warning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            stop = "/opt/elsewhere/ironmem hook stop --harness claude-code"
            prompt = "'/opt/old dir/ironmem' hook user-prompt-submit"
            session = {"type": "command", "command": "env", "args": ["IRONMEM_MCP_MODE=trusted", "/old/ironmem", "hook", "session-start"]}
            precompact = {"type": "command", "command": "bash", "args": ["-c", '"/old/ironmem" hook precompact']}
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            original = {
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": stop}]}],
                    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": prompt}]}],
                    "SessionStart": [{"hooks": [session]}],
                    "PreCompact": [{"hooks": [precompact]}],
                }
            }
            path.write_text(json.dumps(original), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(self.claude_settings(home), original)
            self.assertIn(self.other_program_warning(home, "Stop", "/opt/elsewhere/ironmem", "hook_mining"), result.stderr)
            self.assertIn(self.other_program_warning(home, "UserPromptSubmit", "/opt/old dir/ironmem", "hooks"), result.stderr)
            self.assertIn(self.other_program_warning(home, "SessionStart", "/old/ironmem", "hooks"), result.stderr)
            self.assertIn(self.other_program_warning(home, "PreCompact", "/old/ironmem", "hook_mining"), result.stderr)
            self.assertIn(
                "Claude hooks already wired, left as they are: SessionStart UserPromptSubmit Stop PreCompact",
                result.stdout,
            )

    def test_an_event_wired_to_this_install_gets_no_program_warning(self) -> None:
        # <target> is this install's program and <link> a symlink to it. A PATH
        # lookup, a path the shell builds and a wrapper script run as `bash
        # <script>` are not judged, as doctor's missing-program check does not
        # judge them.
        shell_commands = (
            "<target> hook stop",
            '"<target>" hook stop',
            "'<target>' hook stop",
            "IRONMEM_MCP_MODE=trusted <target> hook stop",
            "IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem hook stop",
            "$HOME/.ironrace/bin/ironmem hook stop",
            "${HOME}/.ironrace/bin/ironmem hook stop",
            '"$HOME/.ironrace/bin/ironmem" hook stop',
            "ironmem hook stop",
            "<link> hook stop",
            "bash /x/.claude-plugin/hooks/ironmem-hook.sh stop",
        )
        exec_entries = (
            ["bash", "-c", "<target> hook stop"],
            ["bash", "-c", '"<target>" hook stop'],
            ["env", "IRONMEM_MCP_MODE=trusted", "<target>", "hook", "stop"],
            ["<link>", "hook", "stop"],
        )
        cases: list[dict[str, object]] = []
        for harness in ("", " --harness claude-code"):
            cases += [{"command": command + harness} for command in shell_commands]
            for argv in exec_entries:
                if argv[0] == "bash":
                    cases.append({"command": "bash", "args": [argv[1], argv[2] + harness]})
                else:
                    cases.append({"command": argv[0], "args": argv[1:] + harness.split()})
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                target = str(home / ".ironrace" / "bin" / "ironmem")
                link = home / "elsewhere" / "ironmem"
                link.parent.mkdir()
                link.symlink_to(target)
                entry = json.loads(
                    json.dumps({"type": "command", **case}).replace("<target>", target).replace("<link>", str(link))
                )
                self.settings_with_stop(home, [{"hooks": [entry]}])
                exec_home = {"type": "command", "command": target, "args": ["hook", "precompact"]}
                settings = self.claude_settings(home)
                settings["hooks"]["PreCompact"] = [{"hooks": [exec_home]}]
                (home / ".claude" / "settings.json").write_text(json.dumps(settings), encoding="utf-8")

                result = self.run_installer(home, home / ".claude.json")

                self.assertNotIn("is already wired, so this install left it", result.stderr)
                self.assertNotIn("WARN", result.stderr)
                self.assertIn("already wired, left as they are: Stop PreCompact", result.stdout)

    def test_every_run_of_a_double_quoted_c_string_is_read_for_its_program(self) -> None:
        # The first run too: when it names this install's program there is no
        # WARN about the run after it, and when it names another one the WARN
        # lists both.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            target = home / ".ironrace" / "bin" / "ironmem"
            stop = f'bash -c "{target} hook stop; /old/ironmem hook stop"'
            self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": stop}]}])

            result = self.run_installer(home, home / ".claude.json")

            self.assertNotIn("the program it names by path is not", result.stderr)
            self.assertIn("Claude hooks already wired, left as they are: Stop", result.stdout)
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            stop = 'bash -c "/older/ironmem hook stop; /old/ironmem hook stop"'
            self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": stop}]}])

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn("the program it names by path is not", result.stderr)
            for program in ("/older/ironmem", "/old/ironmem"):
                self.assertIn(f"            {program}\n", result.stderr)

    def test_a_separator_in_single_quotes_does_not_hide_a_later_program(self) -> None:
        # The `;` and `|` in single quotes start no command. The run after
        # them is still read for its program: another install's gets the
        # WARN, and this install's keeps the WARN away.
        for quoted in ("echo 'a;b'", "jq -r '.a|.b' /dev/null", "cd '/tmp/a (b)'"):
            with self.subTest(quoted=quoted, program="another"), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                stop = f'bash -c "{quoted}; /old/ironmem hook stop"'
                self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": stop}]}])

                result = self.run_installer(home, home / ".claude.json")

                self.assertIn("the program it names by path is not", result.stderr)
                self.assertIn("            /old/ironmem\n", result.stderr)
            with self.subTest(quoted=quoted, program="this install"), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                target = home / ".ironrace" / "bin" / "ironmem"
                groups = [
                    {"hooks": [{"type": "command", "command": f'bash -c "{quoted}; {target} hook stop"'}]},
                    {"hooks": [{"type": "command", "command": "/old/ironmem hook stop"}]},
                ]
                self.settings_with_stop(home, groups)

                result = self.run_installer(home, home / ".claude.json")

                self.assertNotIn("the program it names by path is not", result.stderr)

    def test_an_exec_form_tilde_program_gets_a_warning_even_for_this_install(self) -> None:
        # Doctor reads an exec-form ~/ program as not expanded, so the hook
        # does not run, even when ~/ would expand to this install's program.
        for command, args in (
            ("~/.ironrace/bin/ironmem", ["hook", "stop"]),
            ("env", ["IRONMEM_MCP_MODE=trusted", "~/.ironrace/bin/ironmem", "hook", "stop"]),
        ):
            with self.subTest(command=command), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                entry = {"type": "command", "command": command, "args": args}
                path = self.settings_with_stop(home, [{"hooks": [entry]}])

                result = self.run_installer(home, home / ".claude.json")

                shown = "~/.ironrace/bin/ironmem"
                self.assertIn(
                    f"    WARN: Stop in {path} is already wired, so this install left it as it is, but "
                    "its exec-form entry (args set) starts its program with ~/, which doctor reads as "
                    "not expanded, as no shell reads an exec-form command. It names:\n"
                    f"            {shown}\n"
                    "          Write the program's full path in that entry, or remove the entry and "
                    "re-run this script. If no other command for Stop starts, `ironmem doctor` "
                    "reports it as not running on its hook_mining line.\n",
                    result.stderr,
                )
                self.assertEqual(self.claude_settings(home)["hooks"]["Stop"], [{"hooks": [entry]}])
                self.assertIn("Claude hooks already wired, left as they are: Stop", result.stdout)

    def test_a_literal_path_to_another_file_gets_a_program_warning(self) -> None:
        # Another executable file under a path that looks like this install's.
        for quote in ("", '"', "'"):
            with self.subTest(quote=quote), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                other = home / "other" / ".ironrace" / "bin" / "ironmem"
                other.parent.mkdir(parents=True)
                other.write_text("#!/bin/sh\n", encoding="utf-8")
                other.chmod(0o755)
                stop = f"{quote}{other}{quote} hook stop"
                self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": stop}]}])

                result = self.run_installer(home, home / ".claude.json")

                self.assertIn(self.other_program_warning(home, "Stop", str(other), "hook_mining"), result.stderr)

    def test_a_program_warning_names_only_the_program_path(self) -> None:
        # A token in a NAME=value word or an argument never reaches the output,
        # and a bare `ironmem` run beside the other one still leaves the WARN
        # (doctor then reports the hook as starting, so the WARN is hedged).
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            stop = [
                {"hooks": [{"type": "command", "command": "API_TOKEN=sekrit /old/bin/ironmem hook stop --token sekrit"}]},
                {"hooks": [{"type": "command", "command": "ironmem hook stop"}]},
                {"hooks": [{"type": "command", "command": "env", "args": ["API_TOKEN=sekrit", "/old/bin/ironmem", "hook", "stop"]}]},
            ]
            self.settings_with_stop(home, stop)

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn(self.other_program_warning(home, "Stop", "/old/bin/ironmem", "hook_mining"), result.stderr)
            self.assertEqual(result.stderr.count("/old/bin/ironmem"), 1, "one line per distinct path")
            self.assertNotIn("sekrit", result.stderr + result.stdout)

    def test_a_program_path_with_control_characters_is_shown_with_question_marks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            # No `[`: a glob character would make it a path the shell builds.
            program = "/old/a\x1bb\rc\x07d\x7fe/ironmem"
            self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": "env", "args": [program, "hook", "stop"]}]}])

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn(self.other_program_warning(home, "Stop", "/old/a?b?c?d?e/ironmem", "hook_mining"), result.stderr)
            for byte in ("\x1b", "\r", "\x07", "\x7f"):
                self.assertNotIn(byte, result.stderr)

    def test_a_wrapper_script_named_as_the_program_gets_a_program_warning(self) -> None:
        # Doctor checks a wrapper named directly as the program (not one run as
        # `bash <script>`), so the installer names it too.
        cases = (
            ("/old/hooks/ironmem-hook.sh stop", "/old/hooks/ironmem-hook.sh"),
            ("/old/bin/ironmem-mcp.sh hook stop", "/old/bin/ironmem-mcp.sh"),
            ({"command": "/old/hooks/ironmem-hook.sh", "args": ["stop"]}, "/old/hooks/ironmem-hook.sh"),
            ({"command": "/old/bin/ironmem-mcp.sh", "args": ["hook", "stop"]}, "/old/bin/ironmem-mcp.sh"),
        )
        for entry, program in cases:
            with self.subTest(entry=entry), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                hook = {"type": "command", **(entry if isinstance(entry, dict) else {"command": entry})}
                self.settings_with_stop(home, [{"hooks": [hook]}])

                result = self.run_installer(home, home / ".claude.json")

                self.assertIn(self.other_program_warning(home, "Stop", program, "hook_mining"), result.stderr)
                self.assertIn("Claude hooks already wired, left as they are: Stop", result.stdout)

    def test_a_program_warning_for_a_file_doctor_does_not_read_says_so(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / "elsewhere" / "settings.json"
            path.parent.mkdir(parents=True)
            stop = "/opt/elsewhere/ironmem hook stop"
            path.write_text(json.dumps({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": stop}]}]}}), encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", extra_env={"CLAUDE_SETTINGS_JSON": str(path)}
            )

            self.assertIn(f"    WARN: Stop in {path} is already wired", result.stderr)
            self.assertIn(
                f"          `ironmem doctor` reads only {home / '.claude' / 'settings.json'}, not this file.\n",
                result.stderr,
            )
            self.assertNotIn("warns on its", result.stderr)
            self.assertIn("\n    PreCompact run read-only and skip mining (prompt recall still works) unless the environment Claude Code is launched with sets IRONMEM_MCP_MODE; trusted there makes them mine.\n", result.stdout)
            self.assertNotIn("see `ironmem doctor`", result.stdout)

    def test_a_trailing_slash_in_claude_home_is_still_the_file_doctor_reads(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = {"CLAUDE_HOME": str(home / ".claude") + "/"}

            fresh = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn(
                "\n    Stop PreCompact run read-only and skip mining (prompt recall still works) unless the environment Claude Code is launched with sets IRONMEM_MCP_MODE; trusted there makes them mine (see `ironmem doctor`).\n",
                fresh.stdout,
            )
            path = home / ".claude" / "settings.json"
            path.write_text("[1]", encoding="utf-8")

            refused = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn("Fix that, then re-run this script. `ironmem doctor` shows what is missing.", refused.stderr)
            self.assertNotIn("not this file", refused.stderr)

    @unittest.skipIf(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        "precondition: root writes into a mode 0555 directory",
    )
    def test_a_trailing_slash_in_claude_home_names_doctor_when_the_file_cannot_be_made(self) -> None:
        # `-ef` is false for a file that does not exist, so the directories are
        # compared instead.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            claude = home / ".claude"
            claude.mkdir()
            claude.chmod(0o555)
            try:
                result = self.run_installer(home, home / ".claude.json", extra_env={"CLAUDE_HOME": f"{claude}/"})
            finally:
                claude.chmod(0o755)

            self.assertIn(f"WARN: cannot create {claude}//settings.json; wired no Claude hooks.", result.stderr)
            self.assertIn("`ironmem doctor` shows what is missing.", result.stderr)
            self.assertNotIn("not this file", result.stderr)

    def test_a_jq_error_in_the_mining_line_says_it_cannot_tell(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            env = self.jq_stub(home, "IRONMEM_MCP_MODE | strings")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn("Wired Claude hooks", result.stdout)
            self.assertIn(
                f"    WARN: jq failed reading env.IRONMEM_MCP_MODE in {path}, so this script cannot say whether "
                "Stop PreCompact mine (see `ironmem doctor`).\n          jq said: stub jq failure\n",
                result.stderr,
            )

    def test_jq_stderr_on_success_is_not_read_as_a_program_path(self) -> None:
        # A jq that writes a notice to stderr and still succeeds. A PATH lookup
        # gives no record of its own, so a merged notice would be the only one.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            self.settings_with_stop(home, [{"hooks": [{"type": "command", "command": "ironmem hook stop"}]}])
            env = self.command_stub(home, "jq", "--arg home ", 'echo "/opt/noise/ironmem" >&2')

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertNotIn("WARN", result.stderr)
            self.assertIn("already wired, left as they are: Stop", result.stdout)

    def mining_line_after_install(self, settings: dict[str, object]) -> str:
        """The line the installer prints about Stop and PreCompact mining after
        wiring all four hooks into a settings.json holding `settings`."""
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps(settings), encoding="utf-8")
            result = self.run_installer(home, home / ".claude.json")
            lines = [line for line in result.stdout.splitlines() if line.startswith("    Stop PreCompact ")]
            self.assertEqual(len(lines), 1, result.stdout)
            return lines[0].replace(str(path), "<path>")

    def test_shared_env_rows_say_whether_the_added_miners_mine(self) -> None:
        # The shared fixture's env_rows, which doctor's unit tests read too.
        # settings.json's env beats the launch environment, so a string mode
        # there is known; with none, the launch environment decides, which
        # the installer cannot see.
        lines = {
            "writes": "    Stop PreCompact get a trusted IRONMEM_MCP_MODE from env in <path>, so they mine "
            "(see `ironmem doctor`).",
            "does_not_write": "    Stop PreCompact do not mine: env in <path> sets IRONMEM_MCP_MODE to a value "
            "other than trusted (see `ironmem doctor`).",
            "unset": "    Stop PreCompact run read-only and skip mining (prompt recall still works) unless the "
            "environment Claude Code is launched with sets IRONMEM_MCP_MODE; trusted there makes them mine "
            "(see `ironmem doctor`).",
        }
        rows = json.loads(HOOK_STOP_FIXTURES.read_text(encoding="utf-8"))["env_rows"]
        self.assertGreaterEqual(len(rows), 10)
        for row in rows:
            settings = {"env": row["env"]} if "env" in row else {}
            with self.subTest(note=row["note"], settings=settings):
                self.assertEqual(self.mining_line_after_install(settings), lines[row["mines"]])

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
            self.assertIn("          jq said: stub jq failure\n", result.stderr)
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
            self.assertIn("          jq said: stub jq failure\n", result.stderr)
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
            self.assertIn("          jq said: stub jq failure\n", result.stderr)
            self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_a_jq_error_in_the_program_check_warns_and_leaves_the_event(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            stop = [{"hooks": [{"type": "command", "command": "/opt/elsewhere/ironmem hook stop"}]}]
            path = self.settings_with_stop(home, stop)
            env = self.jq_stub(home, "--arg ev Stop|--arg home ")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn(
                f"    WARN: jq failed checking which program the Stop hook in {path} runs; left it as it is.\n"
                "          jq said: stub jq failure\n",
                result.stderr,
            )
            self.assertNotIn("is already wired, so this install left it", result.stderr)
            settings = self.claude_settings(home)
            self.assertEqual(settings["hooks"]["Stop"], stop)
            self.assertEqual(set(settings["hooks"]), set(self.plugin_hook_names()))
            self.assertIn("Claude hooks already wired, left as they are: Stop", result.stdout)

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

    def test_a_failed_final_write_leaves_settings_unchanged_and_says_so(self) -> None:
        # The merged file is written to a temp file next to settings.json and
        # renamed onto it, so a write that fails partway (a full disk) leaves
        # settings.json as it was and no temp file behind.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_bytes()
            # Writes three bytes of the merged file, then fails, like a full disk.
            env = self.command_stub(home, "cat", "ironmem-hooks", 'head -c 3 "$1"; exit 1')

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_bytes(), before)
            self.assertIn(f"WARN: could not write {path}; wired no Claude hooks.", result.stderr)
            self.assertEqual(sorted(p.name for p in path.parent.iterdir()), ["settings.json"])
            self.assertNotIn("Wired Claude hooks", result.stdout)
            self.assertIn("[mcp_servers.ironmem]", (home / ".codex" / "config.toml").read_text(encoding="utf-8"))

    def test_the_write_replaces_the_file_a_symlink_chain_ends_at(self) -> None:
        # Two relative hops; both links stay links, and the file keeps its mode.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            real = home / "dots" / "sub" / "real.json"
            real.parent.mkdir(parents=True)
            real.write_text("{}", encoding="utf-8")
            real.chmod(0o640)
            hop = home / "dots" / "link2.json"
            hop.symlink_to("sub/real.json")
            link = home / ".claude" / "settings.json"
            link.parent.mkdir()
            link.symlink_to("../dots/link2.json")

            self.run_installer(home, home / ".claude.json")

            self.assertTrue(link.is_symlink() and hop.is_symlink())
            self.assertEqual(set(json.loads(real.read_text(encoding="utf-8"))["hooks"]), set(self.plugin_hook_names()))
            self.assertEqual(stat.S_IMODE(real.stat().st_mode), 0o640)
            self.assertEqual(sorted(p.name for p in real.parent.iterdir()), ["real.json"])

    def test_a_link_target_ending_in_a_newline_is_refused_not_written_to_its_sibling(self) -> None:
        # $(...) drops a trailing newline, so the name the installer follows
        # the link to is the sibling's. It must not rename the result there.
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            claude = home / ".claude"
            claude.mkdir()
            real = claude / "real.json\n"
            real.write_text('{"model": "real"}', encoding="utf-8")
            sibling = claude / "real.json"
            sibling.write_text('{"model": "sibling"}', encoding="utf-8")
            link = claude / "settings.json"
            link.symlink_to("real.json\n")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(sibling.read_text(encoding="utf-8"), '{"model": "sibling"}')
            self.assertEqual(real.read_text(encoding="utf-8"), '{"model": "real"}')
            self.assertTrue(link.is_symlink())
            self.assertEqual(sorted(p.name for p in claude.iterdir()), ["real.json", "real.json\n", "settings.json"])
            self.assertIn(
                f"WARN: could not follow the symlinks from {link}; wired no Claude hooks.",
                result.stderr,
            )

    def test_a_new_settings_file_is_private(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            self.run_installer(home, home / ".claude.json")
            self.assertEqual(stat.S_IMODE((home / ".claude" / "settings.json").stat().st_mode), 0o600)

    @unittest.skipIf(
        hasattr(os, "geteuid") and os.geteuid() == 0,
        "precondition: root writes into a mode 0555 directory",
    )
    def test_a_read_only_directory_refuses_the_write_up_front(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_bytes()
            path.parent.chmod(0o555)
            try:
                result = self.run_installer(home, home / ".claude.json")
            finally:
                path.parent.chmod(0o755)

            self.assertEqual(path.read_bytes(), before)
            self.assertIn(
                f"WARN: cannot write a new file in {path.parent}, the directory holding {path}; wired no Claude hooks.",
                result.stderr,
            )

    def test_a_settings_file_changed_during_the_run_is_not_overwritten(self) -> None:
        for existing in (True, False):
            with self.subTest(existing=existing), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                path = home / ".claude" / "settings.json"
                if existing:
                    self.settings_with_stop(home, [])
                else:
                    path.parent.mkdir(parents=True)
                # Another program writes settings.json while the merge runs: on
                # the last check of the merged work file, just before the write.
                other = '{"model": "written meanwhile"}'
                env = self.command_stub(home, "jq", '-e|type == "object"|ironmem-hooks', f"printf %s {shlex.quote(other)} > {shlex.quote(str(path))}")

                result = self.run_installer(home, home / ".claude.json", extra_env=env)

                self.assertEqual(path.read_text(encoding="utf-8"), other)
                self.assertIn(f"WARN: {path} changed while this script ran; wired no Claude hooks.", result.stderr)
                self.assertNotIn("Wired Claude hooks", result.stdout)

    def test_an_interrupted_wiring_removes_its_temp_files_and_stops(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            (home / "tmp").mkdir()
            # cp runs in the installer's own process, so $PPID is the installer.
            env = {
                **self.command_stub(home, "cp", "ironmem-hooks", "kill -TERM $PPID; exit 1"),
                "TMPDIR": str(home / "tmp"),
            }
            self.settings_with_stop(home, [])

            result = self.run_installer(home, home / ".claude.json", extra_env=env, expected_returncode=143)

            self.assertEqual(list((home / "tmp").iterdir()), [])
            self.assertNotIn("==> Done", result.stdout)

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

    def disabled_all_hooks_warning(self, path: pathlib.Path) -> str:
        return (
            f"    WARN: {path} sets disableAllHooks to true, so Claude Code runs none of these hooks unless a "
            "project's settings set it back to false. To turn them on, remove disableAllHooks from "
            "settings.json or set it to false.\n"
        )

    def test_disable_all_hooks_still_wires_and_warns_instead_of_the_mining_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"disableAllHooks": True}), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            # A project's settings can set it back to false, and then these run.
            self.assertEqual(set(self.claude_settings(home)["hooks"]), set(self.plugin_hook_names()))
            self.assertIn(self.disabled_all_hooks_warning(path), result.stderr)
            self.assertNotIn("prompt recall still works", result.stdout)
            self.assertNotIn("mine", result.stdout)

            again = self.run_installer(home, home / ".claude.json")

            self.assertIn("already wired, left as they are", again.stdout)
            self.assertIn(self.disabled_all_hooks_warning(path), again.stderr)

    def test_disable_all_hooks_false_or_not_a_boolean_true_changes_nothing(self) -> None:
        for value in (False, "true", 1):
            with self.subTest(value=value):
                line = self.mining_line_after_install({"disableAllHooks": value})
                self.assertIn("run read-only and skip mining", line)

    def test_disable_all_hooks_on_the_plugin_path_warns_too(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"disableAllHooks": True, "enabledPlugins": {"ironmem@ironrace": True}}), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertIn("come from the enabled ironmem plugin", result.stdout)
            self.assertIn(self.disabled_all_hooks_warning(path), result.stderr)

    def test_a_jq_error_reading_disable_all_hooks_wires_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            before = path.read_text(encoding="utf-8")
            env = self.jq_stub(home, "disableAllHooks")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertEqual(path.read_text(encoding="utf-8"), before)
            self.assertIn(f"WARN: jq failed reading disableAllHooks in {path}; wired no Claude hooks.", result.stderr)
            self.assertIn("          jq said: stub jq failure\n", result.stderr)

    def test_hand_wired_hooks_beside_the_enabled_plugin_get_a_warning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            self.run_installer(home, home / ".claude.json")
            path = home / ".claude" / "settings.json"
            settings = self.claude_settings(home)
            settings["enabledPlugins"] = {"ironmem@ironrace": True}
            path.write_text(json.dumps(settings), encoding="utf-8")
            before = path.read_text(encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertEqual(path.read_text(encoding="utf-8"), before, "the plugin path writes nothing")
            self.assertIn("Claude hooks come from the enabled ironmem plugin; wired none in", result.stdout)
            self.assertIn(
                f"    WARN: {path} also hand-wires SessionStart UserPromptSubmit Stop PreCompact, which the enabled "
                "ironmem plugin supplies; where both copies start, the hook runs twice. Remove the hand-wired "
                "ironmem entries from settings.json. `ironmem doctor` says which.\n",
                result.stderr,
            )

    def test_the_enabled_plugin_alone_gets_no_duplicate_warning(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"enabledPlugins": {"ironmem@ironrace": True}, "hooks": {"Stop": []}}), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            self.assertNotIn("hand-wires", result.stderr)

    def test_enabled_plugins_is_read_as_doctor_reads_it(self) -> None:
        # The rows of doctor's only_an_ironmem_plugin_key_set_to_true_enables_the_plugin
        # (crates/ironmem/src/doctor/hooks.rs), plus enabledPlugins values that
        # are not an object, which both read as not enabled.
        rows = (
            ({"ironmem@ironrace": True}, True),
            ({"ironmem": True}, True),
            ({"ironmem-extras@x": True}, False),
            ({"notironmem@x": True}, False),
            ({"ironmem@x": "true"}, False),
            ([], False),
            ("x", False),
            (None, False),
            (False, False),
        )
        for plugins, enabled in rows:
            with self.subTest(plugins=plugins), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                path = home / ".claude" / "settings.json"
                path.parent.mkdir(parents=True)
                path.write_text(json.dumps({"enabledPlugins": plugins}), encoding="utf-8")

                result = self.run_installer(home, home / ".claude.json")

                self.assertEqual("come from the enabled ironmem plugin" in result.stdout, enabled, result.stdout)
                self.assertEqual("hooks" in self.claude_settings(home), not enabled)

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
            # A literal script path: Claude Code leaves ${CLAUDE_PLUGIN_ROOT}
            # unset for a settings.json hook, so that spelling does not count.
            wrapper = "bash /p/hooks/ironmem-hook.sh"
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

    def plugin_root_warning(self, settings: pathlib.Path, event: str) -> str:
        return (
            f"    WARN: {event} in {settings} already has an ironmem hook entry that uses "
            "${CLAUDE_PLUGIN_ROOT}, a variable Claude Code sets only for plugin hooks, so that entry does not run "
            f"outside the plugin. This install added its own {event} hook beside it; you can delete the old entry.\n"
        )

    def test_a_plugin_root_command_in_settings_is_added_beside_with_a_warn(self) -> None:
        # Measured on Claude Code 2.1.329: a settings.json hook (no plugin)
        # runs with CLAUDE_PLUGIN_ROOT unset, so these find no script. The
        # rows are the shared fixture's, which doctor reads as not running.
        rows = json.loads(HOOK_STOP_FIXTURES.read_text(encoding="utf-8"))["rows"]
        root_rows = [row for row in rows if not row["wired"] and "CLAUDE_PLUGIN_ROOT" in json.dumps(row)]
        self.assertGreaterEqual(len(root_rows), 6)
        for row in root_rows:
            entry = {key: value for key, value in row.items() if key not in ("wired", "note")}
            with self.subTest(entry=entry), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                group = {"hooks": [{"type": "command", **entry}]}
                path = self.settings_with_stop(home, [group])

                result = self.run_installer(home, home / ".claude.json")

                stop = self.claude_settings(home)["hooks"]["Stop"]
                self.assertEqual(stop[0], group)
                self.assertEqual(len(stop), 2, stop)
                self.assertIn("hook stop --harness claude-code", json.dumps(stop[1]))
                self.assertIn(self.plugin_root_warning(path, "Stop"), result.stderr)
                # Only the event and the variable are named, never the entry.
                self.assertNotIn(entry["command"], result.stderr)
                self.assertEqual(result.stderr.count("CLAUDE_PLUGIN_ROOT"), 1, result.stderr)

    def test_no_plugin_root_warn_for_another_script_or_a_wired_event(self) -> None:
        for stop in (
            [{"hooks": [{"type": "command", "command": "bash ${CLAUDE_PLUGIN_ROOT}/hooks/other.sh stop"}]}],
            [{"hooks": [{"type": "command", "command": "bash ${CLAUDE_PLUGIN_ROOT}/hooks/ironmem-hook.sh stop"},
                        {"type": "command", "command": "bash /p/hooks/ironmem-hook.sh stop"}]}],
        ):
            with self.subTest(stop=stop), tempfile.TemporaryDirectory() as directory:
                home = pathlib.Path(directory)
                self.settings_with_stop(home, stop)

                result = self.run_installer(home, home / ".claude.json")

                self.assertNotIn("CLAUDE_PLUGIN_ROOT", result.stderr)

    def test_a_plugin_root_warn_per_event(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = home / ".claude" / "settings.json"
            path.parent.mkdir(parents=True)
            hooks = {
                event: [{"hooks": [{"type": "command", "command": f"bash ${{CLAUDE_PLUGIN_ROOT}}/hooks/ironmem-hook.sh {name}"}]}]
                for event, name in self.plugin_hook_names().items()
            }
            path.write_text(json.dumps({"hooks": hooks}), encoding="utf-8")

            result = self.run_installer(home, home / ".claude.json")

            for event in self.plugin_hook_names():
                self.assertIn(self.plugin_root_warning(path, event), result.stderr)
            self.assertIn("Wired Claude hooks", result.stdout)

    def test_a_jq_error_in_the_plugin_root_check_warns_and_still_adds(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            path = self.settings_with_stop(home, [])
            env = self.jq_stub(home, "--arg ev Stop --arg name stop |select(plugin_root_copy($name))")

            result = self.run_installer(home, home / ".claude.json", extra_env=env)

            self.assertIn(
                f"WARN: jq failed checking the Stop hooks in {path} for a ${{CLAUDE_PLUGIN_ROOT}} command; adding the hook anyway.",
                result.stderr,
            )
            self.assertIn("          jq said: stub jq failure\n", result.stderr)
            self.assertEqual(len(self.ironmem_hook_commands(self.claude_settings(home), "Stop")), 1)

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
            self.assertIn(
                f"Claude Code hooks were not wired either. Add these four to {home / '.claude' / 'settings.json'} "
                "(skip this if the ironmem Claude Code plugin is enabled)",
                result.stderr,
            )
            self.assertIn(
                f'"<Event>": [ {{ "hooks": [ {{ "type": "command", "command": "{target} hook '
                '<session-start|user-prompt-submit|stop|precompact> --harness claude-code" } ] } ]',
                result.stderr,
            )
            self.assertFalse((home / ".claude" / "settings.json").exists())


if __name__ == "__main__":
    unittest.main()
