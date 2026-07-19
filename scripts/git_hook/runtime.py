"""Process facts shared by both subprocess-running layers.

`collect` and `execute` each shell out, and both need the same two things: the
repository root to run in, and a child environment with Git's own injected
variables stripped. Neither layer owns them -- `_scrub_git_env` in particular
used to live with the execution layer while being called from the collection
layer too, which is what this module resolves.
"""
from __future__ import annotations

import pathlib
from typing import Mapping

# scripts/git_hook/runtime.py -> scripts/ -> repo root.
ROOT = pathlib.Path(__file__).resolve().parents[2]


# Wall-clock ceiling for every Git subprocess this module runs. A hung Git
# (an unreachable remote, a stuck index.lock, a credential helper waiting on a
# tty) would otherwise hang the hook -- and therefore the developer's commit or
# push -- forever with no output. Exceeding it raises `TimeoutExpired`, which
# the collection layer's fail-closed boundary turns into `unknown=True`.
_GIT_TIMEOUT_SECONDS = 60


# Both layers pass `_scrub_git_env` to every subprocess they start. Scrubbing
# only outbound gate invocations would leave the Git calls that *decide* which
# gates run redirectable, which fails open (see `collect._run_git`'s
# docstring) -- which is why this lives here rather than with either layer.
#
# Repo-redirecting variables: inheriting any of these points a child Git
# invocation at a *different* repository than the one the hook is running
# in. This is not theoretical -- a pre-push hook exporting GIT_DIR/
# GIT_INDEX_FILE/GIT_WORK_TREE let a `cargo test` tempdir Git fixture
# inherit them and commit into the real repo (PR #186). Always stripped;
# never in the keep-list.
#   GIT_DIR / GIT_WORK_TREE / GIT_INDEX_FILE   -- which repo/worktree/index
#   GIT_OBJECT_DIRECTORY / GIT_ALTERNATE_OBJECT_DIRECTORIES -- which object store
#   GIT_COMMON_DIR                              -- which shared worktree dir
#   GIT_NAMESPACE                                -- which ref namespace
#
# Keep-list: variables that configure *how* Git authenticates or reports,
# not *where* it points. GIT_CONFIG_* is deliberately NOT here (see below).
#   GIT_ASKPASS, GIT_SSH, GIT_SSH_COMMAND -- each execs a caller-chosen helper
#                                              program to authenticate over SSH.
#                                              They cannot redirect a child Git
#                                              invocation at a different repo,
#                                              and dropping them breaks
#                                              legitimate SSH-agent wrappers and
#                                              CI credential setups -- kept
#                                              despite executing arbitrary code,
#                                              not because they are inert.
#   GIT_TERMINAL_PROMPT                    -- whether Git may prompt interactively
#   GIT_TRACE*                             -- GIT_TRACE=<path> appends trace
#                                              output to that path: a file-write
#                                              primitive, not merely diagnostic
#                                              output, but it cannot redirect a
#                                              child Git invocation at another
#                                              repository the way GIT_CONFIG_*
#                                              can (see below), so it stays.
#
# GIT_CONFIG_* is excluded on purpose, not an oversight: GIT_CONFIG_COUNT +
# GIT_CONFIG_KEY_n/GIT_CONFIG_VALUE_n are the documented equivalent of
# `git -c <key>=<value>` for ARBITRARY config, and GIT_CONFIG_GLOBAL/
# GIT_CONFIG_SYSTEM (also matched by this prefix) replace whole config files.
# That includes core.worktree -- the config equivalent of GIT_WORK_TREE, which
# this module strips above as a repo-redirecting variable -- plus core.bare,
# core.hooksPath, credential.helper, core.pager, core.sshCommand, alias.*, and
# *.textconv. A caller could set GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=
# core.worktree GIT_CONFIG_VALUE_0=/real/repo and reproduce exactly the
# redirection this scrub exists to prevent (PR #186) through the front door,
# so it is stripped like any other unrecognized GIT_*-prefixed variable.
#
# SCOPE -- what this scrub does and does not claim. The invariant is "no
# variable Git injects into the hook environment leaks into a child", NOT the
# broader "no kept variable can influence Git", which is false and was
# previously asserted here. HOME and XDG_CONFIG_HOME survive the scrub and
# reach the very same arbitrary-config primitive as GIT_CONFIG_*: Git reads
# $XDG_CONFIG_HOME/git/config and $HOME/.gitconfig, either of which can set
# core.worktree, core.hooksPath, or credential.helper. They are kept anyway,
# deliberately:
#   - Provenance. GIT_DIR/GIT_INDEX_FILE/... are set *by Git* when it invokes
#     a hook, so they are present by default and leak outward -- that is the
#     PR #186 bug this scrub was built for. HOME/XDG_CONFIG_HOME are ambient
#     developer environment that every gate (cargo needs HOME for ~/.cargo)
#     already depends on; dropping them breaks the gates outright.
#   - PATH dominates them. PATH cannot be scrubbed -- the gates need it to
#     find git and cargo at all -- and whoever can set PATH can replace the
#     git binary, which is strictly more power than redirecting its config.
#     Scrubbing XDG_CONFIG_HOME while PATH stays writable is theater.
# A hostile ambient environment is therefore explicitly out of this module's
# threat model. Both halves are pinned by tests: `test_scrub_git_env_passes_
# ambient_config_vars_through_by_design` and
# `test_scrub_git_env_strips_every_var_git_injects_into_a_hook`.
#
# Default toward scrubbing: anything GIT_*-prefixed that isn't explicitly
# named or prefix-matched here is dropped, including variables not yet
# invented. The keep-list is an allowlist, not a denylist.
_GIT_ENV_KEEP_EXACT: frozenset[str] = frozenset(
    {"GIT_ASKPASS", "GIT_SSH", "GIT_SSH_COMMAND", "GIT_TERMINAL_PROMPT"}
)
_GIT_ENV_KEEP_PREFIXES: tuple[str, ...] = ("GIT_TRACE",)


def _scrub_git_env(source_env: Mapping[str, str]) -> dict[str, str]:
    """Build a child-process env from `source_env` (normally `os.environ`)
    with every `GIT_*` variable removed except the explicit keep-list above.

    Returns a new dict; never mutates `source_env`. Non-`GIT_*` variables
    (PATH, HOME, XDG_CONFIG_HOME, ...) always pass through untouched -- see
    the SCOPE note above for why HOME/XDG_CONFIG_HOME are kept even though
    they reach the same arbitrary-Git-config primitive as the `GIT_CONFIG_*`
    variables this function strips. This function guarantees that no
    Git-injected variable leaks into a child; it does not, and cannot, defend
    against a hostile ambient environment.
    """
    return {
        key: value
        for key, value in source_env.items()
        if not key.startswith("GIT_")
        or key in _GIT_ENV_KEEP_EXACT
        or key.startswith(_GIT_ENV_KEEP_PREFIXES)
    }
