#!/bin/bash
# Fake `claude` CLI for Piggy's engine tests. It NEVER touches a real Claude
# install: it reads PIGGY_CLAUDE_DIR (set by the test to a tempdir) and simulates
# the parts of `claude plugin ...` that Piggy observes — namely edits to
# settings.json's `enabledPlugins` — then records its argv so tests can assert on
# the exact commands the engine issued.
set -euo pipefail

DIR="${PIGGY_CLAUDE_DIR:?PIGGY_CLAUDE_DIR must be set}"
mkdir -p "$DIR"
SETTINGS="$DIR/settings.json"
LOG="$DIR/claude-shim.log"

# Record the full argv (one line per invocation).
printf '%s\n' "$*" >> "$LOG"

# Fail-injection hook for testing rollback: if PIGGY_SHIM_FAIL matches the joined
# args, exit non-zero without side effects.
if [ -n "${PIGGY_SHIM_FAIL:-}" ] && [ "$*" = "$PIGGY_SHIM_FAIL" ]; then
  echo "shim: simulated failure for '$*'" >&2
  exit 7
fi

sub="${1:-}"; verb="${2:-}"; target="${3:-}"

set_plugin() { # $1 = plugin@marketplace, $2 = true|false|remove
  python3 - "$SETTINGS" "$1" "$2" <<'PY'
import json, os, sys
path, plugin, op = sys.argv[1], sys.argv[2], sys.argv[3]
data = {}
if os.path.exists(path):
    with open(path, "r", encoding="utf-8") as f:
        txt = f.read().lstrip("﻿").strip()
        data = json.loads(txt) if txt else {}
ep = data.setdefault("enabledPlugins", {})
if op == "remove":
    ep.pop(plugin, None)
else:
    ep[plugin] = (op == "true")
with open(path, "w", encoding="utf-8") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
PY
}

if [ "$sub" = "plugin" ]; then
  case "$verb" in
    install) set_plugin "$target" true ;;
    uninstall) set_plugin "$target" remove ;;
    enable) set_plugin "$target" true ;;
    disable) set_plugin "$target" false ;;
    marketplace) : ;; # add/remove marketplace: no settings.json effect
    *) : ;;
  esac
fi

exit 0
