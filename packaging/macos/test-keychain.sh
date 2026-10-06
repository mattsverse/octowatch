#!/bin/bash
# Native macOS regression test; creates its own identity, with no Apple secrets.
# Requires OpenSSL 3 (e.g. Homebrew) and the Xcode command-line tools.
set -euo pipefail
source "$(dirname "$0")/keychain.sh"

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/octowatcher-keychain-test.XXXXXX")
keychain="$test_dir/signing.keychain-db"
security list-keychains -d user > "$test_dir/original-keychains"
cleanup() {
    restore_keychain_search_list || true
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
    rm -rf "$test_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=Octowatcher Temporary Signing Test' \
    -addext 'keyUsage=critical,digitalSignature' \
    -addext 'extendedKeyUsage=codeSigning' \
    -keyout "$test_dir/key.pem" -out "$test_dir/cert.pem" >/dev/null 2>&1
openssl pkcs12 -export -legacy -inkey "$test_dir/key.pem" -in "$test_dir/cert.pem" \
    -out "$test_dir/cert.p12" -passout pass:temporary-test-password
security create-keychain -p temporary-test-password "$keychain"
security unlock-keychain -p temporary-test-password "$keychain"
security import "$test_dir/cert.p12" -k "$keychain" -P temporary-test-password \
    -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
    -k temporary-test-password "$keychain" >/dev/null
# This self-signed identity can sign code but is intentionally not trusted for
# distribution, so omit find-identity's valid-only (-v) filter in this test.
identity=$(security find-identity -p codesigning "$keychain" | \
    awk '/"Octowatcher Temporary Signing Test"/ { print $2; exit }')
[[ -n "$identity" ]]
printf 'int main(void) { return 0; }\n' > "$test_dir/main.c"
xcrun clang "$test_dir/main.c" -o "$test_dir/probe"

if codesign --force --sign "$identity" --keychain "$keychain" \
    --options runtime --timestamp=none "$test_dir/probe" > "$test_dir/baseline.log" 2>&1; then
    echo "Expected identity lookup to fail outside the search list" >&2
    exit 1
fi
grep -E 'no identity found|specified item could not be found in the keychain' "$test_dir/baseline.log"

use_signing_keychain "$keychain"
codesign --force --sign "$identity" --keychain "$keychain" \
    --options runtime --timestamp=none "$test_dir/probe"
codesign --verify --strict "$test_dir/probe"
restore_keychain_search_list
security list-keychains -d user > "$test_dir/restored-keychains"
diff -u "$test_dir/original-keychains" "$test_dir/restored-keychains"
echo "PASS: native signing succeeds and the original keychain search list is restored"
