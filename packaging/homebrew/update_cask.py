#!/usr/bin/env python3
"""Verify a published DMG and update only the tap cask's version and checksum."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

REPOSITORY = "mattsverse/octowatch"
ASSET = "Octowatcher.dmg"


def stable_version(value):
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", value):
        raise ValueError(f"Expected a stable X.Y.Z version, got {value!r}")
    return tuple(map(int, value.split(".")))


def update_cask(content, version, checksum):
    requested = stable_version(version)
    if not re.fullmatch(r"[0-9a-f]{64}", checksum):
        raise ValueError("Expected a SHA-256 checksum")
    versions = re.findall(r'^  version "([^"]+)"$', content, re.MULTILINE)
    checksums = re.findall(r'^  sha256 "([0-9a-f]{64})"$', content, re.MULTILINE)
    if len(versions) != 1 or len(checksums) != 1:
        raise ValueError("Expected exactly one version and SHA-256 stanza in the cask")
    current = stable_version(versions[0])
    if requested < current:
        print(f"Skipping {version}: the tap already has {versions[0]}")
        return content
    if requested == current and checksum != checksums[0]:
        raise ValueError("Published DMG changed for the same version; investigate before changing the cask")
    content = re.sub(r'^  version "[^"]+"$', f'  version "{version}"', content, flags=re.MULTILINE)
    return re.sub(r'^  sha256 "[0-9a-f]{64}"$', f'  sha256 "{checksum}"', content, flags=re.MULTILINE)


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True)


def release_checksum(tag):
    # Validate before using the tag in any CLI arguments or download paths.
    if not tag.startswith("v"):
        raise ValueError("Expected a release tag beginning with v")
    stable_version(tag[1:])
    release = json.loads(gh("release", "view", tag, "--repo", REPOSITORY,
                            "--json", "tagName,isDraft,isPrerelease,assets"))
    if release["tagName"] != tag or release["isDraft"] or release["isPrerelease"]:
        raise ValueError("The cask requires an already published stable release")
    assets = [asset for asset in release["assets"] if asset["name"] == ASSET]
    if len(assets) != 1:
        raise ValueError(f"Expected exactly one published {ASSET}")
    expected_url = f"https://github.com/{REPOSITORY}/releases/download/{tag}/{ASSET}"
    if assets[0]["url"] != expected_url:
        raise ValueError("Unexpected DMG download URL")
    with tempfile.TemporaryDirectory(prefix="octowatcher-homebrew-") as directory:
        gh("release", "download", tag, "--repo", REPOSITORY,
           "--pattern", ASSET, "--dir", directory)
        checksum = hashlib.sha256((Path(directory) / ASSET).read_bytes()).hexdigest()
    digest = assets[0].get("digest")
    if digest and digest != f"sha256:{checksum}":
        raise ValueError("Downloaded DMG does not match GitHub's asset digest")
    return checksum


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Published stable release tag, e.g. v0.6.0")
    parser.add_argument("cask", type=Path, help="Existing Casks/octowatcher.rb in the tap checkout")
    args = parser.parse_args()
    if not args.cask.is_file():
        parser.error("Merge the initial Casks/octowatcher.rb into the tap before running automation")
    try:
        checksum = release_checksum(args.tag)
        content = args.cask.read_text()
        updated = update_cask(content, args.tag[1:], checksum)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")
    if updated != content:
        args.cask.write_text(updated)
        print(f"Updated cask to {args.tag} ({checksum})")
    else:
        print("No cask changes needed")


if __name__ == "__main__":
    main()
