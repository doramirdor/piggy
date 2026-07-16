#!/bin/bash
# Fake `python3` for Piggy's engine tests. It NEVER touches a real Python or the
# network: it simulates just the two things the venv/pip install steps observe —
# `python3 --version` and `python3 -m venv <dir>` — and materialises a fake venv
# whose `bin/pip` is a no-op and whose `bin/headroom` answers `--version`.
set -euo pipefail

# `python3 --version`
if [ "${1:-}" = "--version" ]; then
  echo "Python 3.12.4"
  exit 0
fi

# `python3 -m venv <dir>`
if [ "${1:-}" = "-m" ] && [ "${2:-}" = "venv" ]; then
  dir="${3:?venv dir required}"
  mkdir -p "$dir/bin"

  # Fake pip: accepts `install ...` and succeeds without doing anything.
  cat > "$dir/bin/pip" <<'PIP'
#!/bin/bash
exit 0
PIP
  chmod +x "$dir/bin/pip"

  # Fake headroom CLI: answers --version and no-ops wrap/doctor.
  cat > "$dir/bin/headroom" <<'HR'
#!/bin/bash
if [ "${1:-}" = "--version" ]; then echo "headroom 0.31.0"; exit 0; fi
exit 0
HR
  chmod +x "$dir/bin/headroom"
  exit 0
fi

exit 0
