import tempfile
import unittest
from pathlib import Path

from scripts.generate_release_notes import Commit, generate_release_notes


class GenerateReleaseNotesTests(unittest.TestCase):
    def render(self, commits=None, previous_tag="v0.1.7"):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = Path(tmp)
            for name in [
                "stream-server-windows-amd64.exe",
                "stream-server-linux-amd64.deb",
                "SHA256SUMS.txt",
            ]:
                (asset_dir / name).write_text("asset", encoding="utf-8")
            return generate_release_notes(
                repo="perpetus/stream-server",
                tag="v0.1.8",
                previous_tag=previous_tag,
                asset_dir=asset_dir,
                commits=commits or [],
            )

    def test_generates_clickable_links_for_staged_assets(self):
        body = self.render()

        self.assertIn("| Windows portable | [Download](https://github.com/perpetus/stream-server/releases/download/v0.1.8/stream-server-windows-amd64.exe) |", body)
        self.assertIn("| Debian / Ubuntu | [Download](https://github.com/perpetus/stream-server/releases/download/v0.1.8/stream-server-linux-amd64.deb) |", body)
        self.assertIn("| Checksums | [Download](https://github.com/perpetus/stream-server/releases/download/v0.1.8/SHA256SUMS.txt) |", body)

    def test_excludes_literal_glob_conventions(self):
        body = self.render()

        self.assertNotIn("*.msi", body)
        self.assertNotIn("*.deb", body)
        self.assertNotIn("*.AppImage", body)
        self.assertNotIn("*.pkg.tar.zst", body)

    def test_categorizes_commit_messages(self):
        body = self.render(
            commits=[
                Commit("1111111111111111111111111111111111111111", "feat: add updater"),
                Commit("2222222222222222222222222222222222222222", "fix: repair startup"),
                Commit("3333333333333333333333333333333333333333", "perf: speed up pieces"),
                Commit("4444444444444444444444444444444444444444", "style: polish settings"),
                Commit("5555555555555555555555555555555555555555", "fix(hls): keep segments active"),
                Commit("6666666666666666666666666666666666666666", "feat(settings-gui): show changelog"),
                Commit("7777777777777777777777777777777777777777", "docs: update readme"),
                Commit("8888888888888888888888888888888888888888", "ci: speed up release"),
                Commit("9999999999999999999999999999999999999999", "test: add coverage"),
                Commit("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "misc cleanup"),
            ]
        )

        for heading in [
            "#### Features & Enhancements",
            "#### Bug Fixes",
            "#### Performance",
            "#### UI",
            "#### Streaming",
            "#### Settings",
            "#### Documentation",
            "#### CI & Build",
            "#### Maintenance",
            "#### Other Changes",
        ]:
            self.assertIn(heading, body)

        self.assertIn("- add updater ([1111111](https://github.com/perpetus/stream-server/commit/1111111111111111111111111111111111111111))", body)
        self.assertIn("- keep segments active ([5555555](https://github.com/perpetus/stream-server/commit/5555555555555555555555555555555555555555))", body)

    def test_omits_empty_sections(self):
        body = self.render(
            commits=[
                Commit("1111111111111111111111111111111111111111", "fix: repair startup"),
            ]
        )

        self.assertIn("#### Bug Fixes", body)
        self.assertNotIn("#### Features & Enhancements", body)

    def test_handles_no_previous_tag_as_initial_release(self):
        body = self.render(previous_tag="")

        self.assertIn("### Changelog", body)
        self.assertIn("Initial release.", body)
        self.assertNotIn("Full comparison", body)

    def test_includes_unknown_extra_assets(self):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = Path(tmp)
            (asset_dir / "custom-extra.zip").write_text("asset", encoding="utf-8")
            body = generate_release_notes(
                repo="perpetus/stream-server",
                tag="v0.1.8",
                previous_tag="v0.1.7",
                asset_dir=asset_dir,
                commits=[],
            )

        self.assertIn("| Additional asset | [Download](https://github.com/perpetus/stream-server/releases/download/v0.1.8/custom-extra.zip) |", body)


if __name__ == "__main__":
    unittest.main()
