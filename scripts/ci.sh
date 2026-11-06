#!/usr/bin/env bash
# The constitution's quality gates, in its order (001 FR-013). Stops at the first failure and
# prints the time of each gate.
#   scripts/ci.sh            every gate
#   scripts/ci.sh --quick    format, lints, dependencies and tests
#   TESELADB_BENCH=1 scripts/ci.sh   also the reference benchmark (gate 8)
#   TESELADB_E2E=0 scripts/ci.sh     every gate but the end-to-end ones (gate 6). The owner decided on
#     30-10-2026 that the end-to-end suites run once, when every task of every specification is done.
#   TESELADB_CI_LONG=0 scripts/ci.sh  every gate but the long runs of coverage, faulty variants and TLC,
#     which scripts/final-phase.sh runs as phases of their own (coverage crate by crate)
# Bash 3.2 (macOS) or later. The full run needs Docker (x86_64 tests and shellcheck).
set -euo pipefail
cd "$(dirname "$0")/.."

QUICK=0
case ${1:-} in
  "") ;;
  --quick) QUICK=1 ;;
  *)
    echo "usage: scripts/ci.sh [--quick]" >&2
    exit 2
    ;;
esac

if [[ "${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}" == *_bug=* ]]; then
  echo "ci: RUSTFLAGS contains teseladb_bug or teseladb_bug; faulty variants cannot reach CI" >&2
  exit 1
fi

SHELLCHECK_IMAGE=koalaman/shellcheck:v0.10.0

gate() {
  local name=$1 start=$SECONDS
  shift
  echo "== gate: $name"
  "$@"
  echo "== $name: OK ($((SECONDS - start)) s)"
}

# A gate whose tool is not in the repository yet: it says so instead of passing silently.
absent() {
  echo "== gate: $1: nothing to run yet ($2)"
}

shell_scripts() {
  {
    find scripts -name '*.sh'
    # The compatibility and chaos harnesses; not the packages their runners install.
    find tests -name '*.sh' -not -path '*/node_modules/*'
  } | sort
}

shellcheck_all() {
  local files=()
  while IFS= read -r f; do files+=("$f"); done < <(shell_scripts)
  docker run --rm -v "$PWD:/mnt" -w /mnt "$SHELLCHECK_IMAGE" -x "${files[@]}"
}

# The viuzi-* crates come from the private registry at the versions of Cargo.lock; every later step runs offline.
gate "0. viuzi-* crates of Cargo.lock from the registry" cargo fetch --locked
gate "1. format" cargo fmt --all -- --check
gate "1. lints" cargo clippy --workspace --all-targets --offline -- -D warnings
gate "2. dependencies" scripts/check-deps.sh
gate "2. origin register" scripts/check-origin.sh
gate "2. Unicode 8.0 data (D-043)" scripts/fetch-pinned.sh scripts/unicode-ucd.sha256
gate "2. CLDR 29 data (D-043)" scripts/fetch-pinned.sh scripts/unicode-cldr.sha256
gate "2. ICU 57.1 collation data (D-043)" scripts/fetch-pinned.sh scripts/icu-coll.sha256
gate "2. Unicode 16.0 data of the regex engine (D-049)" scripts/fetch-pinned.sh crates/teseladb-mql/unicode/ucd.sha256
gate "2. PCRE2 10.43 test data (006 T058)" scripts/fetch-pinned.sh crates/teseladb-mql/tests/pcre2.sha256
gate "2. test262 subset of the JavaScript engine (016 T002)" scripts/fetch-pinned.sh crates/teseladb-js/tests/test262.sha256
gate "2. Unicode 15.1 data of the search analyzers (011)" scripts/fetch-pinned.sh scripts/unicode-search.sha256
gate "2. Unicode 12.1 data of the standard tokenizer (011)" scripts/fetch-pinned.sh scripts/unicode-tokenizer.sha256
gate "2. emoji 12.1 data of the standard tokenizer (011)" scripts/fetch-pinned.sh scripts/unicode-emoji-tokenizer.sha256
gate "2. morfologik dictionaries of the search analyzers (011)" scripts/fetch-pinned.sh scripts/morfologik.sha256
gate "2. collation tables match their generator (D-043)" cargo run -q --offline -p teseladb-unicodegen -- --check
gate "3. tests" cargo test --workspace --offline -q
[[ $QUICK -eq 1 ]] && exit 0

