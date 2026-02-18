#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKDIR="${1:-.octorus-local-watch-demo}"
DELAY_SECONDS="${OCTORUS_LOCAL_WATCH_DELAY:-3}"

STEP=1
wait_step() {
  local message="$1"
  printf '\n[step %d] %s\n' "$STEP" "$message"
  sleep "$DELAY_SECONDS"
  STEP=$((STEP + 1))
}

main() {
  cd "$REPO_ROOT"
  printf 'Repository: %s\n' "$REPO_ROOT"
  printf 'Workdir:   %s\n' "$WORKDIR"
  printf 'Delay:    %s seconds\n' "$DELAY_SECONDS"
  printf 'Cleanup arg: --cleanup\n'

  if [ -e "$WORKDIR" ]; then
    rm -rf "$WORKDIR"
  fi
  mkdir -p "$WORKDIR/subdir"

  wait_step "Create base files (a few local diffs should appear)"
  printf 'base-a\n' > "$WORKDIR/a.txt"
  printf 'base-b\n' > "$WORKDIR/b.txt"
  printf 'base-c\n' > "$WORKDIR/c.txt"

  wait_step "Modify a file near the top"
  printf 'changed-a-1\n' > "$WORKDIR/a.txt"

  wait_step "Modify bottom file"
  printf 'changed-c-1\n' > "$WORKDIR/c.txt"

  wait_step "Create a new file under subdir"
  printf 'new-sub-file\n' > "$WORKDIR/subdir/d.txt"

  wait_step "Modify middle file"
  printf 'changed-b-1\n' > "$WORKDIR/b.txt"

  wait_step "Modify top and bottom (same distance from middle)"
  printf 'changed-a-2\n' > "$WORKDIR/a.txt"
  printf 'changed-c-2\n' > "$WORKDIR/c.txt"

  wait_step "Delete middle file"
  rm -f "$WORKDIR/b.txt"

  wait_step "Recreate middle file and modify"
  printf 'recreated-b\n' > "$WORKDIR/b.txt"

  wait_step "Create a far file"
  printf 'far-tail\n' > "$WORKDIR/z_tail.txt"

  wait_step "Delete a subdir file"
  rm -f "$WORKDIR/subdir/d.txt"

  printf '\nDemo finished. To keep workspace clean, run:\n  rm -rf %s\n' "$WORKDIR"
}

main "$@"
