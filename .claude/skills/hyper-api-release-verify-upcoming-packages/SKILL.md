---
name: hyper-api-release-verify-upcoming-packages
description: Verify that all Tableau Hyper DB release packages on the upcoming releases page are downloadable and contain valid zip archives with the correct version number in their filenames. Use when verifying Hyper API release packages before or after publishing.
---

# Verify Hyper Releases

Verifies that every package advertised on the Tableau Hyper DB upcoming releases page is downloadable and contains a valid zip archive whose filename carries the advertised version number.

The deterministic workflow (fetch, parse, download, integrity-check, report) lives in the bundled `verify_release.py`. This SKILL.md is a thin wrapper that tells you when and how to invoke it.

## When to Use

- User asks to verify, check, or validate an upcoming Hyper API / Hyper DB release.
- User mentions the releases page at `tableau.github.io/hyper-db/upcoming/docs/releases`.
- User wants to confirm the published `.zip` / `.whl` artifacts are reachable and not corrupt, before or after publishing.

## How to Run

The script path below is relative to this SKILL.md's directory.

```bash
python3 verify_release.py [--version X.Y.ZZZZZ] [--keep]
```

### Flags

- `--version X.Y.ZZZZZ` — Expected version (e.g. `0.0.25080`). The script asserts the releases page advertises exactly this version and exits 2 on mismatch before downloading anything. Use when the user names a specific release to verify.
- `--keep` — Keep the temp download directory even when all checks pass. Use when the user wants to inspect or reuse the downloaded artifacts afterward.

The script prints progress to stdout and a markdown summary table at the end. Relay that table and the `OVERALL: PASS` / `OVERALL: FAIL` verdict back to the user verbatim. On failure, include the preserved temp directory path so the user can inspect.

## What the Script Does

1. Fetches the releases page and extracts the advertised version and every `.zip` / `.whl` download URL. With `--version`, asserts the page matches before going further.
2. Creates a temp directory for downloads.
3. For each package: checks the version string appears in the filename, downloads via Python's `urllib` (120s socket timeout, follows redirects), and verifies zip/whl archive integrity using `zipfile.testzip()`.
4. Deletes the temp directory on full success, unless `--keep` is set; keeps it and prints its path on any failure.
5. Prints a markdown summary table plus `OVERALL: PASS` or `OVERALL: FAIL`.

## Exit Codes

| Code | Meaning |
| :---: | --- |
| 0 | All packages passed every check |
| 1 | One or more checks failed (temp dir preserved for inspection) |
| 2 | Setup error: page unreachable, version not found, no download URLs, or `--version` mismatch |

## Expected Package Count

At time of writing, the page advertises 12 packages (4 platforms x 3 language bindings: Python wheel, C++ zip, Java zip). This count is advisory and **will change** as platforms or bindings are added, removed, or renamed. The script prints a warning but continues if the count differs — treat a mismatch as worth mentioning, not as a hard failure, and consider updating this section if the new count is the new steady state.

## Requirements

- Python 3.10+ (standard library only — `urllib`, `zipfile`, `argparse`, `shutil`)
- Outbound network access to `tableau.github.io` and `downloads.tableau.com`
