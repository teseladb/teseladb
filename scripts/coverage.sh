#!/usr/bin/env bash
# Line, function and branch coverage per crate (001 FR-014), with rustc's instrumentation and
# llvm-tools. Branch coverage needs the nightly toolchain (`-Z coverage-options=branch`); the code
# and the tests are the same ones built with the pinned toolchain.
#   scripts/coverage.sh [crate...]           (no arguments: every crate of crates/ and tools/ this
#                                            host builds; outside Linux, all but LINUX_CRATES)
#   TESELADB_COV_MIN=95 scripts/coverage.sh  fails if a crate has lines, functions or branches below
#   TESELADB_COV_LINUX=1 scripts/coverage.sh  measures on the Linux kernel of the local Docker engine
#     the crates that only run on Linux (LINUX_CRATES by default: the engine, teseladb-sys,
#     teseladb-io, teseladb-server and teseladb-bench; 023 FR-007, FR-040): this script runs again
#     inside a privileged container of the image of scripts/test-linux.sh, where it builds for
#     `<arch>-unknown-linux-gnu` (023 R10) and runs the tests natively
#
# The crates are named by their folder (`teseladb-engine` is the engine, whose package is
# `teseladb`). Every test binary runs, the programs with their own `main` (`harness = false`)
# included: the engine's two that a Python driver of its tests/data starts (metrics_tester and
# rest_api_httpd) run through it, and each test binary of the engine runs once per reactor backend
# of TESELADB_COV_ENGINE_BACKENDS (default `io_uring linux-aio epoll`), chosen by the engine's
# configuration file under a `$HOME` of its own, one test case at a time, as the engine's own
# coverage runs measure it.
#
# Writes coverage/report.txt (the table), coverage/missed.txt (functions never run and branch
# sides never taken, by source position) and coverage/show.txt (each measured source with the
# times each line ran, summed over every binary) under the
# caller's `CARGO_TARGET_DIR` (default target), and builds in coverage-build next to them: two
# runs with different target directories (two agents, or CI next to a developer) never share them.
# On Linux through Docker they go to coverage-linux, and the build to the volume of
# scripts/test-linux.sh (TESELADB_LINUX_VOLUME), where the nightly toolchain is installed once.
# The tests are built once and the built binaries run from there, so a source that changes while
# the run goes on (another agent editing the tree) cannot mix binaries of two versions; doctests
# are not run, since llvm-cov does not read their binaries.
# Bash 3.2 (macOS) or later.
set -euo pipefail
cd "$(dirname "$0")/.."

# The crates (by folder) that build and run only on Linux (023 FR-007, D-177).
LINUX_CRATES=(teseladb-engine teseladb-sys teseladb-io teseladb-server teseladb-bench)
# The engine's package: its test binaries run once per reactor backend.
ENGINE=teseladb
ENGINE_BACKENDS=${TESELADB_COV_ENGINE_BACKENDS:-io_uring linux-aio epoll}
LINUX_IMAGE=${TESELADB_LINUX_IMAGE:-teseladb-linux-test:1.98.0}
LINUX_VOLUME=${TESELADB_LINUX_VOLUME:-teseladb-linux-target}
# The engine's reactors run linux-aio with many contexts (as its own coverage runs set it).
AIO_MAX_NR=1048576

is_linux_crate() {
  local c
  for c in "${LINUX_CRATES[@]}"; do [[ $1 == "$c" ]] && return 0; done
  return 1
}

on_linux() {
  [[ $(uname -s) == Linux ]]
}

TOOLCHAIN=${TESELADB_COV_TOOLCHAIN:-nightly}
BASE=${CARGO_TARGET_DIR:-target}

