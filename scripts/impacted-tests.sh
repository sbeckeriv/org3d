#!/usr/bin/env bash
set -euo pipefail

BASE=${1:-origin/main}

entity_ids=$(sem diff "$BASE" HEAD --json | jq -r '.changes[].entityId')

if [[ -z "$entity_ids" ]]; then
  echo "No entity changes — running full test suite" >&2
  cargo test
  exit 0
fi

test_names=$(
  echo "$entity_ids" | while IFS= read -r id; do
    sem impact --entity-id "$id" --tests --json 2>/dev/null \
      | jq -r '.tests[].name'
  done | sort -u
)

if [[ -z "$test_names" ]]; then
  echo "No tests impacted by these changes" >&2
  exit 0
fi

echo "Impacted tests:" >&2
echo "$test_names" >&2
echo "---" >&2

filter=$(echo "$test_names" | jq -Rrs 'split("\n") | map(select(. != "")) | join("|")')
cargo test -- "$filter"
