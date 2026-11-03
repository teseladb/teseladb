#!/usr/bin/env bash
# Line, function and branch coverage per crate (001 FR-014), with rustc's instrumentation and
# llvm-tools. Branch coverage needs the nightly toolchain (`-Z coverage-options=branch`); the code
# and the tests are the same ones built with the pinned toolchain.
#   scripts/coverage.sh [crate...]           (no arguments: every crate of crates/ and tools/)
#   TESELADB_COV_MIN=95 scripts/coverage.sh  fails if a crate has lines, functions or branches below
#   TESELADB_COV_LINUX=1 scripts/coverage.sh  measures on the Linux kernel of the local Docker engine
#     (what only runs on Linux, such as io_uring; 012): the tests are built for
#     `<arch>-unknown-linux-musl` and run as scripts/test-linux.sh runs them
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
cargo_target=()
LINUX_IMAGE=teseladb-x86_64-qemu
if [[ ${TESELADB_COV_LINUX:-0} == 1 ]]; then
  OUT=$BASE/coverage-linux
  case $(uname -m) in
    arm64 | aarch64) linux_target=aarch64-unknown-linux-musl ;;
    *) linux_target=x86_64-unknown-linux-musl ;;
  esac
  cargo_target=(--target "$linux_target")
  export "CARGO_TARGET_$(echo "$linux_target" | tr 'a-z-' 'A-Z_')_LINKER"=rust-lld
  docker build -q -t "$LINUX_IMAGE" -f scripts/docker/x86_64-qemu.Dockerfile scripts/docker >/dev/null
fi
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
if [[ ${TESELADB_COV_LINUX:-0} == 1 ]]; then CARGO_TARGET_DIR=$BASE/coverage-linux-build; fi

# One line per test binary: `<executable> <package folder> <package name>`, from a single build;
# and in bins.txt the programs of the packages, which their tests start (`CARGO_BIN_EXE_<name>`):
# what only the program runs, its `main`, is measured from its own object.
cargo +"$TOOLCHAIN" test --offline -q "${pkgs[@]}" ${cargo_target[@]+"${cargo_target[@]}"} --no-run --message-format=json >"$OUT/build.json"
python3 - "$OUT/build.json" "$OUT/bins.txt" >"$OUT/tests.txt" <<'PY'
import json
import os
import sys

bins = open(sys.argv[2], "w", encoding="utf-8")
for line in open(sys.argv[1], encoding="utf-8"):
    m = json.loads(line)
    if m.get("reason") != "compiler-artifact" or not m.get("executable"):
        continue
    if not m["profile"]["test"]:
        if "bin" in m["target"]["kind"]:
            print(m["executable"], file=bins)
        continue
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
  if [[ ${TESELADB_COV_LINUX:-0} == 1 ]]; then
    # Without the seccomp profile (Docker's refuses io_uring) and with the battery's full disk.
    ran=(docker run --rm --security-opt seccomp=unconfined --tmpfs "/full:rw,size=1m"
      -e TESELADB_FULL_DIR=/full -e "LLVM_PROFILE_FILE=$LLVM_PROFILE_FILE"
      -e "CARGO_MANIFEST_DIR=$package" -e "CARGO_PKG_NAME=$name" -v "$PWD:$PWD" -w "$package"
      "$LINUX_IMAGE" "$exe" -q)
  else
    ran=(env "CARGO_MANIFEST_DIR=$package" "CARGO_PKG_NAME=$name" "$exe" -q)
  fi
  if ! (cd "$package" && "${ran[@]}" >"$log" 2>&1); then
    failures+=("$exe")
  fi
done <"$OUT/tests.txt"
while IFS= read -r exe; do objects+=(-object "$exe"); done <"$OUT/bins.txt"

