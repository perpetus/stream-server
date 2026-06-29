#!/usr/bin/env python3
"""Generate Stream Server release notes from staged assets and git history."""

from __future__ import annotations

import argparse
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


PROJECT_DESCRIPTION = """High-performance torrent streaming server with HLS transcoding and a
Stremio-compatible streaming API. Features disk-backed downloads,
hardware-accelerated transcoding, and an embedded HTTP server."""


ASSET_LABELS = {
    "stream-server-windows-amd64.exe": "Windows portable",
    "stream-server-settings-windows-amd64.exe": "Windows settings GUI",
    "stream-server-windows-amd64.msi": "Windows installer",
    "stremio-runtime-windows-amd64.exe": "Windows Stremio runtime",
    "stream-server-updater-windows-amd64.exe": "Windows updater",
    "stream-server-linux-amd64.deb": "Debian / Ubuntu",
    "stream-server-linux-amd64": "Linux portable",
    "stream-server-settings-linux-amd64": "Linux settings GUI",
    "stream-server-linux-amd64.AppImage": "Linux AppImage",
    "stream-server-arch-x86_64.pkg.tar.zst": "Arch Linux",
    "SHA256SUMS.txt": "Checksums",
}


CATEGORY_ORDER = [
    "Features & Enhancements",
    "Bug Fixes",
    "Performance",
    "UI",
    "Streaming",
    "Settings",
    "Documentation",
    "CI & Build",
    "Maintenance",
    "Other Changes",
]


TYPE_CATEGORIES = {
    "feat": "Features & Enhancements",
    "feature": "Features & Enhancements",
    "fix": "Bug Fixes",
    "bugfix": "Bug Fixes",
    "perf": "Performance",
    "ui": "UI",
    "style": "UI",
    "docs": "Documentation",
    "doc": "Documentation",
    "ci": "CI & Build",
    "build": "CI & Build",
    "release": "CI & Build",
    "refactor": "Maintenance",
    "chore": "Maintenance",
    "test": "Maintenance",
}


SCOPE_CATEGORIES = {
    "stream": "Streaming",
    "hls": "Streaming",
    "torrent": "Streaming",
    "enginefs": "Streaming",
    "settings": "Settings",
    "settings-gui": "Settings",
    "config": "Settings",
}


CONVENTIONAL_RE = re.compile(
    r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^)]+)\))?(?:!)?:\s*(?P<message>.+)$"
)


@dataclass(frozen=True)
class Commit:
    sha: str
    subject: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="GitHub repository, e.g. owner/repo")
    parser.add_argument("--tag", required=True, help="Release tag being generated")
    parser.add_argument(
        "--previous-tag",
        default="",
        help="Previous release tag. Empty means this is the initial release.",
    )
    parser.add_argument("--asset-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def git_log(previous_tag: str, tag: str) -> list[Commit]:
    if not previous_tag:
        return []

    result = subprocess.run(
        ["git", "log", f"{previous_tag}..{tag}", "--format=%H%x1f%s"],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    commits: list[Commit] = []
    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        sha, _, subject = line.partition("\x1f")
        if sha and subject:
            commits.append(Commit(sha=sha, subject=subject))
    return commits


def strip_conventional_prefix(subject: str) -> str:
    match = CONVENTIONAL_RE.match(subject)
    if not match:
        return subject.strip()
    return match.group("message").strip()


def categorize(subject: str) -> str:
    match = CONVENTIONAL_RE.match(subject)
    if not match:
        return "Other Changes"

    commit_type = match.group("type").lower()
    scope = (match.group("scope") or "").lower()
    if scope in SCOPE_CATEGORIES:
        return SCOPE_CATEGORIES[scope]
    return TYPE_CATEGORIES.get(commit_type, "Other Changes")


def sorted_assets(asset_dir: Path) -> list[Path]:
    if not asset_dir.exists():
        return []

    def sort_key(path: Path) -> tuple[int, str]:
        try:
            order = list(ASSET_LABELS).index(path.name)
        except ValueError:
            order = len(ASSET_LABELS)
        return (order, path.name.lower())

    return sorted((path for path in asset_dir.iterdir() if path.is_file()), key=sort_key)


def asset_label(name: str) -> str:
    return ASSET_LABELS.get(name, "Additional asset")


def github_download_url(repo: str, tag: str, name: str) -> str:
    return f"https://github.com/{repo}/releases/download/{tag}/{name}"


def github_commit_url(repo: str, sha: str) -> str:
    return f"https://github.com/{repo}/commit/{sha}"


def render_downloads(repo: str, tag: str, assets: Iterable[Path]) -> list[str]:
    lines = [
        "### Downloads",
        "",
        "| Platform | Download |",
        "| --- | --- |",
    ]
    for asset in assets:
        name = asset.name
        lines.append(f"| {asset_label(name)} | [Download]({github_download_url(repo, tag, name)}) |")
    lines.extend(["", "Verify your download against `SHA256SUMS.txt`.", "", "---", ""])
    return lines


def grouped_commits(commits: Iterable[Commit]) -> dict[str, list[Commit]]:
    grouped = {category: [] for category in CATEGORY_ORDER}
    for commit in commits:
        grouped[categorize(commit.subject)].append(commit)
    return grouped


def render_changelog(repo: str, tag: str, previous_tag: str, commits: list[Commit]) -> list[str]:
    if not previous_tag:
        return ["### Changelog", "", "Initial release."]

    lines = [f"### Changelog (since {previous_tag})", ""]
    grouped = grouped_commits(commits)
    for category in CATEGORY_ORDER:
        entries = grouped[category]
        if not entries:
            continue
        lines.extend([f"#### {category}", ""])
        for commit in entries:
            short = commit.sha[:7]
            message = strip_conventional_prefix(commit.subject)
            lines.append(f"- {message} ([{short}]({github_commit_url(repo, commit.sha)}))")
        lines.append("")

    lines.append(f"**Full comparison**: https://github.com/{repo}/compare/{previous_tag}...{tag}")
    return lines


def generate_release_notes(
    repo: str,
    tag: str,
    previous_tag: str,
    asset_dir: Path,
    commits: list[Commit] | None = None,
) -> str:
    commits = git_log(previous_tag, tag) if commits is None else commits
    lines = [f"## Stream Server {tag}", "", PROJECT_DESCRIPTION, ""]
    lines.extend(render_downloads(repo, tag, sorted_assets(asset_dir)))
    lines.extend(render_changelog(repo, tag, previous_tag, commits))
    return "\n".join(lines).rstrip() + "\n"


def main() -> None:
    args = parse_args()
    body = generate_release_notes(
        repo=args.repo,
        tag=args.tag,
        previous_tag=args.previous_tag.strip(),
        asset_dir=args.asset_dir,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(body, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
