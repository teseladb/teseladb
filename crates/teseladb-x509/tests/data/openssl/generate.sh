#!/usr/bin/env bash
# Regenerates the OpenSSL fixtures of tests/openssl.rs (013 T007): keys in every format TeselaDB
# reads, a chain that mixes RSA-PSS, ECDSA P-384, P-256 and Ed25519, and a CRL. Written with
# OpenSSL 3.6.3 on 2026-10-30; the output depends on fresh random keys, so after running it update
# scripts/x509-vectors.sha256. The password protects test keys only.
#   crates/teseladb-x509/tests/data/openssl/generate.sh
set -euo pipefail
cd "$(dirname "$0")"
PASS=pass:teseladb-test
DAYS=3650
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out rsa-pkcs8.pem
openssl pkey -in rsa-pkcs8.pem -traditional -out rsa-pkcs1.pem
openssl pkcs8 -topk8 -in rsa-pkcs8.pem -v2 aes-256-cbc -v2prf hmacWithSHA256 -passout "$PASS" -out rsa-enc.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ec256-pkcs8.pem
openssl ec -in ec256-pkcs8.pem -out ec256-sec1.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 -out ec384-pkcs8.pem
openssl pkcs8 -topk8 -in ec384-pkcs8.pem -v2 aes-128-cbc -v2prf hmacWithSHA1 -passout "$PASS" -out ec384-enc.pem
openssl genpkey -algorithm ED25519 -out ed25519.pem

openssl req -x509 -new -key rsa-pkcs8.pem -subj "/C=ES/O=TeselaDB Test/CN=Root" -days "$DAYS" \
  -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:32 -sha256 \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out root.pem
openssl req -new -key ec384-pkcs8.pem -subj "/C=ES/O=TeselaDB Test/CN=Intermediate" -out "$TMP/inter.csr"
printf 'basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid\n' \
  > "$TMP/inter.ext"
openssl x509 -req -in "$TMP/inter.csr" -CA root.pem -CAkey rsa-pkcs8.pem -days "$DAYS" -set_serial 2 \
  -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:32 -sha256 -extfile "$TMP/inter.ext" -out inter.pem
openssl req -new -key ec256-pkcs8.pem -subj "/O=TeselaDB Test/CN=node1" \
  -addext "subjectAltName=DNS:node1.teseladb.test,DNS:*.nodes.teseladb.test,IP:127.0.0.1,IP:::1" -out leaf.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nauthorityKeyIdentifier=keyid\n' \
  > "$TMP/leaf.ext"
openssl x509 -req -in leaf.csr -CA inter.pem -CAkey ec384-pkcs8.pem -days "$DAYS" -set_serial 3 -sha384 \
  -copy_extensions copy -extfile "$TMP/leaf.ext" -out leaf.pem
openssl req -new -key ed25519.pem -subj "/O=TeselaDB Test/OU=clients/CN=alice" -out "$TMP/client.csr"
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n' \
  > "$TMP/client.ext"
openssl x509 -req -in "$TMP/client.csr" -CA inter.pem -CAkey ec384-pkcs8.pem -days "$DAYS" -set_serial 4 -sha384 \
  -extfile "$TMP/client.ext" -out client.pem

printf 'R\t351231000000Z\t261030000000Z\t04\tunknown\t/O=TeselaDB Test/OU=clients/CN=alice\n' > "$TMP/index.txt"
echo 01 > "$TMP/crlnumber"
printf '[ca]\ndefault_ca=c\n[c]\ndatabase=%s/index.txt\ncrlnumber=%s/crlnumber\ndefault_md=sha384\ndefault_crl_days=3650\n' \
  "$TMP" "$TMP" > "$TMP/ca.cnf"
openssl ca -config "$TMP/ca.cnf" -gencrl -keyfile ec384-pkcs8.pem -cert inter.pem -out inter.crl

# Legacy OpenSSL encrypted PEM (`Proc-Type: 4,ENCRYPTED`, D-065), from the keys above.
openssl pkey -in rsa-pkcs8.pem -traditional -aes256 -passout "$PASS" -out rsa-legacy-aes256.pem
openssl pkey -in rsa-pkcs8.pem -traditional -des3 -passout "$PASS" -out rsa-legacy-des3.pem
openssl ec -in ec256-pkcs8.pem -aes128 -passout "$PASS" -out ec256-legacy-aes128.pem
openssl ec -in ec384-pkcs8.pem -aes192 -passout "$PASS" -out ec384-legacy-aes192.pem

# A P-521 root with ecdsa-with-SHA512 over the P-256 leaf key (013 T040, added on 2026-10-31).
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-521 -out ec521-pkcs8.pem
openssl req -x509 -new -key ec521-pkcs8.pem -subj "/C=ES/O=TeselaDB Test/CN=Root P-521" -days "$DAYS" -sha512 \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ec521-root.pem
openssl x509 -req -in leaf.csr -CA ec521-root.pem -CAkey ec521-pkcs8.pem -days "$DAYS" -set_serial 5 -sha512 \
  -copy_extensions copy -extfile <(printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n') \
  -out ec521-leaf.pem
# The same leaf signed by the RSA root with SHA-1, which OpenSSL refuses at security level 1 (013
# T032): `CA signature digest algorithm too weak`.
openssl x509 -req -in leaf.csr -CA root.pem -CAkey rsa-pkcs8.pem -days "$DAYS" -set_serial 6 -sha1 \
  -copy_extensions copy -extfile <(printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n') \
  -out sha1-leaf.pem
