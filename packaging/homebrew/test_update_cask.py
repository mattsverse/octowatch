import hashlib
import json
from pathlib import Path
import unittest
from unittest.mock import patch

import update_cask


class CaskUpdateTests(unittest.TestCase):
    checksum = "a" * 64
    content = ('cask "octowatcher" do\n  version "0.6.0"\n'
               f'  sha256 "{"a" * 64}"\n'
               '  depends_on formula: "gh"\n  app "Octowatcher.app"\nend\n')

    def test_upgrade_preserves_everything_except_version_and_checksum(self):
        updated = update_cask.update_cask(self.content, "0.7.0", "b" * 64)
        self.assertEqual(updated, self.content.replace('"0.6.0"', '"0.7.0"')
                         .replace(self.checksum, "b" * 64))

    def test_numeric_version_order_prevents_rollback(self):
        newer = self.content.replace('"0.6.0"', '"0.10.0"')
        self.assertEqual(update_cask.update_cask(newer, "0.9.0", "b" * 64), newer)

    def test_retry_is_idempotent_but_replaced_release_is_rejected(self):
        self.assertEqual(update_cask.update_cask(self.content, "0.6.0", self.checksum), self.content)
        with self.assertRaisesRegex(ValueError, "changed for the same version"):
            update_cask.update_cask(self.content, "0.6.0", "b" * 64)

    def test_rejects_prereleases_and_ambiguous_cask_stanzas(self):
        for version in ("0.7.0-rc.1", "0.7.0+build", "01.0.0", "latest"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                update_cask.update_cask(self.content, version, self.checksum)
        with self.assertRaises(ValueError):
            update_cask.update_cask(self.content + '  version "0.8.0"\n', "0.7.0", self.checksum)

    def release(self, **overrides):
        data = {"tagName": "v0.7.0", "isDraft": False, "isPrerelease": False,
                "assets": [{"name": "Octowatcher.dmg",
                            "url": "https://github.com/mattsverse/octowatch/releases/download/v0.7.0/Octowatcher.dmg",
                            "digest": "sha256:" + hashlib.sha256(b"DMG bytes").hexdigest()}]}
        return data | overrides

    def test_unpublished_or_incomplete_releases_never_download(self):
        for overrides in ({"isDraft": True}, {"isPrerelease": True},
                          {"tagName": "v0.8.0"}, {"assets": []}):
            with self.subTest(overrides=overrides), patch.object(
                    update_cask, "gh", return_value=json.dumps(self.release(**overrides))) as gh:
                with self.assertRaises(ValueError):
                    update_cask.release_checksum("v0.7.0")
                self.assertEqual(gh.call_count, 1)

    def test_checksum_comes_from_download_and_detects_corruption(self):
        for downloaded in (b"DMG bytes", b"corrupted bytes"):
            def fake_gh(*args):
                if args[1] == "view":
                    return json.dumps(self.release())
                directory = args[args.index("--dir") + 1]
                (Path(directory) / "Octowatcher.dmg").write_bytes(downloaded)
                return ""

            with self.subTest(downloaded=downloaded), patch.object(update_cask, "gh", side_effect=fake_gh):
                if downloaded == b"DMG bytes":
                    self.assertEqual(update_cask.release_checksum("v0.7.0"),
                                     hashlib.sha256(downloaded).hexdigest())
                else:
                    with self.assertRaisesRegex(ValueError, "asset digest"):
                        update_cask.release_checksum("v0.7.0")


if __name__ == "__main__":
    unittest.main()
