#!/usr/bin/env python3
"""Verify Tableau Hyper DB release packages.

Fetches the upcoming releases page, extracts the current version and all
`.zip`/`.whl` download URLs, then for each package:

  * verifies the version string appears in the filename,
  * downloads it via urllib (follows redirects, 120s timeout),
  * checks zip archive integrity (whl files are zip archives internally).

Uses only the Python standard library — no external tools required.

Prints a markdown summary table and exits:

  0 — all checks passed (temp dir deleted unless --keep)
  1 — one or more checks failed (temp dir preserved, path printed)
  2 — setup error (page unreachable, no version, or version mismatch)
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path

RELEASES_URL = "https://tableau.github.io/hyper-db/upcoming/docs/releases"
EXPECTED_LINK_COUNT = 12  # 4 platforms x 3 language bindings; advisory only
FETCH_TIMEOUT_SECONDS = 30  # small HTML page, kept separate from package downloads
DOWNLOAD_TIMEOUT_SECONDS = 120


def fetch_releases_page() -> str:
    req = urllib.request.Request(
        RELEASES_URL, headers={"User-Agent": "verify-upcoming-hyper-release/1.0"}
    )
    with urllib.request.urlopen(req, timeout=FETCH_TIMEOUT_SECONDS) as resp:
        return resp.read().decode("utf-8")


def extract_version(html: str) -> str | None:
    # The page renders "latest available version is <b>v<!-- -->0.0.NNNNN</b>",
    # so the v and the digits are split by an HTML comment in the raw source.
    # Strip HTML comments before matching.
    stripped = re.sub(r"<!--.*?-->", "", html, flags=re.DOTALL)
    m = re.search(
        r"latest available version is[^<]*<b>v(0\.0\.\d+)</b>",
        stripped,
        flags=re.IGNORECASE,
    )
    if m:
        return m.group(1)
    # Fallback: any v0.0.NNNNN after comment stripping.
    m = re.search(r"v(0\.0\.\d+)", stripped)
    return m.group(1) if m else None


def extract_download_urls(html: str) -> list[str]:
    pattern = r'https?://[^\s"\'<>]*downloads\.tableau\.com/[^\s"\'<>]+?\.(?:zip|whl)'
    seen: set[str] = set()
    urls: list[str] = []
    for url in re.findall(pattern, html):
        if url not in seen:
            seen.add(url)
            urls.append(url)
    return urls


def download_file(url: str, dest: Path) -> tuple[bool, str]:
    # urllib follows redirects by default; timeout covers the full transfer
    # only in the sense that the socket read will error if it stalls.
    req = urllib.request.Request(
        url, headers={"User-Agent": "verify-upcoming-hyper-release/1.0"}
    )
    try:
        with urllib.request.urlopen(req, timeout=DOWNLOAD_TIMEOUT_SECONDS) as resp:
            with dest.open("wb") as f:
                shutil.copyfileobj(resp, f)
        return True, ""
    except Exception as e:
        # Best-effort: remove partial file so the zip check doesn't see a stub
        dest.unlink(missing_ok=True)
        return False, str(e)


def check_zip(path: Path) -> tuple[bool, str | None]:
    try:
        with zipfile.ZipFile(path) as zf:
            bad = zf.testzip()
            if bad is None:
                return True, None
            return False, f"bad file in archive: {bad}"
    except Exception as e:
        return False, str(e)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify Tableau Hyper DB release packages are downloadable and valid."
    )
    parser.add_argument(
        "--version",
        metavar="X.Y.ZZZZZ",
        help=(
            "Expected version string (e.g. 0.0.25080). If set, the script asserts "
            "the releases page advertises this version and fails fast otherwise."
        ),
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="Keep the temp download directory even when all checks pass.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    # Step 1: fetch and parse
    print(f"Fetching {RELEASES_URL} ...")
    try:
        html = fetch_releases_page()
    except Exception as e:
        print(f"ERROR: failed to fetch releases page: {e}", file=sys.stderr)
        return 2

    version = extract_version(html)
    if not version:
        print("ERROR: could not identify version number on releases page", file=sys.stderr)
        return 2

    if args.version and args.version != version:
        print(
            f"ERROR: expected version {args.version} but releases page advertises {version}",
            file=sys.stderr,
        )
        return 2

    urls = extract_download_urls(html)
    if not urls:
        print("ERROR: no download URLs found on releases page", file=sys.stderr)
        return 2

    print(f"Version: {version}")
    print(f"Found {len(urls)} download links")
    if len(urls) != EXPECTED_LINK_COUNT:
        print(
            f"WARNING: expected {EXPECTED_LINK_COUNT} links "
            f"(4 platforms x 3 bindings), got {len(urls)}"
        )

    # Step 2: create temp dir
    tmpdir = Path(tempfile.mkdtemp(prefix="verify-hyper-"))
    print(f"Temp dir: {tmpdir}")

    # Steps 3-5: check each package
    results: list[dict[str, object]] = []
    for i, url in enumerate(urls, 1):
        filename = url.rsplit("/", 1)[-1]
        row: dict[str, object] = {"num": i, "filename": filename}

        row["version_match"] = "PASS" if version in filename else "FAIL"

        dest = tmpdir / filename
        print(f"[{i}/{len(urls)}] Downloading {filename} ...")
        ok, err = download_file(url, dest)
        row["download"] = "PASS" if ok else "FAIL"
        if not ok:
            if err:
                print(f"  download failed: {err}")
            row["zip_valid"] = "SKIP"
        else:
            ok, zip_err = check_zip(dest)
            row["zip_valid"] = "PASS" if ok else "FAIL"
            if not ok:
                print(f"  zip check failed: {zip_err}")

        results.append(row)

    # Step 6: cleanup
    all_pass = all(
        r["version_match"] == "PASS"
        and r["download"] == "PASS"
        and r["zip_valid"] == "PASS"
        for r in results
    )
    kept = not all_pass or args.keep
    if not kept:
        shutil.rmtree(tmpdir, ignore_errors=True)

    # Step 7: summary
    print()
    print("## Verification Results")
    print()
    print(f"**Version:** {version}")
    print(f"**Packages found:** {len(urls)}")
    print()
    print("| # | Package | Version Match | Download | Zip Valid |")
    print("|---|---------|:---:|:---:|:---:|")
    for r in results:
        print(
            f"| {r['num']} | {r['filename']} | "
            f"{r['version_match']} | {r['download']} | {r['zip_valid']} |"
        )
    print()
    if all_pass:
        print("**OVERALL: PASS**")
        if kept:
            print(f"Downloads preserved in: {tmpdir}")
        return 0
    print("**OVERALL: FAIL**")
    print(f"Failed files preserved in: {tmpdir}")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
