#!/usr/bin/env bash
# Enforces the engine's internal dependency rules (ARCHITECT.md §3).
# Fails on any `crate::<module>` reference the table forbids.
set -euo pipefail

src="$(cd "$(dirname "$0")/../kamili/src" && pwd)"

allowed() {
  case "$1" in
    spec | llm | sandbox | config | skills | index) echo "" ;;
    agents) echo "llm" ;;
    engine) echo "spec llm agents sandbox config skills index" ;;
    api) echo "engine config llm" ;;
    *) echo "__unknown__" ;;
  esac
}

status=0

# Grouped imports (`use crate::{a, b}`) would hide edges from the check below.
if grep -rn --include='*.rs' 'crate::{' "$src"; then
  echo "error: use one \`crate::<module>\` path per import so the check can see it" >&2
  status=1
fi

for dir in "$src"/*/; do
  module="$(basename "$dir")"
  rules="$(allowed "$module")"
  if [ "$rules" = "__unknown__" ]; then
    echo "error: module \`$module\` has no rule in scripts/check-module-deps.sh" >&2
    status=1
    continue
  fi
  used="$(grep -rhoE --include='*.rs' 'crate::[a-z_]+' "$dir" | sed 's/^crate:://' | sort -u || true)"
  for dep in $used; do
    [ "$dep" = "$module" ] && continue
    case " $rules " in
      *" $dep "*) ;;
      *) echo "error: \`$module\` must not use \`$dep\`" >&2; status=1 ;;
    esac
  done
done

[ "$status" -eq 0 ] && echo "module dependencies OK"
exit "$status"
