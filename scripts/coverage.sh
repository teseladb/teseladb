#!/usr/bin/env bash
# Line, function and branch coverage per crate (001 FR-014), with rustc's instrumentation and
# llvm-tools. Branch coverage needs the nightly toolchain (`-Z coverage-options=branch`); the code
# and the tests are the same ones built with the pinned toolchain.
#   scripts/coverage.sh [crate...]           (no arguments: every crate of crates/ and tools/)
#   TESELADB_COV_MIN=95 scripts/coverage.sh  fails if a crate has lines, functions or branches below
#
# Writes coverage/report.txt (the table), coverage/missed.txt (functions never run and branch
# sides never taken, by source position) and coverage/show.txt (per-line detail) under the
# caller's `CARGO_TARGET_DIR` (default target), and builds in coverage-build next to them: two
# runs with different target directories (two agents, or CI next to a developer) never share them.
# The tests are built once and the built binaries run from there, so a source that changes while
# the run goes on (another agent editing the tree) cannot mix binaries of two versions; doctests
# are not run, since llvm-cov does not read their binaries.
# Bash 3.2 (macOS) or later.
set -euo pipefail
cd "$(dirname "$0")/.."

TOOLCHAIN=${TESELADB_COV_TOOLCHAIN:-nightly}
SYSROOT=$(rustc +"$TOOLCHAIN" --print sysroot)
HOST=$(rustc +"$TOOLCHAIN" -vV | sed -n 's/^host: //p')
BIN="$SYSROOT/lib/rustlib/$HOST/bin"
BASE=${CARGO_TARGET_DIR:-target}
OUT=$BASE/coverage
rm -rf "$OUT"
mkdir -p "$OUT/prof"

crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
  for d in crates/*/ tools/*/; do
    [[ -f $d/Cargo.toml ]] && crates+=("$(basename "$d")")
  done
fi
pkgs=()
for c in "${crates[@]}"; do pkgs+=(-p "$c"); done

# `coverage(off)` is only used in test modules: outside them the feature stays unused.
export RUSTFLAGS="-C instrument-coverage -Z coverage-options=branch --cfg coverage -A unused_features"
PROF=$(cd "$OUT/prof" && pwd)
export LLVM_PROFILE_FILE="$PROF/%p-%m.profraw"
export CARGO_TARGET_DIR=$BASE/coverage-build

# One line per test binary: `<executable> <package folder> <package name>`, from a single build.
cargo +"$TOOLCHAIN" test --offline -q "${pkgs[@]}" --no-run --message-format=json >"$OUT/build.json"
python3 - "$OUT/build.json" >"$OUT/tests.txt" <<'PY'
import json
import os
import sys

for line in open(sys.argv[1], encoding="utf-8"):
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["profile"]["test"]:
        package = os.path.dirname(m["manifest_path"])
        # `path+file:///…/<folder>#<version>`, or `…#<name>@<version>` when the name differs.
        fragment = m["package_id"].rpartition("#")[2]
        name = fragment.partition("@")[0] if "@" in fragment else os.path.basename(package)
        print(m["executable"], package, name, sep="\t")
PY

