#!/usr/bin/env bash
#
# Cross-check the Rust parser against an independent jq reduction over 3 real
# session files. Both sides:
#   * skip malformed lines,
#   * keep only type=="assistant" with model != "<synthetic>",
#   * dedupe by requestId (fallback message.id, fallback uuid), last-wins,
#   * sum the four token streams.
#
# The parser's per-model figures are summed across models so the two overall
# totals must match exactly. Exit non-zero on any mismatch.
#
# Usage: PIGGY_BIN=target/release/piggy scripts/verify-against-jq.sh [file ...]

set -euo pipefail

BIN="${PIGGY_BIN:-target/release/piggy}"
if [ ! -x "$BIN" ]; then
  echo "error: piggy binary not found at $BIN (build with: cargo build --release -p piggy-cli)" >&2
  exit 2
fi

# Collect input files (portable; avoids mapfile which is bash 4+).
FILES=()
if [ "$#" -gt 0 ]; then
  for a in "$@"; do FILES+=("$a"); done
else
  while IFS= read -r line; do
    [ -n "$line" ] && FILES+=("$line")
  done < <(find "$HOME/.claude/projects" -name '*.jsonl' -size +20k 2>/dev/null | head -3)
fi

if [ "${#FILES[@]}" -eq 0 ]; then
  echo "error: no input files found" >&2
  exit 2
fi

# Parser side: sum token streams across all models -> "in out cc cr".
parser_totals() {
  "$BIN" parse "$1" --json | jq -r '
    [.models[]]
    | { i:(map(.input_tokens)|add // 0),
        o:(map(.output_tokens)|add // 0),
        cc:(map(.cache_creation_tokens)|add // 0),
        cr:(map(.cache_read_tokens)|add // 0) }
    | "\(.i) \(.o) \(.cc) \(.cr)"'
}

# jq side: independent lenient reduction over the raw file -> "in out cc cr".
jq_totals() {
  jq -R 'fromjson? // empty' "$1" | jq -s -r '
    [ .[] | select(.type=="assistant" and (.message.model != "<synthetic>")) ]
    | reduce .[] as $l ({}; .[($l.requestId // $l.message.id // $l.uuid // "nokey")] = $l.message.usage)
    | [ .[] ]
    | { i:(map(.input_tokens // 0)|add // 0),
        o:(map(.output_tokens // 0)|add // 0),
        cc:(map(.cache_creation_input_tokens // 0)|add // 0),
        cr:(map(.cache_read_input_tokens // 0)|add // 0) }
    | "\(.i) \(.o) \(.cc) \(.cr)"'
}

fail=0
for f in "${FILES[@]}"; do
  echo "== $f"
  read -r pi po pcc pcr <<< "$(parser_totals "$f")"
  read -r ji jo jcc jcr <<< "$(jq_totals "$f")"
  echo "  parser: in=$pi out=$po cache_write=$pcc cache_read=$pcr"
  echo "  jq:     in=$ji out=$jo cache_write=$jcc cache_read=$jcr"
  if [ "$pi" = "$ji" ] && [ "$po" = "$jo" ] && [ "$pcc" = "$jcc" ] && [ "$pcr" = "$jcr" ]; then
    echo "  OK"
  else
    echo "  MISMATCH"
    fail=1
  fi
done

exit "$fail"
