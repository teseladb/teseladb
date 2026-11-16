#!/usr/bin/env bash
# Runs inside anotherproduct-rs-bench:1.98.0. Usage: inner.sh MODE build|run [filter-regex]
#   MODE dev   : dev profile (debug assertions, overflow checks), default cfgs, stable 1.98
#   MODE dbg   : dev profile + the C++ Debug-mode cfg set (engine_debug, default allocator, shuffle,
#                debug shared_ptr/promise, stack guards, type-erase-more), stable 1.98
#   MODE asan  : nightly, -Zsanitizer=address -Zbuild-std, C++ Sanitize-mode cfg set
#   MODE tsan  : nightly, -Zsanitizer=thread -Zbuild-std, default allocator (does not start:
#                the process's dl_iterate_phdr override is instrumented and runs before the
#                sanitizer is initialized)
#   MODE rel   : release profile, default cfgs, stable 1.98
#
# The container mounts the target volume at /target (sources copied to /target/d2-sanitize-src,
# nightly installed with rustup under /target/d2-sanitize-rustup), this directory at /work and
# the bench's lib/ (cargo_executables.py) at /bench/lib. batch.sh runs the whole battery in
# batches that delete their binaries, to keep the target directory small; miri.sh runs the
# tests without FFI under Miri. lsan.supp lists the leaks that are not the engine's.
set -uo pipefail
MODE=$1
ACTION=$2
FILTER=${3:-.}
SRC=/target/d2-sanitize-src
OUT=/target/d2-sanitize-out/$MODE
export CARGO_TARGET_DIR=/target/d2-sanitize-target-$MODE
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-3}
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
# SEL: what to build (default every test target and the lib unit tests), e.g. SEL="--test smp_test --test thread_test"
read -r -a SELA <<<"${SEL:---tests --lib}"
TRIPLE=aarch64-unknown-linux-gnu
DEBUG_CFGS="--cfg engine_debug --cfg engine_default_allocator --cfg engine_shuffle_task_queue --cfg engine_debug_shared_ptr --cfg engine_debug_promise --cfg engine_thread_stack_guards"
EXTRA=()
TC=+1.98.0
case $MODE in
    dev) export RUSTFLAGS="" ;;
    rel) export RUSTFLAGS="" CARGO_PROFILE_RELEASE_DEBUG=line-tables-only; EXTRA=(--release) ;;
    dbg) export RUSTFLAGS="$DEBUG_CFGS --cfg engine_type_erase_more" ;;
    asan)
        export RUSTUP_HOME=/target/d2-sanitize-rustup CARGO_HOME=/target/d2-sanitize-cargo
        TC=+nightly-2026-08-27
        export RUSTFLAGS="-Zsanitizer=address --cfg engine_asan_enabled --cfg engine_have_asan_fiber_support $DEBUG_CFGS"
        export RUSTDOCFLAGS="$RUSTFLAGS"
        export ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=1:abort_on_error=1:alloc_dealloc_mismatch=0:allocator_may_return_null=1:external_symbolizer_path=/usr/bin/addr2line}
        export LSAN_OPTIONS=suppressions=/work/lsan.supp:print_suppressions=0
        EXTRA=(-Zbuild-std --target $TRIPLE)
        ;;
    tsan)
        export RUSTUP_HOME=/target/d2-sanitize-rustup CARGO_HOME=/target/d2-sanitize-cargo
        TC=+nightly-2026-08-27
        export RUSTFLAGS="-Zsanitizer=thread --cfg engine_default_allocator --cfg no_exception_intercept"
        export RUSTDOCFLAGS="$RUSTFLAGS"
        export TSAN_OPTIONS=${TSAN_OPTIONS:-halt_on_error=0:report_signal_unsafe=0:external_symbolizer_path=/usr/bin/addr2line}
        EXTRA=(-Zbuild-std --target $TRIPLE)
        ;;
    *) echo "bad mode"; exit 2 ;;
esac
CARGO="/usr/local/cargo/bin/cargo"
[ -n "${CARGO_HOME:-}" ] && [ "$MODE" = asan -o "$MODE" = tsan ] && CARGO="/usr/local/cargo/bin/cargo"
mkdir -p "$OUT/logs"
cd "$SRC"
if [ "$ACTION" = build ]; then
    OFF=--offline
    [ ${#EXTRA[@]} -gt 0 ] && { OFF=; export CARGO_NET_OFFLINE=false; }
    $CARGO $TC test $OFF --no-fail-fast --no-run "${SELA[@]}" "${EXTRA[@]}" --message-format=json >"$OUT/build.json" 2>"$OUT/build.err"
    rc=$?
    python3 /bench/lib/cargo_executables.py "$SRC/Cargo.toml" <"$OUT/build.json" >"$OUT/exes.tsv"
    tail -30 "$OUT/build.err"
    echo "build rc=$rc, $(wc -l <"$OUT/exes.tsv") executables"
    exit $rc
fi
echo 1048576 >/proc/sys/fs/aio-max-nr
: >"$OUT/tests.tsv"
while IFS=$'\t' read -r kind name path harness; do
    [ "$kind" = bench ] && continue
    echo "$name" | grep -Eq "$FILTER" || continue
    args=()
    [ "$harness" = 1 ] && args=(--test-threads 1)
    cmd=("$path" "${args[@]}")
    case "$name" in
        metrics_tester) cmd=(python3 "$SRC/tests/data/prometheus_test.py" --exporter "$path" --config "$SRC/tests/data/conf-example.yaml") ;;
        rest_api_httpd) cmd=(python3 "$SRC/tests/data/json2code_test.py" --rest-api-httpd "$path") ;;
    esac
    rc=0
    start=$(date +%s)
    (cd "$SRC" && CARGO_MANIFEST_DIR="$SRC" timeout -s KILL "${TEST_TIMEOUT:-900}" taskset -c "${TEST_CPUS:-8,9}" "${cmd[@]}") >"$OUT/logs/$name.log" 2>&1 || rc=$?
    printf '%s\t%s\t%ss\n' "$name" "$rc" "$(( $(date +%s) - start ))" | tee -a "$OUT/tests.tsv"
done <"$OUT/exes.tsv"
echo "failed: $(awk -F'\t' '$2 != 0' "$OUT/tests.tsv" | wc -l) of $(wc -l <"$OUT/tests.tsv")"
