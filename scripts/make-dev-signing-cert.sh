#!/usr/bin/env bash
# Create a self-signed code-signing identity ("WireDesk Dev") in the login
# keychain so scripts/build-mac-app.sh can sign every rebuild with the SAME
# identity. macOS keys the Accessibility grant to the code signature: an
# ad-hoc signature changes on every build and the grant is lost each time,
# a fixed identity keeps it. Run once; idempotent.
#
# Two password prompts are normal on first use: `add-trusted-cert` asks for
# the login password, and the first `codesign` with the new key shows
# "codesign wants to access key …" — choose "Always Allow".
set -euo pipefail

NAME="${1:-WireDesk Dev}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "\"$NAME\""; then
    echo "Identity '$NAME' already exists — nothing to do."
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = codesign
prompt = no
[dn]
CN = $NAME
[codesign]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
subjectKeyIdentifier = hash
EOF

echo "==> Generating self-signed certificate '$NAME' (10 years)…"
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -config "$TMP/openssl.cnf" 2>/dev/null

# A throwaway export password: the .p12 lives only inside $TMP for a moment.
openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -out "$TMP/dev.p12" -passout pass:wiredesk -name "$NAME"

echo "==> Importing into login keychain…"
security import "$TMP/dev.p12" -k "$KEYCHAIN" -P wiredesk \
    -T /usr/bin/codesign -T /usr/bin/security >/dev/null

echo "==> Trusting the certificate for code signing (login password prompt)…"
security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$TMP/cert.pem"

echo
echo "Done. Verify with:  security find-identity -v -p codesigning"
echo "scripts/build-mac-app.sh picks '$NAME' up automatically."
echo "The Accessibility grant must be given once more for the first build"
echo "signed this way; after that it survives rebuilds."
