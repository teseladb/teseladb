#!/usr/bin/env bash
# TLS interoperability of a secure node with other TLS stacks (013 T009 and T041, FR-019,
# FR-003): OpenSSL's `s_client` and `curl` complete TLS 1.3 and TLS 1.2 (each TLS 1.2 suite) with
# the clients' port and verify the node certificate against the CA of the PKI and the name
# `localhost`; a TLS 1.2 fallback is refused (RFC 7507), versions before TLS 1.2 are refused on the
# clients' port, and anything before TLS 1.3 on the port between nodes.
#   scripts/tls-interop.sh            (needs openssl 3, curl and python3 in PATH)
set -euo pipefail
cd "$(dirname "$0")/.."

OPENSSL=${OPENSSL:-openssl}
CURL=${CURL:-curl}
for tool in "$OPENSSL" "$CURL"; do
  command -v "$tool" >/dev/null || { echo "tls-interop: $tool is missing" >&2; exit 1; }
done
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
cargo build -q --offline -p teseladb-server
BIN=$CARGO_TARGET_DIR/debug/teseladb

WORK=$(mktemp -d)
NODE=
cleanup() {
  [[ -z $NODE ]] || kill "$NODE" 2>/dev/null || true
  [[ -z $NODE ]] || wait "$NODE" 2>/dev/null || true
  find "$WORK" -depth -delete
}
trap cleanup EXIT

# The node logs the clients' port it got; the peer port is chosen here.
PEERS=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
"$BIN" start --cores 1 --port 0 --peer-listen "127.0.0.1:$PEERS" --dbpath "$WORK/data" \
  --logpath "$WORK/log" >"$WORK/out" 2>&1 &
NODE=$!
clients_port() {
  sed -n "s/.*clients' port addr=[^ ]*:\([0-9]*\).*/\1/p" "$WORK/log" 2>/dev/null | head -n 1
}
for _ in $(seq 600); do
  [[ -n $(clients_port) ]] && break
  kill -0 "$NODE" 2>/dev/null || { cat "$WORK/out" >&2; exit 1; }
  sleep 0.1
done
CLIENTS=$(clients_port)
[[ -n $CLIENTS ]] || { echo "tls-interop: the node did not start" >&2; exit 1; }
CA=$WORK/data/pki/ca.pem

passed=0
failed=0
# expect <name> <text the output has> <command…>
expect() {
  local name=$1 text=$2 out
  shift 2
  out=$("$@" </dev/null 2>&1 || true)
  if [[ $out == *"$text"* ]]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $name: no \"$text\" in:" >&2
    echo "$out" | tail -n 20 >&2
  fi
}
s_client() {
  "$OPENSSL" s_client -connect "127.0.0.1:$1" -CAfile "$CA" -verify_return_error \
    -verify_hostname localhost "${@:2}"
}

expect "openssl, TLS 1.3, clients" "Verify return code: 0 (ok)" s_client "$CLIENTS" -tls1_3
expect "openssl, TLS 1.3 protocol" "Protocol: TLSv1.3" s_client "$CLIENTS" -tls1_3
expect "openssl, TLS 1.1 refused" "alert protocol version" \
  s_client "$CLIENTS" -tls1_1 -cipher 'DEFAULT@SECLEVEL=0'
for suite in ECDHE-ECDSA-AES128-GCM-SHA256 ECDHE-ECDSA-AES256-GCM-SHA384 ECDHE-ECDSA-CHACHA20-POLY1305; do
  expect "openssl, TLS 1.2, $suite" "Cipher is $suite" s_client "$CLIENTS" -tls1_2 -cipher "$suite"
done
expect "openssl, TLS 1.2 protocol" "Protocol: TLSv1.2" s_client "$CLIENTS" -tls1_2
expect "openssl, TLS 1.2 certificate" "Verify return code: 0 (ok)" s_client "$CLIENTS" -tls1_2
expect "openssl, TLS 1.2 extended master secret" "Extended master secret: yes" \
  s_client "$CLIENTS" -tls1_2
expect "openssl, TLS 1.2 fallback refused" "inappropriate fallback" \
  s_client "$CLIENTS" -tls1_2 -fallback_scsv
expect "openssl, TLS 1.3, peers" "Verify return code: 0 (ok)" s_client "$PEERS" -tls1_3
expect "openssl, TLS 1.2 refused between nodes" "Cipher is (NONE)" s_client "$PEERS" -tls1_2
expect "curl, TLS 1.2, clients" "SSL connection using TLSv1.2" \
  "$CURL" -sv --tlsv1.2 --tls-max 1.2 --cacert "$CA" "https://localhost:$CLIENTS/" -o /dev/null
expect "curl, TLS 1.3, clients" "SSL certificate verify ok" \
  "$CURL" -sv --cacert "$CA" "https://localhost:$CLIENTS/" -o /dev/null

echo "tls-interop: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
