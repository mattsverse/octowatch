#!/bin/bash
# Run after cargo-bundle creates the universal .app. Credentials come from CI.
set -euo pipefail
source "$(dirname "$0")/keychain.sh"

for variable in APPLE_CERTIFICATE_P12 APPLE_CERTIFICATE_PASSWORD APPLE_ID APPLE_TEAM_ID APPLE_APP_SPECIFIC_PASSWORD; do
    if [[ -z "${!variable:-}" ]]; then
        echo "Missing required secret: $variable" >&2
        exit 1
    fi
done

app="target/universal/release/bundle/osx/Octowatcher.app"
dmg="target/universal/release/bundle/dmg/Octowatcher.dmg"
[[ -d "$app" ]] || { echo "Missing universal app: $app" >&2; exit 1; }

umask 077
work_dir=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/octowatcher-signing.XXXXXX")
keychain="$work_dir/signing.keychain-db"
mount_point="$work_dir/mounted"
cleanup() {
    if mount | grep -Fq " on $mount_point ("; then
        hdiutil detach "$mount_point" || true
    fi
    restore_keychain_search_list || true
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
    rm -rf "$work_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

keychain_password=$(openssl rand -hex 32)
printf '%s' "$APPLE_CERTIFICATE_P12" | base64 --decode > "$work_dir/certificate.p12"
security create-keychain -p "$keychain_password" "$keychain"
use_signing_keychain "$keychain"
security set-keychain-settings -lut 7200 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$work_dir/certificate.p12" -k "$keychain" \
    -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
    -k "$keychain_password" "$keychain" >/dev/null
rm "$work_dir/certificate.p12"

# Select only a Developer ID Application identity from the requested team.
identity=$(security find-identity -v -p codesigning "$keychain" | \
    awk -v team="($APPLE_TEAM_ID)" '/"Developer ID Application:/ && index($0, team) { print $2; exit }')
if [[ -z "$identity" ]]; then
    echo "No valid Developer ID Application certificate for team $APPLE_TEAM_ID in the P12" >&2
    exit 1
fi

xcrun notarytool store-credentials octowatcher --keychain "$keychain" \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_SPECIFIC_PASSWORD"
umask 022

notarize() {
    local artifact=$1
    local result="$work_dir/notarization.json"
    local submission_id
    local submit_status=0
    # Keep the response even on failure so a rejected submission has a useful log.
    xcrun notarytool submit "$artifact" --keychain "$keychain" \
        --keychain-profile octowatcher --wait --timeout 40m --output-format json > "$result" || submit_status=$?
    cat "$result"
    if [[ "$submit_status" -ne 0 ]] || ! jq -se 'length == 1 and .[0].status == "Accepted"' "$result" >/dev/null; then
        echo "Notarization failed or timed out for $artifact" >&2
        submission_id=$(jq -r '.id // empty' "$result" || true)
        if [[ -n "$submission_id" ]]; then
            xcrun notarytool log "$submission_id" --keychain "$keychain" \
                --keychain-profile octowatcher || true
        fi
        return 1
    fi
}

# The bundle contains a single Rust executable, with no embedded frameworks.
codesign --force --sign "$identity" --keychain "$keychain" \
    --options runtime --timestamp "$app"
codesign --verify --deep --strict --verbose=2 "$app"
ditto -c -k --keepParent "$app" "$work_dir/Octowatcher.zip"
notarize "$work_dir/Octowatcher.zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"

# Both downloads contain the same signed app and its offline notarization ticket.
# COPYFILE_DISABLE excludes AppleDouble metadata, preserving normal bundle files.
COPYFILE_DISABLE=1 tar -C "$(dirname "$app")" -czf octowatcher-macos.tar.gz Octowatcher.app
mkdir "$work_dir/extracted"
tar -xzf octowatcher-macos.tar.gz -C "$work_dir/extracted"
codesign --verify --deep --strict --verbose=2 "$work_dir/extracted/Octowatcher.app"
xcrun stapler validate "$work_dir/extracted/Octowatcher.app"

mkdir -p "$(dirname "$dmg")" "$work_dir/dmg"
ditto "$app" "$work_dir/dmg/Octowatcher.app"
ln -s /Applications "$work_dir/dmg/Applications"
# TEMP: diagnose notarytool rejecting the DMG
inspect() {
    echo "=== $1: $2"
    ls -l "$2" || true
    hdiutil imageinfo "$2" 2>&1 | head -5 || true
    tail -c 512 "$2" | xxd | head -2 || true
}
sw_vers; xcode-select -p; xcrun notarytool --version; df -h . "$work_dir" || true
hdiutil create -ov -volname Octowatcher -fs HFS+ -format UDZO \
    -srcfolder "$work_dir/dmg" "$dmg"
inspect "after create" "$dmg"
mkdir -p diag
cp "$dmg" diag/fresh.dmg
cp -R "$work_dir/dmg" diag/srcfolder
curl -fsSL -o diag/v0.4.1.dmg https://github.com/mattsverse/octowatch/releases/download/v0.4.1/Octowatcher.dmg || true
inspect "control: published v0.4.1 DMG" diag/v0.4.1.dmg
hdiutil verify diag/v0.4.1.dmg 2>&1 | tail -3 || true
hdiutil verify "$dmg" 2>&1 | tail -3 || true
hdiutil create -ov -volname Octowatcher -fs HFS+ -format UDRW \
    -srcfolder "$work_dir/dmg" "$work_dir/uncompressed.dmg"
inspect "uncompressed UDRW" "$work_dir/uncompressed.dmg"
hdiutil create -ov -volname Octowatcher -fs APFS -format UDZO \
    -srcfolder "$work_dir/dmg" "$work_dir/apfs.dmg"
inspect "APFS UDZO" "$work_dir/apfs.dmg"
hdiutil create -ov -volname Octowatcher -fs HFS+ -format UDZO \
    -srcfolder "$work_dir/dmg" "$work_dir/temp-copy.dmg"
inspect "temp-dir copy after create" "$work_dir/temp-copy.dmg"
codesign --force --sign "$identity" --keychain "$keychain" --timestamp "$dmg"
inspect "after codesign" "$dmg"
codesign --force --sign "$identity" --keychain "$keychain" --timestamp "$work_dir/temp-copy.dmg"
inspect "temp-dir copy after codesign" "$work_dir/temp-copy.dmg"
notarize "$dmg"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
codesign --verify --strict --verbose=2 "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"

# Check the actual app users drag out of the final image.
hdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$mount_point"
codesign --verify --deep --strict --verbose=2 "$mount_point/Octowatcher.app"
xcrun stapler validate "$mount_point/Octowatcher.app"
spctl --assess --type execute --verbose=2 "$mount_point/Octowatcher.app"
hdiutil detach "$mount_point"