# Each binary runs in its package's folder with the variables cargo gives a test at run time;
# every binary runs even when one fails; the report is still written, and then the run fails
# naming those that did.
# What each binary printed goes to coverage/tests/<binary>.txt.
mkdir -p "$OUT/tests"
LOGS=$(cd "$OUT/tests" && pwd)
objects=()
failures=()
while IFS=$'\t' read -r exe package name; do
  if [[ ${#objects[@]} -eq 0 ]]; then objects+=("$exe"); else objects+=(-object "$exe"); fi
  log=$LOGS/$(basename "$exe").txt
  if ! (cd "$package" && CARGO_MANIFEST_DIR=$package CARGO_PKG_NAME=$name "$exe" -q >"$log" 2>&1); then
    failures+=("$exe")
  fi
done <"$OUT/tests.txt"

"$BIN/llvm-profdata" merge -sparse "$OUT"/prof/*.profraw -o "$OUT/merged.profdata"

# Tests and examples are diagnostic code, not product code.
IGNORE='(/\.cargo/|/rustc/|/tests/|/examples/|/target/)'
"$BIN/llvm-cov" show "${objects[@]}" -instr-profile="$OUT/merged.profdata" \
  -ignore-filename-regex="$IGNORE" -show-branches=count -show-line-counts-or-regions > "$OUT/show.txt"
"$BIN/llvm-cov" export "${objects[@]}" -instr-profile="$OUT/merged.profdata" \
  -ignore-filename-regex="$IGNORE" > "$OUT/export.json"

# llvm-cov writes real paths: the root without symbolic links makes them relative.
if python3 - "$OUT" "$(pwd -P)" "${TESELADB_COV_MIN:-}" "${crates[@]}" <<'PY'
import json
import re
import sys

out, root, minimum, crates = sys.argv[1], sys.argv[2] + "/", sys.argv[3], sys.argv[4:]
data = json.load(open(f"{out}/export.json"))["data"][0]
crate_of_path = re.compile(r"(?:crates|tools)/([^/]+)/src/")


def crate_of(path):
    m = crate_of_path.search(path)
    return m.group(1) if m and m.group(1) in crates else None


def relative(path):
    return path[len(root):] if path.startswith(root) else path


stats = {c: {"lines": [0, 0], "functions": {}, "branches": {}} for c in crates}
for f in data["files"]:
    c = crate_of(f["filename"])
    if c is None:
        continue
    lines = f["summary"]["lines"]
    stats[c]["lines"][0] += lines["covered"]
    stats[c]["lines"][1] += lines["count"]
    # A function compiled into two binaries with different hashes (the lib in its unit tests and as
    # a dependency) shows up twice and llvm-cov does not merge its branches: a side counts as taken
    # if it is taken in either of them.
    seen = stats[c]["branches"]
    for b in f["branches"]:
        key = (relative(f["filename"]), b[0], b[1])
        taken, not_taken = seen.get(key, (False, False))
        seen[key] = (taken or b[4] > 0, not_taken or b[5] > 0)

# Functions by the position of their first region, merged the same way.
for fn in data["functions"]:
    if not fn["regions"]:
        continue
    c = crate_of(fn["filenames"][0])
    if c is None:
        continue
    key = (relative(fn["filenames"][0]), fn["regions"][0][0], fn["regions"][0][1])
    funcs = stats[c]["functions"]
    funcs[key] = funcs.get(key, False) or fn["count"] > 0


def percent(covered, count):
    return 100.0 * covered / count if count else 100.0


report, missed, below = [], [], []
width = max(len(c) for c in crates)
for c in crates:
    s = stats[c]
    pct = {
        "lines": percent(*s["lines"]),
        "functions": percent(sum(s["functions"].values()), len(s["functions"])),
        "branches": percent(
            sum(int(t) + int(e) for t, e in s["branches"].values()), 2 * len(s["branches"])
        ),
    }
    report.append(
        f"{c:<{width}}  lines {pct['lines']:6.2f} %  functions {pct['functions']:6.2f} %"
        f"  branches {pct['branches']:6.2f} %"
    )
    gaps = [f"{p}:{l}:{col}: function never run" for (p, l, col), run in sorted(s["functions"].items()) if not run]
    for (p, l, col), (t, e) in sorted(s["branches"].items()):
        if not (t and e):
            gaps.append(f"{p}:{l}:{col}: " + " ".join(x for x, ok in (("true", t), ("false", e)) if not ok))
    missed += gaps
    if minimum:
        bad = [k for k in ("lines", "functions", "branches") if pct[k] < float(minimum)]
        if bad:
            below.append((c, bad, gaps))

open(f"{out}/report.txt", "w").write("\n".join(report) + "\n")
open(f"{out}/missed.txt", "w").write("\n".join(missed) + ("\n" if missed else ""))
print("\n".join(report))
for c, bad, gaps in below:
    print(f"coverage: {c} below {minimum}: {', '.join(bad)}")
    for g in gaps:
        print(f"  {g}")
sys.exit(1 if below else 0)
PY
then status=0; else status=1; fi
for f in ${failures[@]+"${failures[@]}"}; do
  echo "coverage: tests failed: $f"
  echo "  output in $LOGS/$(basename "$f").txt"
  status=1
done
exit "$status"
