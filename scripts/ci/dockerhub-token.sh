#!/usr/bin/env bash
# Reads the Docker Hub token of the pipelines from Vault for the release of main (.forgejo/workflows/deploy-production.yml),
# the only one that pushes to Docker Hub, and hands it to the next steps as DOCKERHUB_TOKEN, masked in the log. The run
# enters Vault through jwt-forgejo with its JWT of audience vault.viuzi and the role <repository>-<stage> of
# vault/configure.sh of atbion-core-infra, which for production only deploy-production.yml on a tag v* reaches, and reads
# secret/ci/teseladb, whose key dockerhub_token is the token of the user viuzi.
#   scripts/ci/dockerhub-token.sh <repository> <stage dev|prd>
# Reads VAULT_ADDR, ACTIONS_ID_TOKEN_REQUEST_URL, ACTIONS_ID_TOKEN_REQUEST_TOKEN and FORGEJO_ENV from the runner.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/ci/dockerhub-token.sh <repository> <dev|prd>" >&2
  exit 2
fi
REPOSITORY=$1
STAGE=$2
case "$STAGE" in
  dev | prd) ;;
  *) echo "dockerhub-token: unknown stage $STAGE" >&2; exit 2 ;;
esac
: "${VAULT_ADDR:?this runner has no address of Vault}" "${ACTIONS_ID_TOKEN_REQUEST_URL:?}" "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:?}" "${FORGEJO_ENV:?}"
SECRET=secret/data/ci/teseladb

jwt=$(curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=vault.viuzi" | jq -r .value)
echo "::add-mask::$jwt"
token=$(jq -n --arg role "$REPOSITORY-$STAGE" --arg jwt "$jwt" '{role: $role, jwt: $jwt}' |
  curl -fsS -X POST "$VAULT_ADDR/v1/auth/jwt-forgejo/login" -H "Content-Type: application/json" --data-binary @- |
  jq -r '.auth.client_token // empty')
[[ -n $token ]] || { echo "Vault let no run in with the role $REPOSITORY-$STAGE (vault/configure.sh of atbion-core-infra)" >&2; exit 1; }
echo "::add-mask::$token"
value=$(curl -fsS -H "X-Vault-Token: $token" "$VAULT_ADDR/v1/$SECRET" | jq -r '.data.data.dockerhub_token // empty')
curl -fsS -X POST -H "X-Vault-Token: $token" "$VAULT_ADDR/v1/auth/token/revoke-self" >/dev/null || true
[[ -n $value ]] || { echo "$SECRET has no dockerhub_token" >&2; exit 1; }
echo "::add-mask::$value"
echo "DOCKERHUB_TOKEN=$value" >>"$FORGEJO_ENV"
echo "the Docker Hub token of $SECRET is ready for the next steps"
