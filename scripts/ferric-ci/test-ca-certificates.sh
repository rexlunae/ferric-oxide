#!/usr/bin/env bash
# Checks recipes/data/ca-certificates/install-certs.sh against generated bundles.
set -euo pipefail
script="$(cd "$(dirname "$0")/../.." && pwd)/recipes/data/ca-certificates/install-certs.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() {
    echo "$1: $2" >&2
    ls -l "$work/certs" >&2 || true
    exit 1
}
for ca in one two; do
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
        -subj "/CN=Ferric test CA $ca" -keyout "$work/$ca.key" -out "$work/$ca.pem" 2> /dev/null
done
# The bundle layout certifi and curl publish: comment headers between blocks.
{
    echo "# Label: \"Ferric test CA one\""
    cat "$work/one.pem"
    echo
    echo "# Label: \"Ferric test CA two\""
    cat "$work/two.pem"
} > "$work/bundle.pem"

bash "$script" "$work/bundle.pem" "$work/certs"
cmp -s "$work/bundle.pem" "$work/certs/ca-certificates.crt" || fail install "bundle not copied unchanged"
[ "$(find "$work/certs" -maxdepth 1 -name '*.pem' -type f | wc -l)" -eq 2 ] || fail install "expected two certificate files"
for ca in one two; do
    fingerprint="$(openssl x509 -in "$work/$ca.pem" -noout -fingerprint -sha256)"
    fingerprint="${fingerprint#*=}"
    fingerprint="${fingerprint//:/}"
    name="$(printf '%s' "${fingerprint:0:16}" | tr 'A-F' 'a-f').pem"
    [ -f "$work/certs/$name" ] || fail install "$ca is not installed as $name"
    hash="$(openssl x509 -in "$work/$ca.pem" -noout -subject_hash)"
    [ "$(readlink "$work/certs/$hash.0")" = "$name" ] || fail install "$hash.0 does not link to $name"
    old="$(openssl x509 -in "$work/$ca.pem" -noout -subject_hash_old)"
    [ -L "$work/certs/$old.0" ] || fail install "no pre-1.0 hash link $old.0"
    openssl verify -CApath "$work/certs" "$work/$ca.pem" > /dev/null || fail install "$ca does not verify by CApath"
done
[ -z "$(find "$work/certs" -mindepth 1 -type d)" ] || fail install "scratch directory left behind"
echo "PASS: install"

: > "$work/empty.pem"
if bash "$script" "$work/empty.pem" "$work/empty" 2> /dev/null; then
    fail empty "an empty bundle was accepted"
fi
echo "PASS: empty"

cat "$work/one.pem" "$work/one.pem" > "$work/twice.pem"
if bash "$script" "$work/twice.pem" "$work/twice" 2> /dev/null; then
    fail twice "a repeated certificate was accepted"
fi
echo "PASS: twice"

{ cat "$work/one.pem"; printf -- '-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n'; } > "$work/broken.pem"
if bash "$script" "$work/broken.pem" "$work/broken" 2> /dev/null; then
    fail broken "a block that is not a certificate was accepted"
fi
echo "PASS: broken"

# An openssl without rehash, as macOS's LibreSSL: refused up front, by name.
mkdir -p "$work/bin"
cat > "$work/bin/libressl" <<'FAKE'
#!/usr/bin/env bash
case "$1" in
    version) echo "LibreSSL 3.3.6" ;;
    rehash) echo "openssl:Error: 'rehash' is an invalid command." >&2; exit 1 ;;
    *) exec openssl "$@" ;;
esac
FAKE
chmod +x "$work/bin/libressl"
if OPENSSL="$work/bin/libressl" bash "$script" "$work/bundle.pem" "$work/libre" 2> "$work/libre.err"; then
    fail libressl "an openssl without rehash was accepted"
fi
grep -qF "LibreSSL 3.3.6" "$work/libre.err" && grep -qF "no rehash command" "$work/libre.err" ||
    fail libressl "the error does not name the openssl and its missing rehash: $(cat "$work/libre.err")"
[ -z "$(find "$work/libre" -name '*.pem' 2> /dev/null)" ] || fail libressl "certificates installed before the check"
echo "PASS: libressl"

# OPENSSL selects the command used throughout.
cat > "$work/bin/counted" <<'FAKE'
#!/usr/bin/env bash
echo "$1" >> "${COUNT_FILE:?}"
exec openssl "$@"
FAKE
chmod +x "$work/bin/counted"
COUNT_FILE="$work/count" OPENSSL="$work/bin/counted" bash "$script" "$work/bundle.pem" "$work/chosen"
grep -qxF rehash "$work/count" && [ "$(grep -cxF x509 "$work/count")" -eq 4 ] ||
    fail openssl-var "OPENSSL was not used for every call: $(tr '\n' ' ' < "$work/count")"
echo "PASS: openssl-var"
