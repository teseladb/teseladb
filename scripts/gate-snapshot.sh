#!/usr/bin/env bash
# Runs scripts/ci.sh on a snapshot of the working tree in a worktree of its own, so that the tree
# can keep changing while the gates run (they take minutes, and a build in progress would mix the
# edits made meanwhile into what it checks).
#   scripts/gate-snapshot.sh [arguments of scripts/ci.sh, such as --quick]
# The snapshot is a commit of every tracked and untracked file that .gitignore does not exclude,
# made with an index of its own: the branch, its index and the stash stay as they were. It is
# checked out detached in `<this worktree>-gate` (created the first time, next to this one), whose
# build directory is its own and builds without incremental artifacts, the largest part of a
# build directory. The pinned downloads of .tools/ come from this worktree. Prints the snapshot's
# commit and leaves the gates' output in `<gate worktree>/gate.log`; the exit code is ci.sh's.
set -euo pipefail
ROOT=$(git rev-parse --show-toplevel)
GATE=$ROOT-gate

index=$(mktemp "${TMPDIR:-/tmp}/gate-index.XXXXXX")
trap 'rm -f "$index"' EXIT
cp "$(git rev-parse --git-path index)" "$index"
GIT_INDEX_FILE=$index git add -A
tree=$(GIT_INDEX_FILE=$index git write-tree)
commit=$(git commit-tree "$tree" -p HEAD -m "gate snapshot")
echo "gate-snapshot: $commit"

if [[ -d $GATE ]]; then
  git -C "$GATE" checkout --quiet --detach --force "$commit"
  git -C "$GATE" clean -fdq -e target -e .tools
else
  git worktree add --quiet --detach "$GATE" "$commit"
fi
if [[ -d $ROOT/.tools && ! -e $GATE/.tools ]]; then
  ln -s "$ROOT/.tools" "$GATE/.tools"
fi

cd "$GATE"
rc=0
CARGO_INCREMENTAL=0 scripts/ci.sh "$@" >"$GATE/gate.log" 2>&1 || rc=$?
grep -E '^== .*: (OK|FAILED)' "$GATE/gate.log" | tail -3 || true
echo "gate-snapshot: exit $rc; output in $GATE/gate.log"
exit "$rc"
