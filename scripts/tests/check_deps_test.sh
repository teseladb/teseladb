#!/usr/bin/env bash
# Tests of scripts/check-deps.sh (001 FR-012) on throwaway repositories: each case builds the
# smallest tree that triggers one check and asserts the exit code and the message.
#   scripts/tests/check_deps_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/check-deps.sh
WORK=$(mktemp -d)
trap 'find "$WORK" -depth -delete' EXIT
# Built in two halves so that this file does not contain the word itself.
ORIGIN_WORD="raf""tree"
passed=0
failed=0

# A clean repository: a lock file without external packages, a logic crate, an I/O crate and the
# simulator, which may do I/O only in `main.rs` and `cli.rs`.
fixture() {
  local dir=$WORK/$1
  mkdir -p "$dir/scripts" "$dir/crates/teseladb-types/src" "$dir/crates/teseladb-io/src" \
    "$dir/tools/teseladb-sim/src"
  cp "$SCRIPT" "$dir/scripts/check-deps.sh"
  cat > "$dir/Cargo.lock" <<'EOF'
version = 4

[[package]]
name = "teseladb-types"
version = "0.1.0"
EOF
  printf '[package]\nname = "teseladb-types"\n' > "$dir/crates/teseladb-types/Cargo.toml"
  printf '#![forbid(unsafe_code)]\npub fn f() {}\n' > "$dir/crates/teseladb-types/src/lib.rs"
  printf '[package]\nname = "teseladb-io"\n' > "$dir/crates/teseladb-io/Cargo.toml"
  printf '#![forbid(unsafe_code)]\nuse std::fs;\nuse std::net::TcpStream;\nuse std::thread;\n' \
    > "$dir/crates/teseladb-io/src/lib.rs"
  printf '[package]\nname = "teseladb-sim"\n' > "$dir/tools/teseladb-sim/Cargo.toml"
  printf '#![forbid(unsafe_code)]\nfn main() { std::process::exit(0) }\n' > "$dir/tools/teseladb-sim/src/main.rs"
  printf '#![forbid(unsafe_code)]\npub fn run() {}\n' > "$dir/tools/teseladb-sim/src/lib.rs"
  printf 'Copied from %s.\n' "$ORIGIN_WORD" > "$dir/ORIGIN.md"
  echo "$dir"
}

# expect NAME DIR CODE PATTERN: runs the script in DIR and checks its exit code and output.
expect() {
  local name=$1 dir=$2 code=$3 pattern=$4 out rc=0
  out=$("$dir/scripts/check-deps.sh" 2>&1) || rc=$?
  if [[ $rc -eq $code ]] && grep -Eq -- "$pattern" <<<"$out"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAILED: $name (exit $rc, expected $code; /$pattern/ in output)"
    echo "  | ${out//$'\n'/$'\n'  | }"
  fi
}

d=$(fixture clean)
expect "clean repository" "$d" 0 "^check-deps: OK$"

d=$(fixture external)
cat >> "$d/Cargo.lock" <<'EOF'

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
EOF
expect "external package in Cargo.lock" "$d" 1 "external package: serde"

d=$(fixture git-source)
printf '\n[[package]]\nname = "foo"\nversion = "0.1.0"\nsource = "git+https://example.com/foo"\n' >> "$d/Cargo.lock"
expect "git package in Cargo.lock" "$d" 1 "external package: foo"

# The viuzi-* crates copied from TeselaDB come from the shared-crates next to the repository
# (001 FR-023, constitution 3.0.0, principle V).
viuzi() {
  local d=$1 path=$2
  printf '\n[[package]]\nname = "teseladb-bson"\nversion = "0.1.0"\n' >> "$d/Cargo.lock"
  printf '[workspace]\nmembers = ["crates/*"]\n\n[workspace.dependencies]\nteseladb-bson = { path = "%s" }\n' \
    "$path" > "$d/Cargo.toml"
}
d=$(fixture viuzi)
viuzi "$d" ../../shared-crates/crates/teseladb-bson
printf '\n[dependencies]\nteseladb-bson = { workspace = true }\n' >> "$d/crates/teseladb-types/Cargo.toml"
expect "a viuzi crate from shared-crates" "$d" 0 "^check-deps: OK$"

d=$(fixture viuzi-elsewhere)
viuzi "$d" ../teseladb-bson
expect "a viuzi crate from another path" "$d" 1 "Cargo.toml: teseladb-bson: path \.\./teseladb-bson, not \.\./\.\./shared-crates/crates/teseladb-bson"

d=$(fixture viuzi-own-path)
viuzi "$d" ../../shared-crates/crates/teseladb-bson
printf '\n[dependencies]\nteseladb-bson = { path = "../../../teseladb-bson" }\n' >> "$d/crates/teseladb-types/Cargo.toml"
expect "a member with its own path to a viuzi crate" "$d" 1 "crates/teseladb-types/Cargo.toml: teseladb-bson: not from the workspace's dependencies"