gate "1. lints for x86_64 Linux" cargo clippy --workspace --all-targets --offline \
  --target x86_64-unknown-linux-musl -- -D warnings
gate "1. shell scripts" shellcheck_all
gate "2. tests of check-deps" scripts/tests/check_deps_test.sh
gate "2. tests of check-origin" scripts/tests/check_origin_test.sh
gate "2. tests of fetch-pinned" scripts/tests/fetch_pinned_test.sh
gate "2. tests of the battery's requires" scripts/tests/compat_requires_test.sh
gate "2. tests of the battery's secrets scan" scripts/tests/compat_secrets_test.sh
gate "2. tests of the Docker Hub token of the releases" scripts/tests/dockerhub_token_test.sh
gate "2. tests of the battery's publication of the enrollment tokens" scripts/tests/compat_publish_test.sh
gate "2. tests of final-phase" scripts/tests/final_phase_test.sh
# The published source of teseladb-raftlog, the one TeselaDB is built with (scripts/teseladb-src.sh).
gate "3. simulated crashes of teseladb-raftlog (2,000 seeds × 3 lives)" env VIUZI_CRASH_SEEDS=2000 CARGO_TARGET_DIR=target/viuzi \
  cargo test -q --offline --release --manifest-path "$(scripts/teseladb-src.sh teseladb-raftlog)/Cargo.toml" --test crash_sim
gate "3. LSM crashes (10,000 seeds × 3 lives, 004 SC-001)" env TESELADB_LSM_CRASH_SEEDS=10000 \
  cargo test -q --offline --release -p teseladb-lsm --test crash
gate "3. real crashes with SIGKILL (50 kills)" env TESELADB_KILL_ROUNDS=50 \
  cargo test -q --offline --release -p teseladb-io --test crash_real
gate "3. real crashes of log and LSM with SIGKILL (50 kills, 004)" env TESELADB_KILL_ROUNDS=50 \
  cargo test -q --offline --release -p teseladb-io --test crash_real_lsm
gate "3. tests on x86_64 Linux" scripts/test-x86_64.sh
gate "3. tests on the Linux kernel, io_uring included (012)" scripts/test-linux.sh
gate "3. TLS interoperability with openssl and curl (013 FR-019)" scripts/tls-interop.sh
gate "4. tests of coverage" scripts/tests/coverage_test.sh
long_gate() {
  if [[ ${TESELADB_CI_LONG:-1} == 0 ]]; then
    echo "== gate: $1: run by scripts/final-phase.sh (TESELADB_CI_LONG=0)"
  else
    gate "$@"
  fi
}
long_gate "4. coverage (95 % per crate)" env TESELADB_COV_MIN=95 scripts/coverage.sh

gate "5. tests of sim-mutants" scripts/tests/sim_mutants_test.sh
long_gate "5. faulty variants, each caught by its checker (002 SC-002)" scripts/sim-mutants.sh
gate "5. simulator (200 seeds of every workload)" cargo run -q --offline --release -p teseladb-sim -- \
  --seeds 200 --workload all --jobs "$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
if [[ ${TESELADB_E2E:-1} == 0 ]]; then
  echo "== gate: 6. end-to-end: deferred until every task is done (TESELADB_E2E=0, owner 30-10-2026)"
elif [[ -x tests/compat/run.sh ]]; then
  gate "6. compatibility with the real drivers" tests/compat/run.sh
  gate "6. the database tools in both directions (006 FR-025)" tests/compat/tools/run.sh
else
  absent "6. compatibility" "tests/compat arrives with specification 002"
fi
gate "7. tests of tlc" scripts/tests/tlc_test.sh
long_gate "7. TLC (every model, writes spec/tla/RESULTS.md)" scripts/tlc.sh --all
if [[ ${TESELADB_BENCH:-0} == 1 ]]; then
  # 002 replaces it with `reference` and its baseline.
  gate "8. performance" cargo run -q --offline --release -p teseladb-bench -- --scenario raftlog
fi
echo "ci: all gates OK"
