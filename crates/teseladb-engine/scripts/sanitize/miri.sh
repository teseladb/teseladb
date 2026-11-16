#!/usr/bin/env bash
# miri.sh [test targets...]: runs the given integration tests and the lib unit tests under Miri.
set -uo pipefail
export RUSTUP_HOME=/target/d2-sanitize-rustup CARGO_HOME=/target/d2-sanitize-cargo
export CARGO_TARGET_DIR=/target/d2-sanitize-target-miri CARGO_NET_OFFLINE=false
export RUSTFLAGS="--cfg engine_default_allocator --cfg no_exception_intercept"
export MIRIFLAGS=${MIRIFLAGS:--Zmiri-disable-isolation -Zmiri-many-seeds=0..1}
OUT=/target/d2-sanitize-out/miri
mkdir -p "$OUT"
cd /target/d2-sanitize-src
TESTS=${*:-checked_ptr_test circular_buffer_fixed_capacity_test circular_buffer_test chunked_fifo_test cov_core_small_test defer_test deleter_test noncopyable_function_test packet_test shared_ptr_test simple_stream_test cov_simple_stream_test sstring_test tuple_utils_test weak_ptr_test cov_small_utils_test cov_misc_callables_test tcp_option_test source_location_test net_config_test}
: >"$OUT/tests.tsv"
for t in $TESTS; do
    if [ "$t" = lib ]; then sel=(--lib -- "${LIB_FILTER:-chunked_fifo}"); else sel=(--test "$t" -- --skip random_walk --skip random --skip stress --skip many --skip backtraces --skip thread_cputime --skip traced); fi
    rc=0
    timeout -s KILL "${MIRI_TIMEOUT:-1500}" /usr/local/cargo/bin/cargo +nightly-2026-08-27 miri test "${sel[@]}" >"$OUT/$t.log" 2>&1 || rc=$?
    printf '%s\t%s\n' "$t" "$rc" | tee -a "$OUT/tests.tsv"
done