# The shared-crates checkout is at the commit of scripts/shared-crates.rev, with nothing uncommitted.
viuzi_repo() {
  local d=$1
  git init -q "$d/vrepo"
  printf 'x\n' > "$d/vrepo/f"
  git -C "$d/vrepo" add f
  git -C "$d/vrepo" -c user.name=t -c user.email=t@t commit -qm x
}
d=$(fixture viuzi-rev)
viuzi_repo "$d"
git -C "$d/vrepo" rev-parse HEAD > "$d/scripts/shared-crates.rev"
out=$(cd "$d" && TESELADB_VIUZI_REPO=vrepo scripts/check-deps.sh 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 0 ]]; then
  passed=$((passed + 1))
else
  failed=$((failed + 1)); echo "FAILED: shared-crates at its commit (exit $rc): $out"
fi
d=$(fixture viuzi-rev-other)
viuzi_repo "$d"
echo 0000000000000000000000000000000000000000 > "$d/scripts/shared-crates.rev"
out=$(cd "$d" && TESELADB_VIUZI_REPO=vrepo scripts/check-deps.sh 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "vrepo: at [0-9a-f]\{40\}, not 0000000000000000000000000000000000000000 (scripts/shared-crates.rev)" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1)); echo "FAILED: shared-crates at another commit (exit $rc): $out"
fi
d=$(fixture viuzi-rev-dirty)
viuzi_repo "$d"
git -C "$d/vrepo" rev-parse HEAD > "$d/scripts/shared-crates.rev"
printf 'y\n' >> "$d/vrepo/f"
out=$(cd "$d" && TESELADB_VIUZI_REPO=vrepo scripts/check-deps.sh 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -q "vrepo: uncommitted changes" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1)); echo "FAILED: shared-crates with uncommitted changes (exit $rc): $out"
fi

d=$(fixture outside)
printf '\n[[package]]\nname = "foo"\nversion = "0.1.0"\n' >> "$d/Cargo.lock"
expect "a path package outside the workspace" "$d" 1 "Cargo.lock: package outside the workspace: foo"

for api in "std::fs::read" "std::net::TcpListener" "std::thread::spawn" "std::time::Instant" \
  "std::time::SystemTime" "std::process::exit" "std::env::var" "std::io::stdin" "std::io::stdout"; do
  d=$(fixture "api-${api//:/_}")
  printf 'pub fn g() {\n    let _ = %s;\n}\n' "$api" >> "$d/crates/teseladb-types/src/lib.rs"
  expect "$api in a logic crate" "$d" 1 "crates/teseladb-types/src/lib.rs:4: .*${api}"
done

d=$(fixture grouped-import)
printf 'use std::{\n    collections::HashMap,\n    fs,\n};\n' >> "$d/crates/teseladb-types/src/lib.rs"
expect "grouped import of std::fs" "$d" 1 "crates/teseladb-types/src/lib.rs:5: +fs,"

d=$(fixture grouped-io)
printf 'use std::io::{Read, stdout};\n' >> "$d/crates/teseladb-types/src/lib.rs"
expect "grouped import of stdout" "$d" 1 "crates/teseladb-types/src/lib.rs:3: .*stdout"

d=$(fixture io-crate-is-free)
printf 'use std::env;\nuse std::process;\n' >> "$d/crates/teseladb-io/src/lib.rs"
expect "I/O allowed in an I/O crate" "$d" 0 "^check-deps: OK$"

d=$(fixture depends-on-io)
printf '[dependencies]\nteseladb-io = { path = "../teseladb-io" }\n' >> "$d/crates/teseladb-types/Cargo.toml"
expect "logic crate depending on an I/O crate" "$d" 1 "teseladb-types depends on the I/O crate teseladb-io"

d=$(fixture sim-lib-io)
printf 'pub fn h() { let _ = std::fs::read("x"); }\n' >> "$d/tools/teseladb-sim/src/lib.rs"
expect "I/O in the simulator outside main.rs and cli.rs" "$d" 1 "tools/teseladb-sim/src/lib.rs:3: "

d=$(fixture origin-word)
printf '# %s notes\n' "$ORIGIN_WORD" > "$d/README.md"
expect "origin word outside ORIGIN.md" "$d" 1 "README.md:1: "

d=$(fixture origin-word-case)
mkdir -p "$d/docs"
printf 'see %s\n' "$(tr '[:lower:]' '[:upper:]' <<<"$ORIGIN_WORD")" > "$d/docs/notes.md"
expect "origin word in capitals" "$d" 1 "docs/notes.md:1: "

