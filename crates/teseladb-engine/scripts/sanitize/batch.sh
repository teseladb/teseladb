#!/usr/bin/env bash
# batch.sh MODE [BATCH]: builds and runs every test target (and the lib unit tests) in batches
# of BATCH targets, deleting each batch's executables after it ran, so that the target dir
# stays small. Results accumulate in /target/d2-sanitize-out/MODE/all.tsv.
set -uo pipefail
MODE=$1
BATCH=${2:-25}
SRC=/target/d2-sanitize-src
OUT=/target/d2-sanitize-out/$MODE
mkdir -p "$OUT"
: >"$OUT/all.tsv"
mapfile -t names < <(cd "$SRC/tests" && ls *.rs | sed 's/\.rs$//' | grep -v -x -e slab_test ${SKIP:+-e $SKIP})
targets=(lib "${names[@]}")
for ((i = 0; i < ${#targets[@]}; i += BATCH)); do
    sel=()
    for t in "${targets[@]:i:BATCH}"; do
        if [ "$t" = lib ]; then sel+=(--lib); else sel+=(--test "$t"); fi
    done
    SEL="${sel[*]}" bash /work/inner.sh "$MODE" build | tail -3
    bash /work/inner.sh "$MODE" run >/dev/null
    cat "$OUT/tests.tsv" | tee -a "$OUT/all.tsv"
    awk -F'\t' '{print $3}' "$OUT/exes.tsv" | xargs -r rm -f
    du -sh "/target/d2-sanitize-target-$MODE" 2>/dev/null
done
echo "ALL DONE: $(awk -F'\t' '$2 != 0' "$OUT/all.tsv" | wc -l) failed of $(wc -l <"$OUT/all.tsv")"
