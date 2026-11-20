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
        force_wiring: bool = False,
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
        if force_wiring:
            command.append("--force-wiring")
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


if __name__ == "__main__":
    unittest.main()
