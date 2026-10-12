#!/usr/bin/env python3
"""Check every dependency's licence against the ADR-0005 / ADR-0008 allow-list.

CI runs `cargo-deny` with `deny.toml`. This script exists for the case where
cargo-deny is not installed — it needs only a populated cargo registry cache and
a `Cargo.lock`, so it works offline and takes about a second.

It answers one question: *does the allow-list actually cover this dependency
tree?* That is worth asking locally, because discovering a gap in CI (or worse,
in a release) is expensive, and because a licence expression is easy to get
subtly wrong — `MIT OR Apache-2.0` and `MIT AND Apache-2.0` are not the same
thing and both appear in the wild.

Usage:
    python scripts/check-licences.py [--lock Cargo.lock] [--deny deny.toml]
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

DEFAULT_ALLOW = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "Unlicense",
    "BSL-1.0",
    "CC0-1.0",
    "Unicode-3.0",
    "MPL-2.0",
]


def load_allow(deny_path: Path | None) -> list[str]:
    if deny_path is None or not deny_path.exists():
        return DEFAULT_ALLOW
    try:
        with deny_path.open("rb") as handle:
            data = tomllib.load(handle)
        return list(data.get("licenses", {}).get("allow", DEFAULT_ALLOW))
    except Exception as error:  # noqa: BLE001 - fall back loudly, never silently
        print(f"warning: could not read {deny_path} ({error}); using built-in list")
        return DEFAULT_ALLOW


def tokens(expression: str) -> list[str]:
    return [t for t in re.split(r"\s+", expression.strip()) if t]


def permitted(expression: str, allow: list[str]) -> bool:
    """Evaluate an SPDX-ish licence expression against the allow-list.

    `OR` means the consumer may pick, so one acceptable branch is enough.
    `AND` (and `/`, which some crates use) means every branch applies.
    """
    allow_set = {a.lower() for a in allow}

    expr = expression.replace("(", " ").replace(")", " ")
    parts = re.split(r"\s+(?:OR|AND)\s+|\s*/\s*", expr, flags=re.IGNORECASE)
    operators = re.findall(r"\s+(OR|AND)\s+|(\s*/\s*)", expr, flags=re.IGNORECASE)

    branches = [p.strip().lower() for p in parts if p.strip()]
    if not branches:
        return False

    and_like = bool(operators) and all(
        op[0].upper() == "AND" or op[1] for op in operators
    )
    if and_like:
        return all(b in allow_set for b in branches)
    return any(b in allow_set for b in branches)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lock", default="Cargo.lock")
    parser.add_argument("--deny", default="deny.toml")
    parser.add_argument("--registry", default=None)
    parser.add_argument(
        "--list-unchecked",
        action="store_true",
        help="print the crates that could not be checked (other targets)",
    )
    args = parser.parse_args()
    list_unchecked = args.list_unchecked

    allow = load_allow(Path(args.deny) if args.deny else None)
    lock = Path(args.lock)
    if not lock.exists():
        print(f"no {lock}; nothing to check")
        return 0

    with lock.open("rb") as handle:
        packages = tomllib.load(handle).get("package", [])

    registry = (
        Path(args.registry)
        if args.registry
        else Path.home() / ".cargo" / "registry" / "src"
    )
    roots = [p for p in registry.glob("*") if p.is_dir()] if registry.exists() else []

    checked = 0
    unknown: list[tuple[str, str]] = []
    bad: list[tuple[str, str]] = []
    missing_cache: list[str] = []

    for package in packages:
        name = package.get("name", "")
        version = package.get("version", "")
        source = package.get("source", "")
        if "registry" not in source:
            # Path and git dependencies carry their licence in their own manifest.
            continue

        manifest = None
        for root in roots:
            candidate = root / f"{name}-{version}" / "Cargo.toml"
            if candidate.exists():
                manifest = candidate
                break
        if manifest is None:
            missing_cache.append(f"{name}-{version}")
            continue

        try:
            with manifest.open("rb") as handle:
                data = tomllib.load(handle)
        except Exception:  # noqa: BLE001
            missing_cache.append(f"{name}-{version}")
            continue

        expression = data.get("package", {}).get("license")
        if expression is None:
            unknown.append((name, version))
            continue

        checked += 1
        if not permitted(expression, allow):
            bad.append((f"{name}-{version}", expression))

    print(f"checked {checked} crates from {lock}")
    if missing_cache:
        # These are almost always crates for other targets (macOS, Android, wasm,
        # other Windows architectures). They appear in Cargo.lock, which is
        # target-independent, but their sources are never downloaded for the
        # target being built — so this script cannot see their manifests.
        # CI's cargo-deny works from the whole lock file and does cover them.
        print(
            f"not built for this target, so not checked here: {len(missing_cache)}"
        )
        print("  (CI's cargo-deny covers these; run with --list-unchecked to see them)")
        if list_unchecked:
            for name in sorted(missing_cache):
                print(f"    {name}")
    if unknown:
        print(f"\nNO LICENCE FIELD ({len(unknown)}):")
        for name, version in unknown:
            print(f"  {name}-{version}")
    if bad:
        print(f"\nNOT ON THE ALLOW-LIST ({len(bad)}):")
        for name, expression in bad:
            print(f"  {name}: {expression}")
        print("\nEither add the licence (with an ADR) or drop the dependency.")
        return 1

    print("\nok: every checked crate is inside the allow-list")
    return 0


if __name__ == "__main__":
    sys.exit(main())
