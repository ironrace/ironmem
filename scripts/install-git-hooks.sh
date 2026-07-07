#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/install-git-hooks.sh [--check]

Installs this repo's tracked hooks and fallback .git/hooks shims.

Options:
  --check   Verify core.hooksPath, tracked hook executability, and fallback
            shim contents without modifying the clone.
EOF
}

CHECK=0
for arg in "$@"; do
  case "$arg" in
    --check)
      CHECK=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

shim_content() {
  local hook_name="$1"
  cat <<EOF
#!/usr/bin/env bash
# ironmem managed hook shim; run scripts/install-git-hooks.sh to refresh.
set -euo pipefail
repo_root="\$(git rev-parse --show-toplevel)"
exec "\$repo_root/.githooks/$hook_name" "\$@"
EOF
}

normalize_hooks_path() {
  local value="$1"
  if [[ -z "$value" ]]; then
    return 0
  fi
  if [[ "$value" = /* ]]; then
    python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$value"
  else
    python3 -c 'import os,sys; print(os.path.realpath(os.path.join(sys.argv[1], sys.argv[2])))' "$REPO_ROOT" "$value"
  fi
}

EXPECTED_HOOKS_PATH="$(normalize_hooks_path ".githooks")"

check_install() {
  local failed=0
  local active
  active="$(git -C "$REPO_ROOT" config --local --get core.hooksPath || true)"
  local normalized_active
  normalized_active="$(normalize_hooks_path "$active")"

  if [[ "$normalized_active" != "$EXPECTED_HOOKS_PATH" ]]; then
    echo "ERROR: core.hooksPath is '${active:-<unset>}' but expected .githooks" >&2
    failed=1
  fi

  for hook_name in pre-commit pre-push; do
    if [[ ! -x "$REPO_ROOT/.githooks/$hook_name" ]]; then
      echo "ERROR: .githooks/$hook_name is not executable" >&2
      failed=1
    fi

    local shim="$REPO_ROOT/.git/hooks/$hook_name"
    if [[ ! -f "$shim" ]]; then
      echo "ERROR: fallback shim missing: .git/hooks/$hook_name" >&2
      failed=1
      continue
    fi
    if ! diff -u <(shim_content "$hook_name") "$shim" >/dev/null; then
      echo "ERROR: fallback shim drifted: .git/hooks/$hook_name" >&2
      failed=1
    fi
  done

  if [[ "$failed" -ne 0 ]]; then
    echo "Run: bash scripts/install-git-hooks.sh" >&2
  fi
  return "$failed"
}

if [[ "$CHECK" -eq 1 ]]; then
  check_install
  exit $?
fi

git -C "$REPO_ROOT" config core.hooksPath .githooks
chmod +x "$REPO_ROOT/.githooks/pre-commit" "$REPO_ROOT/.githooks/pre-push"
chmod +x "$REPO_ROOT/scripts/run_git_hook.py" "$REPO_ROOT/scripts/test_run_git_hook.py"

mkdir -p "$REPO_ROOT/.git/hooks"
for hook_name in pre-commit pre-push; do
  shim="$REPO_ROOT/.git/hooks/$hook_name"
  shim_content "$hook_name" > "$shim"
  chmod +x "$shim"
done

echo "Installed ironmem Git hooks (.githooks plus fallback .git/hooks shims)."
