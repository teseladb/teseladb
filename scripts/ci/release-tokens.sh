#!/usr/bin/env bash
# Reads the tokens of the release of main (.forgejo/workflows/deploy-production.yml), the only one that publishes to Docker
# Hub and GitHub, from Vault and hands them to the next steps, masked in the log: DOCKERHUB_TOKEN, of the user teseladb
# of Docker Hub, and GH_TOKEN, for scripts/release/github-release.sh (scopes public_repo and write:packages). The run
# enters Vault through jwt-forgejo with its JWT of audience vault.viuzi and the role <repository>-<stage> of
# vault/configure.sh of atbion-core-infra, which for production only deploy-production.yml on a tag v* reaches, and reads
# secret/ci/teseladb, with the keys dockerhub_token and github_token.
#   scripts/ci/release-tokens.sh <repository> <stage dev|prd>
# Reads VAULT_ADDR, ACTIONS_ID_TOKEN_REQUEST_URL, ACTIONS_ID_TOKEN_REQUEST_TOKEN and FORGEJO_ENV from the runner.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/ci/release-tokens.sh <repository> <dev|prd>" >&2
  exit 2
fi
REPOSITORY=$1
STAGE=$2
case "$STAGE" in
  dev | prd) ;;
  *) echo "release-tokens: unknown stage $STAGE" >&2; exit 2 ;;
esac
: "${VAULT_ADDR:?this runner has no address of Vault}" "${ACTIONS_ID_TOKEN_REQUEST_URL:?}" "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:?}" "${FORGEJO_ENV:?}"
SECRET=secret/data/ci/teseladb

jwt=$(curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=vault.viuzi" | jq -r .value)
echo "::add-mask::$jwt"
# Vault's answer is read whatever its status: a refusal says why (no such role, a claim that does not match), and it
# holds no secret.
login=$(jq -n --arg role "$REPOSITORY-$STAGE" --arg jwt "$jwt" '{role: $role, jwt: $jwt}' |
  curl -sS -X POST "$VAULT_ADDR/v1/auth/jwt-forgejo/login" -H "Content-Type: application/json" --data-binary @-)
token=$(jq -r '.auth.client_token // empty' <<<"$login")
if [[ -z $token ]]; then
  echo "Vault let no run in with the role $REPOSITORY-$STAGE (vault/configure.sh of atbion-core-infra): $(jq -c '.errors // .' <<<"$login")" >&2
  exit 1
fi
echo "::add-mask::$token"
read_secret=$(curl -sS -H "X-Vault-Token: $token" "$VAULT_ADDR/v1/$SECRET")
curl -fsS -X POST -H "X-Vault-Token: $token" "$VAULT_ADDR/v1/auth/token/revoke-self" >/dev/null || true
for pair in dockerhub_token=DOCKERHUB_TOKEN github_token=GH_TOKEN; do
  key=${pair%%=*}
  value=$(jq -r --arg key "$key" '.data.data[$key] // empty' <<<"$read_secret")
  [[ -n $value ]] || { echo "$SECRET has no $key: $(jq -c '.errors // "the key is missing"' <<<"$read_secret")" >&2; exit 1; }
  echo "::add-mask::$value"
  echo "${pair#*=}=$value" >>"$FORGEJO_ENV"
done
echo "the Docker Hub and GitHub tokens of $SECRET are ready for the next steps"