# The Linux run from another host: this script again, inside the container, with the repository at
# its own path (the tests find their files through paths fixed at build time).
if [[ ${TESELADB_COV_LINUX:-0} == 1 && ${TESELADB_COV_INSIDE:-0} != 1 ]]; then
  mkdir -p "$BASE/coverage-linux"
  out=$(cd "$BASE/coverage-linux" && pwd)
  registry=${CARGO_HOME:-$HOME/.cargo}/registry
  args=("$@")
  [[ ${#args[@]} -gt 0 ]] || args=("${LINUX_CRATES[@]}")
  exec docker run --rm -i --privileged --init --dns 127.0.0.1 --tmpfs /full:rw,size=1m \
    --ulimit memlock=-1 -e TESELADB_FULL_DIR=/full -e TESELADB_COV_INSIDE=1 -e TESELADB_COV_LINUX=1 \
    -e TESELADB_COV_MIN="${TESELADB_COV_MIN:-}" -e TESELADB_COV_TOOLCHAIN="$TOOLCHAIN" \
    -e TESELADB_COV_ENGINE_BACKENDS="$ENGINE_BACKENDS" -e TESELADB_COV_OUT="$out" \
    -e TESELADB_COV_BUILD=/target/coverage-linux-build -e RUSTUP_HOME=/target/cov-rustup \
    -e CARGO_HOME=/cargo-home -v "$registry:/cargo-home/registry" -v "$LINUX_VOLUME:/target" \
    -v "$PWD:$PWD" -w "$PWD" "$LINUX_IMAGE" bash scripts/coverage.sh "${args[@]}"
fi

if [[ ${TESELADB_COV_INSIDE:-0} == 1 ]]; then
  # The image has the pinned stable toolchain only: the nightly one and its llvm-tools are installed
  # once into the volume, and later runs reuse them.
  if ! rustup run "$TOOLCHAIN" rustc -V >/dev/null 2>&1; then
    rustup toolchain install "$TOOLCHAIN" --no-self-update --profile minimal -c llvm-tools >&2
  fi
  # The tools link the toolchain's shared libLLVM, which lives in the sysroot's lib/.
  nightly_lib=$(rustc +"$TOOLCHAIN" --print sysroot)/lib
  export LD_LIBRARY_PATH=$nightly_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
  echo "$AIO_MAX_NR" >/proc/sys/fs/aio-max-nr
fi
SYSROOT=$(rustc +"$TOOLCHAIN" --print sysroot)
HOST=$(rustc +"$TOOLCHAIN" -vV | sed -n 's/^host: //p')
BIN="$SYSROOT/lib/rustlib/$HOST/bin"
OUT=${TESELADB_COV_OUT:-$BASE/coverage}
rm -rf "$OUT"
mkdir -p "$OUT/prof"

crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
  for d in crates/*/ tools/*/; do
    [[ -f $d/Cargo.toml ]] || continue
    c=$(basename "$d")
    if ! on_linux && is_linux_crate "$c"; then continue; fi
    crates+=("$c")
  done
fi
pkgs=()
for c in "${crates[@]}"; do
  if ! on_linux && is_linux_crate "$c"; then
    echo "coverage: $c runs on Linux only: measure it with TESELADB_COV_LINUX=1" >&2
    exit 2
  fi
  manifest=crates/$c/Cargo.toml
  [[ -f $manifest ]] || manifest=tools/$c/Cargo.toml
  [[ -f $manifest ]] || { echo "coverage: no crate $c in crates/ or tools/" >&2; exit 2; }
  pkgs+=(-p "$(awk -F'"' '/^name = / { print $2; exit }' "$manifest")")
done

# `coverage(off)` is only used in test modules: outside them the feature stays unused.
export RUSTFLAGS="-C instrument-coverage -Z coverage-options=branch --cfg coverage -A unused_features"
PROF=$(cd "$OUT/prof" && pwd)
export LLVM_PROFILE_FILE="$PROF/%p-%m.profraw"
export CARGO_TARGET_DIR=${TESELADB_COV_BUILD:-$BASE/coverage-build}

# One line per test binary: `<executable> <package folder> <package name> <harness> <target>`, from
# a single build (`harness` is 0 for a test target with its own `main`, which rustc builds without
# `--test`); and in bins.txt the programs of the packages, which their tests start
# (`CARGO_BIN_EXE_<name>`): what only the program runs, its `main`, is measured from its own object.
cargo +"$TOOLCHAIN" test --offline -q "${pkgs[@]}" --no-run --message-format=json >"$OUT/build.json"
python3 - "$OUT/build.json" "$OUT/bins.txt" >"$OUT/tests.txt" <<'PY'
import json
import os
import sys

bins = open(sys.argv[2], "w", encoding="utf-8")
for line in open(sys.argv[1], encoding="utf-8"):
    m = json.loads(line)
    if m.get("reason") != "compiler-artifact" or not m.get("executable"):
        continue
    harness = m["profile"]["test"]
    if not harness and "test" not in m["target"]["kind"]:
        if "bin" in m["target"]["kind"]:
            print(m["executable"], file=bins)
        continue
    package = os.path.dirname(m["manifest_path"])
    # `path+file:///…/<folder>#<version>`, or `…#<name>@<version>` when the name differs.
    fragment = m["package_id"].rpartition("#")[2]
    name = fragment.partition("@")[0] if "@" in fragment else os.path.basename(package)
    print(m["executable"], package, name, int(harness), m["target"]["name"], sep="\t")
PY

# Each binary runs in its package's folder with the variables cargo gives a test at run time;
# every binary runs even when one fails; the report is still written, and then the run fails
# naming those that did.
# What each binary printed goes to coverage/tests/<binary>.txt.
# Each test binary writes its profiles to prof/<n>/, and is read with a profile of its own: a
# profile merged from every binary gives each one the counts the others ran of a function they
# share, and its records with another hash are dropped («functions have mismatched data»). Its
# signature (the `%m` of the profile's name) comes from a first run that only lists its tests, and
# tells its own profiles from those of the programs it starts. A program with its own `main` takes
# no `--list`: every profile it leaves is read as its own (the engine's and teseladb-io's start no
# other program, at most themselves again).
mkdir -p "$OUT/tests" "$OUT/sig" "$OUT/home"
LOGS=$(cd "$OUT/tests" && pwd)
SIG=$(cd "$OUT/sig" && pwd)
HOMES=$(cd "$OUT/home" && pwd)
# run_test PROFILE PACKAGE NAME BACKEND EXE ARGS...: runs a test binary as cargo would, writing
# PROFILE; with a BACKEND, under a `$HOME` whose engine configuration file selects that reactor
# backend.
run_test() {
  local profile=$1 package=$2 name=$3 backend=$4 home=$HOME
  shift 4
  if [[ -n $backend ]]; then
    home=$HOMES/$backend
    mkdir -p "$home/.config/engine"
    echo "reactor-backend = $backend" >"$home/.config/engine/engine.conf"
  fi
  # Nothing on its standard input: this script's loops read theirs.
  (cd "$package" && env "HOME=$home" "LLVM_PROFILE_FILE=$profile" "CARGO_MANIFEST_DIR=$package" \
    "CARGO_PKG_NAME=$name" "$@" </dev/null)
}
# command_of EXE PACKAGE NAME HARNESS TARGET: the command that runs a test binary, one word per line:
# a libtest binary quietly (the engine's one test case at a time), the engine's programs that a
# Python driver of its tests/data starts through it, any other program on its own.
command_of() {
  local exe=$1 package=$2 name=$3 harness=$4 target=$5
  if [[ $harness == 1 ]]; then
    printf '%s\n' "$exe" -q
    if [[ $name == "$ENGINE" ]]; then printf '%s\n' --test-threads 1; fi
    return
  fi
  case $name:$target in
    "$ENGINE":metrics_tester)
      printf '%s\n' python3 "$package/tests/data/prometheus_test.py" --exporter "$exe" \
        --config "$package/tests/data/conf-example.yaml" ;;
    "$ENGINE":rest_api_httpd)
      printf '%s\n' python3 "$package/tests/data/json2code_test.py" --rest-api-httpd "$exe" ;;
    *) printf '%s\n' "$exe" ;;
  esac
}
# backends_of NAME: the reactor backends a package's tests run on, one per line (an empty line:
# the default backend, without a configuration file).
backends_of() {
  local list
  if [[ $1 == "$ENGINE" ]]; then
    read -r -a list <<<"$ENGINE_BACKENDS"
    printf '%s\n' "${list[@]}"
  else
    echo
  fi
}
# signature FILE: the binary signature in the name of a profile (`[<pid>-]<signature>_<n>.profraw`).
signature() {
  local b
  b=$(basename "$1")
  b=${b#*-}
  echo "${b%%_*}"
}
tests=()
harnesses=()
failures=()
n=0
while IFS=$'\t' read -r exe package name harness target; do
  n=$((n + 1))
  tests+=("$exe")
  harnesses+=("$harness")
  mkdir -p "$PROF/$n" "$SIG/$n"
  if [[ $harness == 1 ]]; then
    run_test "$SIG/$n/%m.profraw" "$package" "$name" "" "$exe" --list >/dev/null 2>&1 || true
  fi
  cmd=()
  while IFS= read -r word; do cmd+=("$word"); done < <(command_of "$exe" "$package" "$name" "$harness" "$target")
  log=$LOGS/$(basename "$exe").txt
  : >"$log"
  ok=1
  while IFS= read -r backend; do
    if [[ -n $backend ]]; then echo "== reactor backend $backend" >>"$log"; fi
    run_test "$PROF/$n/%p-%m.profraw" "$package" "$name" "$backend" "${cmd[@]}" >>"$log" 2>&1 || ok=0
  done < <(backends_of "$name")
  if [[ $ok == 0 ]]; then failures+=("$exe"); fi
done <"$OUT/tests.txt"

# The profile of each test binary, from its own profiles; and one of the programs, from the
# profiles of every signature that is not a test binary's.
own=$OUT/own.txt
: >"$own"
for ((i = 1; i <= n; i++)); do
  for f in "$SIG/$i"/*.profraw; do if [[ -e $f ]]; then signature "$f" >>"$own"; fi; done
done
programs=()
for ((i = 1; i <= n; i++)); do
  mine=()
  sig=""
  for f in "$SIG/$i"/*.profraw; do if [[ -e $f ]]; then sig=$(signature "$f"); fi; done
  for f in "$PROF/$i"/*.profraw; do
    [[ -e $f ]] || continue
    s=$(signature "$f")
    if [[ ${harnesses[$((i - 1))]} == 0 || $s == "$sig" ]]; then
      mine+=("$f")
    elif ! grep -qx "$s" "$own"; then
      programs+=("$f")
    fi
  done
  if [[ ${#mine[@]} -gt 0 ]]; then
    "$BIN/llvm-profdata" merge -sparse "${mine[@]}" -o "$PROF/$i.profdata"
  fi
done
if [[ ${#programs[@]} -gt 0 ]]; then
  "$BIN/llvm-profdata" merge -sparse "${programs[@]}" -o "$PROF/programs.profdata"
fi

# Tests and examples are diagnostic code, not product code.
IGNORE='(/\.cargo/|/rustc/|/tests/|/examples/|/target/)'
# One export per binary, merged below by position in the source: given several binaries, llvm-cov
# keeps only the first record of each function. Only the sources of the measured crates are
# exported.
sources=()
for c in "${crates[@]}"; do
  for d in crates/"$c"/src tools/"$c"/src; do [[ -d $d ]] && sources+=("$(cd "$d" && pwd -P)"); done
done
mkdir -p "$OUT/export"
# export N EXE PROFILE: the coverage of EXE read with PROFILE, to export/N.json.
export_one() {
  # A test file of another platform (`#![cfg(target_os = ...)]`) builds an empty binary with
  # nothing to measure: it is left out. Any other failure stops the run.
  if ! "$BIN/llvm-cov" export "$2" -instr-profile="$3" -ignore-filename-regex="$IGNORE" \
    ${sources[@]+"${sources[@]}"} >"$OUT/export/$1.json" 2>"$OUT/export/$1.err"; then
    grep -q "no coverage data found" "$OUT/export/$1.err" || { cat "$OUT/export/$1.err" >&2; exit 1; }
    rm "$OUT/export/$1.json"
  fi
  rm "$OUT/export/$1.err"
}
for ((i = 1; i <= n; i++)); do
  # A binary without a profile of its own (one that died before writing it) has nothing to add.
  if [[ -e $PROF/$i.profdata ]]; then
    export_one "$i" "${tests[$((i - 1))]}" "$PROF/$i.profdata"
  fi
done
p=0
while IFS= read -r exe; do
  p=$((p + 1))
  if [[ -e $PROF/programs.profdata ]]; then export_one "p$p" "$exe" "$PROF/programs.profdata"; fi
done <"$OUT/bins.txt"

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
    """How many times each line of a file ran, from the file's segments, which hold every instantiation of
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
            ran[line] = count
        wrapped = starts[-1]
        # The lines up to the next segment are inside the last one of this line.
        nxt = segments[i][0] if i < len(segments) else line + 1
        if wrapped[3]:
            for between in range(line + 1, nxt):
                ran[between] = wrapped[2]
    return ran


stats = {c: {"lines": {}, "functions": {}, "branches": {}} for c in crates}
for data in exports:
    for f in data["files"]:
        c = crate_of(f["filename"])
        if c is None:
            continue
        # Each binary was read with its own profile: the counts of a line, a branch side or a
        # function are the sums of those of its copies in every binary (the lib in its unit tests
        # and as a dependency of each test file, with different names and hashes).
        lines = stats[c]["lines"].setdefault(relative(f["filename"]), {})
        for line, count in line_counts(f["segments"]).items():
            lines[line] = lines.get(line, 0) + count
        seen = stats[c]["branches"]
        for b in f["branches"]:
            key = (relative(f["filename"]), b[0], b[1])
            taken, not_taken = seen.get(key, (0, 0))
            seen[key] = (taken + b[4], not_taken + b[5])

    # Functions by the position of their first region, merged the same way.
    for fn in data["functions"]:
        if not fn["regions"]:
            continue
        c = crate_of(fn["filenames"][0])
        if c is None:
            continue
        key = (relative(fn["filenames"][0]), fn["regions"][0][0], fn["regions"][0][1])
        funcs = stats[c]["functions"]
        funcs[key] = funcs.get(key, 0) + fn["count"]

# show.txt: each measured source once, `line|count|source`, with the summed counts (blank where
# the line holds no code).
with open(f"{out}/show.txt", "w") as show:
    for c in crates:
        for path, counts in sorted(stats[c]["lines"].items()):
            show.write(f"{path}:\n")
            try:
                text = open(root + path, encoding="utf-8", errors="replace").read().splitlines()
            except OSError:
                text = []
            for i in range(1, max([len(text), *counts]) + 1):
                count = counts.get(i)
                source = text[i - 1] if i <= len(text) else ""
                show.write(f"{i:>6}|{'' if count is None else count:>7}|{source}\n")
            show.write("\n")

for s in stats.values():
    s["lines"] = [
        sum(sum(n > 0 for n in ran.values()) for ran in s["lines"].values()),
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
        "functions": percent(sum(n > 0 for n in s["functions"].values()), len(s["functions"])),
        "branches": percent(
            sum(int(t > 0) + int(e > 0) for t, e in s["branches"].values()),
            2 * len(s["branches"]),
        ),
    }
    report.append(
        f"{c:<{width}}  lines {pct['lines']:6.2f} %  functions {pct['functions']:6.2f} %"
        f"  branches {pct['branches']:6.2f} %"
    )
    gaps = [f"{p}:{l}:{col}: function never run" for (p, l, col), run in sorted(s["functions"].items()) if not run]
    for (p, l, col), (t, e) in sorted(s["branches"].items()):
        if not (t and e):
            gaps.append(f"{p}:{l}:{col}: " + " ".join(x for x, n in (("true", t), ("false", e)) if not n))
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
