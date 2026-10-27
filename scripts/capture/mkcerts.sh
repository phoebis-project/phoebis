#!/usr/bin/env bash
# Makes the throwaway CA and the leaf that a control capture's receiver presents for api.anthropic.com,
# platform.claude.com and claude.com, the control hosts the client hardcodes and ANTHROPIC_BASE_URL cannot
# redirect. The capture's client trusts the CA through NODE_EXTRA_CA_CERTS in its own environment, so the CA
# never enters a system or user trust store.
#
# Usage: scripts/capture/mkcerts.sh <out-dir>
#
# Writes <out-dir>/certs/ca.pem, ca.key, leaf.pem and leaf.key. The certificates are made per run and never
# checked in: a private key in the tree trips secret scanners, and a throwaway CA has no value past its run, so
# both certificates expire after 7 days.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out-dir>" >&2
  exit 1
fi

if ! command -v openssl >/dev/null; then
  echo "mkcerts.sh: openssl is needed" >&2
  exit 1
fi

certs="$1/certs"
mkdir -p "$certs"

# Python's default TLS context verifies under OpenSSL's strict rules, which refuse a CA without keyUsage that curl
# accepts, so both certificates carry the extensions a strict verifier asks for. openssl req reads no extension
# file, so the CA is self-signed through x509 -req as the leaf is signed.
printf '%s\n' "basicConstraints = critical, CA:TRUE" "keyUsage = critical, keyCertSign, cRLSign" > "$certs/ca-ext.cnf"
openssl req -newkey rsa:2048 -nodes -subj "/CN=phoebis capture CA" \
  -keyout "$certs/ca.key" -out "$certs/ca.csr"
openssl x509 -req -in "$certs/ca.csr" -signkey "$certs/ca.key" -days 7 -extfile "$certs/ca-ext.cnf" \
  -out "$certs/ca.pem"
openssl req -newkey rsa:2048 -nodes -subj "/CN=api.anthropic.com" \
  -keyout "$certs/leaf.key" -out "$certs/leaf.csr"
printf '%s\n' "basicConstraints = CA:FALSE" "extendedKeyUsage = serverAuth" \
  "keyUsage = digitalSignature, keyEncipherment" \
  "subjectAltName = DNS:api.anthropic.com,DNS:platform.claude.com,DNS:claude.com" > "$certs/ext.cnf"
openssl x509 -req -in "$certs/leaf.csr" -CA "$certs/ca.pem" -CAkey "$certs/ca.key" -CAcreateserial -days 7 \
  -extfile "$certs/ext.cnf" -out "$certs/leaf.pem"