d=$(fixture origin-word-in-target)
mkdir -p "$d/target/debug"
printf '%s\n' "$ORIGIN_WORD" > "$d/target/debug/build.log"
expect "build output is not searched" "$d" 0 "^check-deps: OK$"

d=$(fixture git-in-engine)
mkdir -p "$d/crates/teseladb-multiraft/src"
printf '[package]\nname = "teseladb-multiraft"\n' > "$d/crates/teseladb-multiraft/Cargo.toml"
printf 'pub struct Core;\n\npub fn f(o: Oid) {}\n' > "$d/crates/teseladb-multiraft/src/lib.rs"
expect "Git type in the Multi-Raft engine" "$d" 1 "crates/teseladb-multiraft/src/lib.rs:3: .*Oid"

d=$(fixture testkv-linked)
printf '[dependencies]\nteseladb-testkv = { path = "../teseladb-testkv" }\n' >> "$d/crates/teseladb-types/Cargo.toml"
expect "test state machine in [dependencies]" "$d" 1 "teseladb-types depends on teseladb-testkv"

d=$(fixture testkv-dev)
printf '[dev-dependencies]\nteseladb-testkv = { path = "../teseladb-testkv" }\n' >> "$d/crates/teseladb-types/Cargo.toml"
expect "test state machine in [dev-dependencies]" "$d" 0 "^check-deps: OK$"

d=$(fixture unsafe-binary)
printf 'fn main() {}\n' > "$d/tools/teseladb-sim/src/main.rs"
expect "a binary that does not forbid unsafe code" "$d" 1 \
  "tools/teseladb-sim/src/main.rs: no #!\[forbid\(unsafe_code\)\]"

d=$(fixture unsafe-sys)
mkdir -p "$d/crates/teseladb-sys/src"
printf '[package]\nname = "teseladb-sys"\n' > "$d/crates/teseladb-sys/Cargo.toml"
printf '#![deny(unsafe_code)]\npub fn f() {}\n' > "$d/crates/teseladb-sys/src/lib.rs"
expect "teseladb-sys may use unsafe code" "$d" 0 "^check-deps: OK$"

d=$(fixture several)
printf 'pub fn g() {\n    let _ = std::env::var("A");\n    let _ = std::fs::read("b");\n}\n' \
  >> "$d/crates/teseladb-types/src/lib.rs"
printf '%s\n' "$ORIGIN_WORD" > "$d/NOTES"
out=$("$d/scripts/check-deps.sh" 2>&1 || true)
if [[ $(grep -c "^check-deps: " <<<"$out") -eq 3 ]]; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: one message per finding"
  echo "  | ${out//$'\n'/$'\n'  | }"
fi

# No lock shared between threads in the crates of the hot path (012 FR-008): a `Mutex`, an
# `RwLock` or a `Condvar` there is a finding; in another crate, or only in a comment, it is not.
for lock in "std::sync::Mutex" "std::sync::{Arc, RwLock}" "std::sync::Condvar"; do
  d=$(fixture "lock-$(tr -c 'a-zA-Z' '-' <<<"$lock")")
  printf 'use %s;\n' "$lock" >> "$d/crates/teseladb-io/src/lib.rs"
  expect "$lock in a hot-path crate" "$d" 1 "crates/teseladb-io/src/lib.rs:[0-9]+: .*\\(a lock in a hot-path crate\\)"
done
# Cryptography runs on every authenticated or encrypted operation (D-090: a generator per thread).
# teseladb-crypto lives in shared-crates since 05-11-2026 (001 FR-023).
d=$(fixture lock-crypto)
mkdir -p "$d/viuzi/teseladb-crypto/src"
printf 'use std::sync::Mutex;\n' > "$d/viuzi/teseladb-crypto/src/lib.rs"
out=$(cd "$d" && TESELADB_ANOTHERPRODUCT=viuzi scripts/check-deps.sh 2>&1) && rc=0 || rc=$?
if [[ $rc -eq 1 ]] && grep -Eq "viuzi/teseladb-crypto/src/lib.rs:[0-9]+: .*\\(a lock in a hot-path crate\\)" <<<"$out"; then
  passed=$((passed + 1))
else
  failed=$((failed + 1))
  echo "FAILED: a lock in teseladb-crypto (exit $rc)"
  echo "  | ${out//$'\n'/$'\n'  | }"
fi
d=$(fixture lock-elsewhere)
printf 'use std::sync::Mutex;\n' >> "$d/crates/teseladb-types/src/lib.rs"
printf '// A Mutex would hold up the core.\n' >> "$d/crates/teseladb-io/src/lib.rs"
expect "a lock outside the hot path, or in a comment" "$d" 0 "^check-deps: OK$"

echo "check_deps_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