"$BIN/llvm-profdata" merge -sparse "$OUT"/prof/*.profraw -o "$OUT/merged.profdata"

# Tests and examples are diagnostic code, not product code.
IGNORE='(/\.cargo/|/rustc/|/tests/|/examples/|/target/)'
"$BIN/llvm-cov" show "${objects[@]}" -instr-profile="$OUT/merged.profdata" \
  -ignore-filename-regex="$IGNORE" -show-branches=count -show-line-counts-or-regions > "$OUT/show.txt"
# One export per binary, merged below: given several binaries, llvm-cov keeps only the first
# record of each function, and a binary that links a function without calling it carries a record
# with no counts that hides the counts of the binary that ran it («functions have mismatched
# data»). Only the sources of the measured crates are exported. The warning `show` prints is about
# those duplicates across binaries; one binary alone still warns for the few functions whose record
# has another hash in the profile (in teseladb-sim only `Sim::registry`, a getter without
# branches, measured 03-11-2026), which its export leaves out.
sources=()
for c in "${crates[@]}"; do
  for d in crates/"$c"/src tools/"$c"/src; do [[ -d $d ]] && sources+=("$(cd "$d" && pwd -P)"); done
done
mkdir -p "$OUT/export"
n=0
for exe in "${objects[@]}"; do
  [[ $exe == -object ]] && continue
  n=$((n + 1))
  # A test file of another platform (`#![cfg(target_os = ...)]`) builds an empty binary with
  # nothing to measure: it is left out. Any other failure stops the run.
  if ! "$BIN/llvm-cov" export "$exe" -instr-profile="$OUT/merged.profdata" \
    -ignore-filename-regex="$IGNORE" ${sources[@]+"${sources[@]}"} > "$OUT/export/$n.json" \
    2>"$OUT/export/$n.err"; then
    grep -q "no coverage data found" "$OUT/export/$n.err" || { cat "$OUT/export/$n.err" >&2; exit 1; }
    rm "$OUT/export/$n.json"
  fi
  rm "$OUT/export/$n.err"
done

# llvm-cov writes real paths: the root without symbolic links makes them relative.
if python3 - "$OUT" "$(pwd -P)" "${TESELADB_COV_MIN:-}" "${crates[@]}" <<'PY'
import json
import os
import re
import sys

out, root, minimum, crates = sys.argv[1], sys.argv[2] + "/", sys.argv[3], sys.argv[4:]
exports = [
    json.load(open(os.path.join(f"{out}/export", name)))["data"][0]
    for name in sorted(os.listdir(f"{out}/export"))
]
crate_of_path = re.compile(r"(?:crates|tools)/([^/]+)/src/")


def crate_of(path):
    m = crate_of_path.search(path)
    return m.group(1) if m and m.group(1) in crates else None


def relative(path):
    return path[len(root):] if path.startswith(root) else path


def line_counts(segments):
    """Whether each line of a file ran, from the file's segments, which hold every instantiation of
    its generic functions: the rule of llvm's `LineCoverageStats`, the one `llvm-cov show` prints.
    A segment is `[line, column, count, has_count, is_region_entry, is_gap_region]`. llvm's file
    summary instead keeps, for each generic function, its single best-covered instantiation, and
    misses the lines another instantiation ran."""
    ran = {}
    wrapped = None
    i = 0
    while i < len(segments):
        line = segments[i][0]
        starts = []
        while i < len(segments) and segments[i][0] == line:
            starts.append(segments[i])
            i += 1
        entries = [s for s in starts if s[3] and s[4] and not s[5]]
        skipped = not starts[0][3] and starts[0][4]
        if not skipped and ((wrapped is not None and wrapped[3]) or entries):
            count = wrapped[2] if wrapped is not None else 0
            for s in entries:
                count = max(count, s[2])
            ran[line] = count > 0
        wrapped = starts[-1]
        # The lines up to the next segment are inside the last one of this line.
        nxt = segments[i][0] if i < len(segments) else line + 1
        if wrapped[3]:
            for between in range(line + 1, nxt):
                ran[between] = wrapped[2] > 0
    return ran


stats = {c: {"lines": {}, "functions": {}, "branches": {}} for c in crates}
for data in exports:
    for f in data["files"]:
        c = crate_of(f["filename"])
        if c is None:
            continue
        # A line runs if it ran in any binary.
        lines = stats[c]["lines"].setdefault(relative(f["filename"]), {})
        for line, ran in line_counts(f["segments"]).items():
            lines[line] = lines.get(line, False) or ran
        # A function compiled into two binaries with different hashes (the lib in its unit tests
        # and as a dependency) shows up twice and llvm-cov does not merge its branches: a side
        # counts as taken if it is taken in either of them.
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
for s in stats.values():
    s["lines"] = [
        sum(sum(ran.values()) for ran in s["lines"].values()),
        sum(len(ran) for ran in s["lines"].values()),
    ]


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
