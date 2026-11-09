#!/usr/bin/env bash
# Runs TLC over a model in spec/tla/, or over every model of a directory. The TLA+ tools jar is a
# development tool: it is downloaded once into .tools/ (ignored by git), its SHA-256 is verified
# and it is not part of the repository.
# Usage: scripts/tlc.sh spec/tla/LeaseRead.tla [TLC options]
#        TLC_CFG=spec/tla/LeaseRead.other.cfg scripts/tlc.sh spec/tla/LeaseRead.tla
#        scripts/tlc.sh --all [dir]   every model with each of its .cfg (default spec/tla); writes
#                                     dir/RESULTS.md with states, depth and date (002 FR-012) only
#                                     if none fails
# Models have to be under the repository: without Java, TLC runs in Docker over it.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
JAR="$ROOT/.tools/tla2tools.jar"
VERSION=1.7.4
SHA=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
URL=https://github.com/tlaplus/tlaplus/releases/download/v$VERSION/tla2tools.jar

usage() {
  echo "usage: scripts/tlc.sh spec/tla/<Model>.tla [TLC options] | scripts/tlc.sh --all [dir]" >&2
  exit 2
}

fetch_jar() {
  [[ -f $JAR ]] && return
  mkdir -p "$ROOT/.tools"
  curl -fsSL -o "$JAR.tmp" "$URL" || {
    echo "tlc: cannot download $URL" >&2
    exit 1
  }
  if ! echo "$SHA  $JAR.tmp" | shasum -a 256 -c - >/dev/null 2>&1; then
    echo "tlc: $URL does not have the expected SHA-256 ($SHA); not used" >&2
    rm -f "$JAR.tmp"
    exit 1
  fi
  mv "$JAR.tmp" "$JAR"
}

# The Java on the host that works, if any: `$JAVA_HOME`, the `java` of the path, or a Homebrew
# OpenJDK that is installed without being linked. On macOS `java` may exist as a launcher without
# a JDK behind it, so each one is tried.
JAVA=
for candidate in ${JAVA_HOME:+"$JAVA_HOME/bin/java"} java \
  /opt/homebrew/opt/openjdk/bin/java /usr/local/opt/openjdk/bin/java; do
  if "$candidate" -version >/dev/null 2>&1; then
    JAVA=$candidate
    break
  fi
done

# Where TLC runs: Java on the host or, failing that, Docker.
runner() {
  if [[ -n $JAVA ]]; then
    echo java
  elif docker version >/dev/null 2>&1; then
    echo docker
  else
    echo "tlc: TLC needs Java 11 or later, or Docker to run it in eclipse-temurin:21-jre" >&2
    exit 1
  fi
}

# Runs TLC with runner $1 over model $2 and configuration $3, with the options after them. TLC's
# state files go to a temporary directory (inside the container when it runs there).
tlc() {
  local how=$1 model=$2 cfg=$3 meta=/tmp/tlc-states-$$-$RANDOM
  shift 3
  if [[ $how == java ]]; then
    "$JAVA" -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -metadir "$meta" -config "$cfg" \
      "$model" "$@"
    return
  fi
  # Inside the container the repository is /w: paths under it are rewritten.
  model=$(cd "$(dirname "$model")" && pwd)/$(basename "$model")
  cfg=$(cd "$(dirname "$cfg")" && pwd)/$(basename "$cfg")
  if [[ $model != "$ROOT"/* || $cfg != "$ROOT"/* ]]; then
    echo "tlc: without Java the model has to be under $ROOT" >&2
    return 2
  fi
  # The jar is mounted on its own: in a worktree `.tools/tla2tools.jar` is a link to a path of the
  # main checkout that the container does not see.
  local jar=$JAR
  [[ -L $jar ]] && jar=$(readlink "$jar")
  [[ $jar == /* ]] || jar="$ROOT/.tools/$jar"
  docker run --rm -v "$ROOT:/w" -v "$jar:/opt/tla2tools.jar:ro" -w /w eclipse-temurin:21-jre \
    java -XX:+UseParallelGC -cp /opt/tla2tools.jar tlc2.TLC -workers auto -metadir "$meta" \
    -config "${cfg#"$ROOT"/}" "${model#"$ROOT"/}" "$@"
}

# $1 with a dot between thousands, as Spanish text writes numbers.
thousands() {
  sed -E ':a
s/^([0-9]+)([0-9]{3})/\1.\2/
ta' <<<"$1"
}

# The row of RESULTS.md for model $1, configuration $2, TLC's output $3 and $4 seconds: TLC ends
# with "N states generated, M distinct states found, …" and "The depth of the complete state
# graph search is D."
row() {
  local totals depth
  totals=$(grep -E '^[0-9]+ states generated, [0-9]+ distinct states found' <<<"$3" | tail -1)
  depth=$(sed -nE 's/^The depth of the complete state graph search is ([0-9]+)\..*/\1/p' <<<"$3")
  printf "| \`%s\` | \`%s\` | %s | %s | %s | %s s |\n" "$1" "$2" \
    "$(thousands "$(sed -E 's/^([0-9]+) states generated.*/\1/' <<<"$totals")")" \
    "$(thousands "$(sed -E 's/.*, ([0-9]+) distinct states found.*/\1/' <<<"$totals")")" "$depth" "$4"
}

all() {
  local dir=$1 how model cfg out start rows="" failed=0
  [[ -d $dir ]] || usage
  how=$(runner)
  fetch_jar
  for model in "$dir"/*.tla; do
    [[ -f $model ]] || continue
    for cfg in "${model%.tla}".cfg "${model%.tla}".*.cfg; do
      [[ -f $cfg ]] || continue
      start=$SECONDS
      out=$(tlc "$how" "$model" "$cfg" 2>&1) || true
      if ! grep -q 'No error has been found' <<<"$out"; then
        echo "$out"
        echo "tlc: $cfg: TLC found an error or did not finish" >&2
        failed=1
        continue
      fi
      rows+=$(row "$(basename "${model%.tla}")" "$(basename "$cfg")" "$out" $((SECONDS - start)))
      rows+=$'\n'
      echo "-- $cfg: $(grep -m1 -E '^[0-9]+ states generated' <<<"$out")"
    done
  done
  [[ $failed -eq 0 ]] || return 1
  if [[ -z $rows ]]; then
    echo "tlc: $dir has no model with a .cfg" >&2
    return 1
  fi
  {
    echo "# Resultados de TLC"
    echo
    echo "Generado por \`scripts/tlc.sh --all\` el $(date +%d-%m-%Y) con TLA+ tools $VERSION ($how, todos los workers)."
    echo "Cada modelo explica en su cabecera qué comprueba; \`scripts/ci.sh\` vuelve a generar esta tabla en la puerta 7."
    echo
    echo "| Modelo | Configuración | Estados generados | Estados distintos | Profundidad | Tiempo |"
    echo "|--------|---------------|-------------------|-------------------|-------------|--------|"
    printf '%s' "$rows"
    echo
    echo "Ninguno encuentra errores."
  } >"$dir/RESULTS.md.tmp"
  mv "$dir/RESULTS.md.tmp" "$dir/RESULTS.md"
}

if [[ ${1:-} == --all ]]; then
  [[ $# -le 2 ]] || usage
  all "${2:-$ROOT/spec/tla}"
  exit
fi
if [[ $# -lt 1 || ! -f $1 ]]; then
  usage
fi
MODEL=$1
shift
how=$(runner)
fetch_jar
tlc "$how" "$MODEL" "${TLC_CFG:-${MODEL%.tla}.cfg}" "$@"
