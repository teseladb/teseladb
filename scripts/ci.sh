#!/usr/bin/env bash
# The constitution's quality gates, in its order (001 FR-013). Stops at the first failure and
# prints the time of each gate.
#   scripts/ci.sh            every gate
#   scripts/ci.sh --quick    format, lints, dependencies and tests
#   TESELADB_BENCH=1 scripts/ci.sh   also the reference benchmark (gate 8)
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
  echo "ci: RUSTFLAGS contains teseladb_bug; faulty variants cannot reach CI" >&2
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

shell_scripts() {
  find scripts -name '*.sh' | sort
}

shellcheck_all() {
  local files=()
  while IFS= read -r f; do files+=("$f"); done < <(shell_scripts)
  docker run --rm -v "$PWD:/mnt" -w /mnt "$SHELLCHECK_IMAGE" -x "${files[@]}"
}

# Cargo.lock only holds workspace packages (001 FR-023): every step runs offline and without a registry.
gate "1. format" cargo fmt --all -- --check
gate "1. lints" cargo clippy --workspace --all-targets --offline -- -D warnings
gate "2. dependencies" scripts/check-deps.sh
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
gate "2. IANA time zone database 2024a (D-169)" scripts/fetch-pinned.sh scripts/tzdata.sha256
gate "2. time zone tables match their generator (D-169)" \
  cargo run -q --offline -p teseladb-tzgen -- .tools/tzdata/tzdata2024a.tar.gz --check
gate "3. tests" cargo test --workspace --offline -q
[[ $QUICK -eq 1 ]] && exit 0

gate "1. lints for x86_64 Linux" cargo clippy --workspace --all-targets --offline \
  --target x86_64-unknown-linux-musl -- -D warnings
gate "1. shell scripts" shellcheck_all
# Miri ignores opt-level and warns about it: its runs build the workspace without the optimization of
# the dev profile (Cargo.toml), which only speeds up the native tests.
MIRI_TEST=(cargo +nightly miri test --config profile.dev.opt-level=0
  --config profile.dev.package.teseladb-crypto.opt-level=0)
# The unsafe modules of D-151 (UNSAFE_MODULES of scripts/check-deps.sh), under Miri.
gate "1. Miri on the unsafe modules of D-151" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-lsm --lib -- skiplist:: shared:: memtable:: art::
# The vector kernels of the query engine (010 FR-030): NEON on the host, and the x86_64 ones
# interpreted, SSE2 by default and AVX2 with the feature on.
gate "1. Miri on the vector kernels (host)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-mql --lib -- simd::
gate "1. Miri on the vector kernels (x86_64, SSE2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-mql --lib --target x86_64-unknown-linux-gnu -- simd::
gate "1. Miri on the vector kernels (x86_64, AVX2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  RUSTFLAGS="-C target-feature=+avx2" \
  "${MIRI_TEST[@]}" -q -p teseladb-mql --lib --target x86_64-unknown-linux-gnu -- simd::
# The unpacking of the posting blocks in four lanes (011 FR-027), on the same three kernels.
gate "1. Miri on the posting lanes (host)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib -- segment::lanes segment::bitpack
gate "1. Miri on the posting lanes (x86_64, SSE2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib --target x86_64-unknown-linux-gnu -- segment::lanes segment::bitpack
gate "1. Miri on the posting lanes (x86_64, AVX2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  RUSTFLAGS="-C target-feature=+avx2" \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib --target x86_64-unknown-linux-gnu -- segment::lanes segment::bitpack
# The byte kernels of text analysis (011 FR-028), on the host and interpreted on x86_64.
gate "1. Miri on the byte kernels of search (host)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib -- byte_simd::
gate "1. Miri on the byte kernels of search (x86_64, SSE2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib --target x86_64-unknown-linux-gnu -- byte_simd::
gate "1. Miri on the byte kernels of search (x86_64, AVX2)" env MIRIFLAGS=-Zmiri-disable-isolation \
  RUSTFLAGS="-C target-feature=+avx2" \
  "${MIRI_TEST[@]}" -q -p teseladb-search --lib --target x86_64-unknown-linux-gnu -- byte_simd::
# The CPU paths of the generic crates (D-169): hashes, AES and carry-less multiplication, and the
# BSON search for the end of a name, on the host and interpreted for x86_64.
miri_arch() { # [target]
  local c
  for c in teseladb-hash teseladb-bson; do
    # Interpreted x86_64 has AVX2 only when the target asks for it, as the CPUs of the releases have.
    MIRIFLAGS=-Zmiri-disable-isolation RUSTFLAGS=${1:+-Ctarget-feature=+avx2} \
      "${MIRI_TEST[@]}" -q -p "$c" --lib ${1:+--target "$1"} -- arch:: || return 1
  done
  # crypto's arch module has no tests of its own: its accelerated paths are compared with the portable ones there.
  # Miri has no AArch64 crypto instructions (AES, SHA, CRC32), so those run only interpreted for x86_64.
  [[ -n ${1:-} ]] || return 0
  for c in teseladb-hash teseladb-crypto; do
    MIRIFLAGS=-Zmiri-disable-isolation "${MIRI_TEST[@]}" -q -p "$c" --test accel --target "$1" || return 1
  done
}
gate "1. Miri on the arch modules of hash, crypto and BSON (host)" miri_arch
gate "1. Miri on the arch modules of hash, crypto and BSON (x86_64)" miri_arch x86_64-unknown-linux-gnu
# The pool that carves blocks out of one mapping of huge pages (012 FR-032), on normal pages.
gate "1. Miri on the huge page pool" env MIRIFLAGS=-Zmiri-disable-isolation \
  "${MIRI_TEST[@]}" -q -p teseladb-sys --lib -- huge_pool::
gate "2. tests of check-deps" scripts/tests/check_deps_test.sh
gate "2. tests of fetch-pinned" scripts/tests/fetch_pinned_test.sh
gate "2. tests of the check of the release images" scripts/tests/check_image_test.sh
gate "2. tests of the Docker Hub and GitHub tokens of the releases" scripts/tests/release_tokens_test.sh
gate "2. tests of the GitHub release" scripts/tests/github_release_test.sh
gate "2. tests of the platform check of the install script" scripts/tests/install_platform_test.sh
gate "2. tests of final-phase" scripts/tests/final_phase_test.sh
gate "3. simulated crashes of teseladb-raftlog (2,000 seeds × 3 lives)" env TESELADB_RAFTLOG_CRASH_SEEDS=2000 \
  cargo test -q --offline --release -p teseladb-raftlog --test crash_sim
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
# The end-to-end battery with the real drivers and the chaos runs live in teseladb-compat (D-170).
gate "7. tests of tlc" scripts/tests/tlc_test.sh
long_gate "7. TLC (every model, writes spec/tla/RESULTS.md)" scripts/tlc.sh --all
if [[ ${TESELADB_BENCH:-0} == 1 ]]; then
  # 002 replaces it with `reference` and its baseline.
  gate "8. performance" cargo run -q --offline --release -p teseladb-bench -- --scenario raftlog
fi
echo "ci: all gates OK"
