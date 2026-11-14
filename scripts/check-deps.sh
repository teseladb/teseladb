#!/usr/bin/env bash
# Gate of the constitution's principles II and V (001 FR-012), one message per finding:
#  - Cargo.lock only holds workspace packages (no registry, the private one included, and no git), and
#    no manifest takes a dependency by a path outside the repository: the generic crates are members
#    in crates/teseladb-* (001 FR-023, D-169, constitution 3.4.0, principle V; owner, 14-11-2026);
#  - logic crates do not depend on the I/O crates nor use the network, files, threads, the system
#    clock, the process, the environment or the standard input and output;
#  - the Multi-Raft engine has no Git types and no crate links the test state machine;
#  - every crate root forbids unsafe code, except the crates that talk to the system or to the CPU
#    (FR-001);
#  - the crates of the hot path share no lock between threads: no `Mutex`, `RwLock` or `Condvar`
#    (012 FR-008);
#  - the origin project of the copied code is only named in ORIGIN.md.
#   scripts/check-deps.sh
# Bash 3.2 (macOS) or later.
set -euo pipefail
cd "$(dirname "$0")/.."

# Crates whose job is I/O. Every other crate of `crates/` is a logic crate.
IO_CRATES=(teseladb-io teseladb-sys teseladb-server)
# Tools: the benchmark measures the real system, the Unicode and time zone generators read the
# official files and write their tables, and the battery's comparator reads the runners' output files
# (constitution, principle II); the simulator is logic except these files (artifacts, histories
# and child processes).
IO_TOOLS=(teseladb-bench teseladb-unicodegen teseladb-tzgen compat-compare)
SIM_IO_FILES=(tools/teseladb-sim/src/main.rs tools/teseladb-sim/src/cli.rs)
# Crates whose code runs on a core's thread for each operation (012 FR-008): no lock shared between
# threads there.
# Cryptography runs on every authenticated or encrypted operation (D-090), and Raft and its log on
# every write.
HOT_CRATES=(teseladb-sys teseladb-io teseladb-devlog teseladb-multiraft teseladb-lsm teseladb-range teseladb-hlc
  teseladb-txn teseladb-index teseladb-meta teseladb-search teseladb-crypto teseladb-raft teseladb-raftlog)
# Crates that may use unsafe code: system calls and CPU intrinsics.
UNSAFE_CRATES=(teseladb-sys)
# Modules that D-151 admits with unsafe code for a measured performance gain: their crate denies
# it and only these files allow it; Miri runs their tests in the gates (scripts/ci.sh).
UNSAFE_MODULES=(crates/teseladb-lsm/src/skiplist.rs crates/teseladb-lsm/src/shared.rs
  crates/teseladb-lsm/src/art.rs
  crates/teseladb-mql/src/simd.rs
  crates/teseladb-hash/src/arch/mod.rs crates/teseladb-crypto/src/arch/mod.rs crates/teseladb-bson/src/arch/mod.rs)
# Built in two halves so that this script does not contain the word itself.
ORIGIN_WORD="raf""tree"

fail=0
finding() {
  echo "check-deps: $*" >&2
  fail=1
}

