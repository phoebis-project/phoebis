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

openssl req -x509 -newkey rsa:2048 -nodes -days 7 -subj "/CN=phoebis capture CA" \
  -keyout "$certs/ca.key" -out "$certs/ca.pem"
openssl req -newkey rsa:2048 -nodes -subj "/CN=api.anthropic.com" \
  -keyout "$certs/leaf.key" -out "$certs/leaf.csr"
echo "subjectAltName = DNS:api.anthropic.com,DNS:platform.claude.com,DNS:claude.com" > "$certs/ext.cnf"
openssl x509 -req -in "$certs/leaf.csr" -CA "$certs/ca.pem" -CAkey "$certs/ca.key" -CAcreateserial -days 7 \
  -extfile "$certs/ext.cnf" -out "$certs/leaf.pem"
