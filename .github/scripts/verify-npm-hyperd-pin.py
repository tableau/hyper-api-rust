#!/usr/bin/env python3
"""Guard: keep the npm release workflow's hyperd pin in sync with the toml.

`.github/workflows/npm-build-publish.yml` bundles `hyperd` into the npm
packages from the PyPI `tableauhyperapi` wheel, using its OWN hardcoded
`HYPERD_VERSION` plus per-platform matrix `hyperd-wheel-tag`s and
`hyperd-sha256`s. Those are decoupled from
`hyperdb-bootstrap/hyperd-version.toml`, which is what `make download-hyperd`
and the crates.io path use.

When only the toml was bumped (as in PR #237), npm silently kept shipping the
old engine: 0.7.1 bundled hyperd 0.0.25080 while crates.io shipped 0.0.26359.
This script fails CI whenever the two drift, so that can't recur silently.

The platform slug (`macos-arm64`, `linux-x86_64`, `windows-x86_64`) is the join
key: it is identical between the toml's `[wheel_tag]` / `[sha256]` tables and
the workflow's `hyperd-slug` matrix field. Only slugs the workflow actually
builds are checked, so a commented-out matrix entry (invisible to the YAML
parser) and any unused extra toml entry are both fine.

The wheel tag (`macosx_13_0_arm64`, `manylinux2014_x86_64`, ...) is checked
alongside the digest because it is a drift vector of its own: it is not
derivable from the version, and a wrong tag resolves to a URL that does not
exist, so it would surface as an opaque 404 mid-release rather than as a
mismatch. The sha256s are digests of the downloaded `.whl` archive.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
TOML = ROOT / "hyperdb-bootstrap" / "hyperd-version.toml"
WORKFLOW = ROOT / ".github" / "workflows" / "npm-build-publish.yml"


def main() -> int:
    toml_data = tomllib.loads(TOML.read_text())
    workflow = yaml.safe_load(WORKFLOW.read_text())

    env = workflow.get("env", {})
    toml_wheel_tag = toml_data.get("wheel_tag", {})
    toml_sha = toml_data.get("sha256", {})

    # (label, expected-from-toml, actual-from-workflow)
    checks: list[tuple[str, str, str | None]] = [
        ("HYPERD_VERSION", str(toml_data["version"]), env.get("HYPERD_VERSION")),
    ]

    errors: list[str] = []

    include = workflow["jobs"]["build-npm"]["strategy"]["matrix"]["include"]
    for entry in include:
        slug = entry.get("hyperd-slug")
        if slug is None:
            continue
        expected_tag = toml_wheel_tag.get(slug)
        if expected_tag is None:
            errors.append(
                f'matrix slug "{slug}" has no [wheel_tag]."{slug}" entry in {TOML.name}'
            )
        else:
            checks.append(
                (f"wheel_tag[{slug}]", expected_tag, entry.get("hyperd-wheel-tag"))
            )
        expected_sha = toml_sha.get(slug)
        if expected_sha is None:
            errors.append(
                f'matrix slug "{slug}" has no [sha256]."{slug}" entry in {TOML.name}'
            )
            continue
        checks.append((f"sha256[{slug}]", expected_sha, entry.get("hyperd-sha256")))

    for label, expected, actual in checks:
        if actual == expected:
            print(f"ok: {label} = {expected}")
        else:
            errors.append(f"{label}: workflow has {actual!r}, toml has {expected!r}")

    if errors:
        print()
        for err in errors:
            print(f"::error::hyperd pin drift — {err}")
        sys.stdout.flush()
        print(
            f"\n{WORKFLOW.name} is out of sync with {TOML.name}. "
            "Update the workflow's env vars and matrix wheel tags / sha256s to "
            "match the toml (or vice versa) so npm bundles the same hyperd as "
            "crates.io.",
            file=sys.stderr,
        )
        return 1

    print(f"\n{WORKFLOW.name} hyperd pin matches {TOML.name}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