# Turns grep's `file:line:text` lines on stdin into findings `file:line: text (why)`.
grep_findings() {
  local why=$1 hit file line
  while IFS= read -r hit; do
    hit=${hit#./}
    file=${hit%%:*}
    line=${hit#*:}
    finding "$file:${line%%:*}: ${line#*:} ($why)"
  done
}

contains() {
  local needle=$1
  shift
  for x in "$@"; do [[ $x == "$needle" ]] && return 0; done
  return 1
}

check_lock() {
  [[ -f Cargo.lock ]] || cargo metadata --offline --format-version 1 >/dev/null
  while IFS= read -r name; do
    finding "Cargo.lock: external package: $name"
  done < <(awk '
    /^\[\[package\]\]/ { name = "" }
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^source = / { print name }
  ' Cargo.lock)
  local members=() m name
  for m in crates/*/Cargo.toml tools/*/Cargo.toml; do
    [[ -f $m ]] && members+=("$(awk -F'"' '/^name = / { print $2; exit }' "$m")")
  done
  while IFS= read -r name; do
    contains "$name" "${members[@]}" ||
      finding "Cargo.lock: package outside the workspace: $name"
  done < <(awk '
    /^\[\[package\]\]/ { if (name != "" && !src) print name; name = ""; src = 0 }
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^source = / { src = 1 }
    END { if (name != "" && !src) print name }
  ' Cargo.lock)
  check_paths
}

# Prints the path `$1/$2` relative to the repository with its `.` and `..` resolved in the text, so
# a path that leaves the repository starts with `..`.
normalize() {
  awk -v p="$1/$2" 'BEGIN {
    n = split(p, part, "/"); k = 0
    for (i = 1; i <= n; i++) {
      if (part[i] == "" || part[i] == ".") continue
      if (part[i] == ".." && k > 0 && out[k] != "..") { k--; continue }
      out[++k] = part[i]
    }
    s = ""; for (i = 1; i <= k; i++) s = s (i > 1 ? "/" : "") out[i]
    print s
  }'
}

# Every dependency by path, in the workspace's manifest or in a member's, points inside the repository.
check_paths() {
  local m name path
  for m in Cargo.toml crates/*/Cargo.toml tools/*/Cargo.toml; do
    [[ -f $m ]] || continue
    while IFS=$'\t' read -r name path; do
      [[ $path != /* && $(normalize "$(dirname "$m")" "$path") != ..* ]] ||
        finding "$m: $name: path outside the workspace ($path)"
    done < <(sed -nE 's/^([A-Za-z0-9_-]+) = \{.*path = "([^"]+)".*/\1\t\2/p' "$m")
  done
}

# Prints `file:line: text` for every forbidden use in the given sources: paths written out in full
# and names brought in by grouped imports (`use std::{fs, io::{Read, stdout}};`).
forbidden_io() {
  [[ $# -gt 0 ]] || return 0
  awk '
    function report(line, text) { printf "%s:%d: %s\n", FILENAME, line, text }
    function count(s, ch,   t) { t = s; return gsub(ch, "", t) }
    FNR == 1 { depth = 0; pending = 0; skipping = 0 }
    # The item after `#[cfg(test)]` is test code, which may run threads and time the way tests
    # in tests/ do; principle II is about the logic.
    {
      plain = $0
      sub(/\/\/.*/, "", plain)
      if (skipping) {
        tdepth += count(plain, "\\{") - count(plain, "\\}")
        if (tdepth <= 0) skipping = 0
        next
      }
      if (plain ~ /^[[:space:]]*#\[cfg\(test\)\]/) { pending = 1; next }
      if (pending) {
        if (count(plain, "\\{") > 0) {
          tdepth = count(plain, "\\{") - count(plain, "\\}")
          pending = 0
          if (tdepth > 0) skipping = 1
          next
        }
        if (plain ~ /;/) pending = 0
        next
      }
    }
    {
      code = $0
      sub(/\/\/.*/, "", code)
      if (code ~ /std::(net|fs|thread|process|env)([^[:alnum:]_]|$)/ ||
          code ~ /std::time::(Instant|SystemTime)/ ||
          code ~ /(^|[^[:alnum:]_])(Instant|SystemTime)::now/ ||
          code ~ /std::io::(stdin|stdout)([^[:alnum:]_]|$)/) {
        report(FNR, $0)
        next
      }
      if (depth == 0 && code ~ /use[[:space:]]+std::.*\{/) {
        depth = 0
        start = 1
      }
      if (start || depth > 0) {
        if (code ~ /(^|[{,[:space:]])(net|fs|thread|process|env)([,}[:space:]]|::|$)/ ||
            code ~ /(^|[^[:alnum:]_])(Instant|SystemTime|stdin|stdout)([^[:alnum:]_]|$)/) {
          report(FNR, $0)
        }
        opened = gsub(/\{/, "{", code)
        closed = gsub(/\}/, "}", code)
        depth += opened - closed
        start = 0
      }
    }
  ' "$@"
}

rust_sources() {
  [[ -d $1 ]] || return 0
  find "$1" -name '*.rs' | sort
}

depends_on_io() {
  local name=$1 manifest=$2
  for io in "${IO_CRATES[@]}"; do
    if grep -Eq "^[[:space:]]*${io}[[:space:]]*=" "$manifest"; then
      finding "$name depends on the I/O crate $io"
    fi
  done
}

check_logic_crates() {
  local dir name file sources hit
  for dir in crates/*/; do
    [[ -d $dir ]] || continue
    name=$(basename "$dir")
    contains "$name" "${IO_CRATES[@]}" && continue
    [[ -f $dir/Cargo.toml ]] && depends_on_io "$name" "$dir/Cargo.toml"
    sources=()
    while IFS= read -r file; do sources+=("$file"); done < <(rust_sources "${dir%/}/src")
    while IFS= read -r hit; do
      finding "$hit (forbidden I/O in a logic crate)"
    done < <(forbidden_io ${sources[@]+"${sources[@]}"})
  done
}

check_tools() {
  local dir name file sources=() hit
  for dir in tools/*/; do
    [[ -d $dir ]] || continue
    name=$(basename "$dir")
    contains "$name" "${IO_TOOLS[@]}" && continue
    [[ -f $dir/Cargo.toml ]] && depends_on_io "$name" "$dir/Cargo.toml"
    sources=()
    while IFS= read -r file; do
      contains "$file" "${SIM_IO_FILES[@]}" || sources+=("$file")
    done < <(rust_sources "${dir%/}/src")
    while IFS= read -r hit; do
      finding "$hit (forbidden I/O in a logic tool)"
    done < <(forbidden_io ${sources[@]+"${sources[@]}"})
  done
}

# The Multi-Raft engine knows nothing of Git (001 FR-005): none of the origin's Git types or
# mechanisms appear in its sources.
check_engine_has_no_git() {
  local dir=crates/teseladb-multiraft/src
  [[ -d $dir ]] || return 0
  grep_findings "Git type or mechanism in the Multi-Raft engine" < <(
    grep -RnwE 'Oid|ObjectType|RepoId|PartitionId|RefValue|anchor|fork' "$dir" || true
  )
}

# The test state machine is never linked into the binary (001 FR-008): only tools and dev
# dependencies may use it.
check_testkv_not_linked() {
  local manifest name
  for manifest in crates/*/Cargo.toml; do
    [[ -f $manifest ]] || continue
    name=$(basename "$(dirname "$manifest")")
    if awk '
      /^\[/ { section = $0 }
      section ~ /^\[(target\..*\.)?dependencies\]$/ && /^[[:space:]]*teseladb-testkv[[:space:]]*=/ { found = 1 }
      END { exit !found }
    ' "$manifest"; then
      finding "$name depends on teseladb-testkv (only dev-dependencies and tools may)"
    fi
  done
}

check_unsafe_forbidden() {
  local root crate
  for root in crates/*/src/lib.rs crates/*/src/main.rs tools/*/src/lib.rs tools/*/src/main.rs; do
    [[ -f $root ]] || continue
    crate=$(basename "$(dirname "$(dirname "$root")")")
    contains "$crate" "${UNSAFE_CRATES[@]}" && continue
    if grep -q "^crates/$crate/" < <(printf '%s\n' "${UNSAFE_MODULES[@]}"); then
      grep -q '^#!\[deny(unsafe_code)\]' "$root" || finding "$root: no #![deny(unsafe_code)]"
    else
      grep -q '^#!\[forbid(unsafe_code)\]' "$root" || finding "$root: no #![forbid(unsafe_code)]"
    fi
  done
  # The permission only in the admitted modules.
  local f
  while IFS= read -r f; do
    contains "$f" "${UNSAFE_MODULES[@]}" || finding "$f: allows unsafe code outside the modules of D-151"
  done < <(grep -rl '^#!\[allow(unsafe_code)\]' crates/*/src tools/*/src 2>/dev/null | grep -v '^crates/teseladb-sys/' | sort)
}

check_hot_path_locks() {
  local crate files=()
  for crate in "${HOT_CRATES[@]}"; do
    [[ -d crates/$crate/src ]] || continue
    while IFS= read -r f; do files+=("$f"); done < <(find "crates/$crate/src" -name '*.rs' | sort)
  done
  [[ ${#files[@]} -gt 0 ]] || return 0
  # The code of each line without its comment.
  grep_findings "a lock in a hot-path crate" < <(awk '
    { code = $0; sub(/\/\/.*/, "", code) }
    code ~ /(^|[^[:alnum:]_])(Mutex|RwLock|Condvar)([^[:alnum:]_]|$)/ {
      printf "%s:%d: %s\n", FILENAME, FNR, $0
    }
  ' "${files[@]}")
}

check_origin_word() {
  grep_findings "the origin project is only named in ORIGIN.md" < <(grep -RIni --exclude-dir=target --exclude-dir=.git --exclude-dir=.tools \
    --exclude=ORIGIN.md -e "$ORIGIN_WORD" . || true)
}

check_lock
check_logic_crates
check_tools
check_engine_has_no_git
check_testkv_not_linked
check_unsafe_forbidden
check_hot_path_locks
check_origin_word

[[ $fail -eq 0 ]] || exit 1
echo "check-deps: OK"
