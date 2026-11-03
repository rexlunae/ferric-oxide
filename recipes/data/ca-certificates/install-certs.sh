#!/usr/bin/env bash
# Usage: install-certs.sh BUNDLE CERTDIR
#
# Installs the PEM bundle BUNDLE (Mozilla's CA certificates, as certifi and
# curl publish them) into CERTDIR the way OpenSSL and rustls look for it:
#
#   CERTDIR/ca-certificates.crt   the bundle, unchanged
#   CERTDIR/<fingerprint>.pem     one file per certificate, named after the
#                                 first 16 hex digits of its SHA-256
#   CERTDIR/<hash>.<n>            `openssl rehash -compat` links to them, by
#                                 current and pre-1.0 OpenSSL subject hash
#
# Needs `openssl` on the build host. Fails if the bundle holds no certificate
# or a block that is not one.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 BUNDLE CERTDIR" >&2
    exit 2
fi
bundle="$1"
certdir="$2"

if ! command -v openssl > /dev/null; then
    echo "install-certs.sh: openssl is needed to hash the certificates" >&2
    exit 1
fi

blocks="$(grep -c -- '-----BEGIN CERTIFICATE-----' "$bundle" || true)"
if [ "$blocks" -eq 0 ]; then
    echo "install-certs.sh: $bundle holds no certificate" >&2
    exit 1
fi

mkdir -p "$certdir"
work="$(mktemp -d "$certdir/.split.XXXXXX")"
trap 'rm -rf "$work"' EXIT

awk -v dir="$work" '
    /^-----BEGIN CERTIFICATE-----$/ { n++; file = sprintf("%s/%04d", dir, n); inside = 1 }
    inside { print > file }
    /^-----END CERTIFICATE-----$/ { inside = 0; close(file) }
' "$bundle"

for split in "$work"/*; do
    fingerprint="$(openssl x509 -in "$split" -noout -fingerprint -sha256)"
    fingerprint="${fingerprint#*=}"
    fingerprint="${fingerprint//:/}"
    name="$(printf '%s' "${fingerprint:0:16}" | tr 'A-F' 'a-f').pem"
    if [ -e "$certdir/$name" ]; then
        echo "install-certs.sh: $bundle holds the certificate $name twice" >&2
        exit 1
    fi
    openssl x509 -in "$split" -out "$certdir/$name"
done

installed="$(find "$certdir" -maxdepth 1 -name '*.pem' -type f | wc -l)"
if [ "$installed" -ne "$blocks" ]; then
    echo "install-certs.sh: $bundle has $blocks certificate blocks, $installed were installed" >&2
    exit 1
fi

rm -rf "$work"
openssl rehash -compat "$certdir"
cp "$bundle" "$certdir/ca-certificates.crt"
