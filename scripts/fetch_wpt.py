#!/usr/bin/env python3
"""
Fetch web-platform-tests on demand (shallow clone, tracks upstream master).

WPT is a large upstream mirror (~500 MB, 500k+ files) that aris's wpt_runner
consumes. It used to be a git submodule; it is now cloned on demand so it
costs nothing until a developer needs to run the WPT suite.

Clones directly into tests/wpt/wpt-master/ (in place, not via a temp dir +
sync — WPT is too large for a full content walk on every fetch). A second run
pulls updates instead of re-cloning.

Usage:
    python scripts/fetch_wpt.py
    python scripts/fetch_wpt.py --local /path/to/a/wpt-checkout
    python scripts/fetch_wpt.py --branch master
"""

import argparse
import subprocess
import sys
from pathlib import Path

WPT_REPO = "https://github.com/web-platform-tests/wpt.git"
DEFAULT_BRANCH = "master"
TARGET_DIR = Path(__file__).resolve().parent.parent / "tests" / "wpt" / "wpt-master"


def _run(cmd: list[str], cwd: Path | None = None) -> None:
    """Run a command, exiting with its status on failure."""
    print(f"[run] {' '.join(cmd)}" + (f"  (cwd={cwd})" if cwd else ""))
    subprocess.run(cmd, cwd=cwd, check=True)


def _sync_directory(src: Path, dst: Path) -> int:
    """Incrementally sync src/ into dst/.

    Only overwrites files whose contents differ, preserving mtimes for
    unchanged files. Removes files in dst that no longer exist in src.
    Returns the number of files actually written.
    """
    written = 0

    src_files: dict[Path, bytes] = {}
    for f in src.rglob("*"):
        if f.is_file():
            src_files[f.relative_to(src)] = f.read_bytes()

    if dst.exists():
        for f in dst.rglob("*"):
            if f.is_file() and f.relative_to(dst) not in src_files:
                f.unlink()
        for d in sorted(dst.rglob("*"), reverse=True):
            if d.is_dir() and not any(d.iterdir()):
                d.rmdir()

    for rel, content in src_files.items():
        dst_file = dst / rel
        if dst_file.exists() and dst_file.read_bytes() == content:
            continue
        dst_file.parent.mkdir(parents=True, exist_ok=True)
        dst_file.write_bytes(content)
        written += 1

    return written


def fetch_via_git(target_dir: Path, branch: str) -> None:
    """Clone WPT into target_dir (shallow), or pull updates if already present."""
    if (target_dir / ".git").is_dir():
        print(f"[INFO] {target_dir} exists — pulling latest from {WPT_REPO}")
        _run(["git", "fetch", "origin", branch], cwd=target_dir)
        _run(["git", "reset", "--hard", f"origin/{branch}"], cwd=target_dir)
        print(f"[OK] Updated to {subprocess.check_output(['git', 'rev-parse', '--short', 'HEAD'], cwd=target_dir, text=True).strip()}")
        return

    target_dir.parent.mkdir(parents=True, exist_ok=True)
    print(f"[INFO] Shallow-cloning {WPT_REPO} (branch {branch}) into {target_dir} ...")
    _run(["git", "clone", "--depth", "1", "--branch", branch, WPT_REPO, str(target_dir)])
    print(f"[OK] Cloned at {subprocess.check_output(['git', 'rev-parse', '--short', 'HEAD'], cwd=target_dir, text=True).strip()}")


def copy_from_local(source_dir: Path, target_dir: Path) -> None:
    """Sync from a local WPT checkout (incremental, no git clone)."""
    if not source_dir.is_dir():
        sys.exit(f"[ERROR] local source does not exist: {source_dir}")
    print(f"[INFO] Syncing from local {source_dir} -> {target_dir}")
    written = _sync_directory(source_dir, target_dir)
    total = sum(1 for _ in target_dir.rglob("*") if _.is_file()) if target_dir.exists() else 0
    print(f"[OK] Synced {written}/{total} files changed (incremental)")


def main() -> None:
    parser = argparse.ArgumentParser(description="Fetch web-platform-tests on demand")
    parser.add_argument(
        "--local",
        type=Path,
        default=None,
        help="Path to a local WPT checkout (skips git clone; incremental sync)",
    )
    parser.add_argument(
        "--branch",
        type=str,
        default=DEFAULT_BRANCH,
        help=f"Branch to fetch (default: {DEFAULT_BRANCH})",
    )
    args = parser.parse_args()

    # `just fetch-wpt /some/path` passes a bare positional into LOCAL; treat a
    # leading non-flag arg as --local for ergonomic invocation. Filter out
    # empty strings (just expands an unset `LOCAL=""` to a trailing space).
    extras = [a for a in sys.argv[1:] if a and not a.startswith("-")]
    if extras and args.local is None:
        args.local = Path(extras[0])

    if args.local:
        copy_from_local(args.local, TARGET_DIR)
    else:
        fetch_via_git(TARGET_DIR, args.branch)

    print(f"\n  wpt_runner default path: {TARGET_DIR / 'dom'}")
    print('  run with: cargo run -p aris-render --features "desktop winit js" --bin wpt_runner')


if __name__ == "__main__":
    main()
