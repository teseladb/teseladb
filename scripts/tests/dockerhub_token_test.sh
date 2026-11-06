#!/usr/bin/env bash
# Tests of scripts/ci/dockerhub-token.sh against a Vault and a token endpoint of Forgejo that a local server plays: a
# refused login says Vault's reason, a secret without the key says so, and a good one writes DOCKERHUB_TOKEN for the
# next steps.
#   scripts/tests/dockerhub_token_test.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

SCRIPT=$PWD/scripts/ci/dockerhub-token.sh
WORK=$(mktemp -d)
SERVER=""
cleanup() {
  if [[ -n $SERVER ]]; then kill "$SERVER" 2>/dev/null || true; fi
  find "$WORK" -depth -delete
}
trap cleanup EXIT
passed=0
failed=0

# The server answers each path with the body of the file named after the case: the JWT of the run, the login and the
# secret.
cat > "$WORK/serve.py" <<'PY'
import http.server, os, sys
case_dir = sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def answer(self):
        name = {"/id": "id", "/v1/auth/jwt-forgejo/login": "login", "/v1/secret/data/ci/teseladb": "secret",
                "/v1/auth/token/revoke-self": "revoke"}.get(self.path.split("?")[0], "missing")
        body = open(os.path.join(case_dir, "current", name)).read().encode() if name != "missing" else b"{}"
        status = 400 if b'"errors"' in body else 200
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)
    do_GET = do_POST = answer
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
PORT=$((39000 + RANDOM % 1000))
mkdir -p "$WORK/current"
python3 "$WORK/serve.py" "$PORT" "$WORK" &
SERVER=$!
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$PORT/id" 2>/dev/null && break; echo '{}' >"$WORK/current/id"; sleep 0.1; done

# Writes the answers of a case.
answers() {
  echo '{"value": "the-jwt"}' >"$WORK/current/id"
  echo "$1" >"$WORK/current/login"
  echo "$2" >"$WORK/current/secret"
  echo '{}' >"$WORK/current/revoke"
}

# Runs the script; its exit code, what it printed and what it wrote for the next steps.
run() {
  : >"$WORK/env"
  set +e
  VAULT_ADDR="http://127.0.0.1:$PORT" ACTIONS_ID_TOKEN_REQUEST_URL="http://127.0.0.1:$PORT/id?x=1" \
    ACTIONS_ID_TOKEN_REQUEST_TOKEN=t FORGEJO_ENV="$WORK/env" "$SCRIPT" teseladb prd >"$WORK/out" 2>&1
  code=$?
  set -e
}

check() {
  local name=$1 want_code=$2 want_text=$3
  if [[ $code -eq $want_code ]] && grep -qF -- "$want_text" "$WORK/out" "$WORK/env"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $name: exit $code, wanted $want_code and «$want_text»:" >&2
    cat "$WORK/out" >&2
  fi
}

answers '{"errors": ["role \"teseladb-prd\" could not be found"]}' '{}'
run
check "a role Vault does not have" 1 'role \"teseladb-prd\" could not be found'

answers '{"auth": {"client_token": "tok"}}' '{"data": {"data": {}}}'
run
check "a secret without the key" 1 "has no dockerhub_token: \"the key is missing\""

answers '{"auth": {"client_token": "tok"}}' '{"data": {"data": {"dockerhub_token": "hub-token"}}}'
run
check "the token for the next steps" 0 "DOCKERHUB_TOKEN=hub-token"

answers '{}' '{}'
VAULT_ADDR="" ACTIONS_ID_TOKEN_REQUEST_URL=x ACTIONS_ID_TOKEN_REQUEST_TOKEN=t FORGEJO_ENV="$WORK/env" "$SCRIPT" teseladb prd >"$WORK/out" 2>&1 && code=0 || code=$?
check "a runner without Vault" 1 "this runner has no address of Vault"
"$SCRIPT" teseladb staging >"$WORK/out" 2>&1 && code=0 || code=$?
check "an unknown stage" 2 "unknown stage staging"

echo "dockerhub_token_test: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
