#!/usr/bin/env python3
"""A deliberately fake CLI, used ONLY to self-test `run.py`.

This is not the product and must never be shipped or mistaken for it. It exists
because `spec/acceptance/run.py` had never been run against anything: if the
harness itself has a bug in its argument handling, its mode switching, or its
pass/fail logic, that bug would otherwise surface the moment the real CLI
landed, wasting the very round the real CLI was built in.

So this fixture implements just enough of `spec/cli-v1.md` to drive the harness:

    fake_cli.py add <url> <dest>     resume from a partial file if one exists
    fake_cli.py resume --all         same, for every recorded task
    fake_cli.py list
    fake_cli.py status <id>

It speaks HTTP with `Range` and honours `CHUA_DATA_DIR`, which is all the harness
needs. It deliberately does NOT implement the engine's behaviour — in particular
it has no §6.3 guard, so the `range-ignored` check SHOULD fail against it. **That
failure is the fixture working correctly**: it proves the harness can detect the
corruption case rather than passing everything.

Usage:
    python run.py --cli "python fake_cli.py"
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

EXIT_OK = 0
EXIT_USAGE = 1
EXIT_TASK_FAILED = 2
EXIT_NOT_FOUND = 3
EXIT_CONFLICT = 4


def state_dir() -> Path:
    base = os.environ.get("CHUA_DATA_DIR")
    path = Path(base) if base else Path.cwd() / ".fake-cli-state"
    path.mkdir(parents=True, exist_ok=True)
    return path


def load() -> dict:
    f = state_dir() / "tasks.json"
    if f.exists():
        try:
            return json.loads(f.read_text())
        except Exception:  # noqa: BLE001
            return {}
    return {}


def save(data: dict) -> None:
    (state_dir() / "tasks.json").write_text(json.dumps(data))


SEGMENTS = 4


def _head_size(url: str) -> int | None:
    request = urllib.request.Request(url, method="HEAD")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            length = response.headers.get("Content-Length")
            return int(length) if length else None
    except Exception:  # noqa: BLE001
        return None


def _fetch(url: str, start: int | None, end: int | None):
    request = urllib.request.Request(url)
    if start is not None:
        request.add_header("Range", f"bytes={start}-{end}")
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.status, response.headers.get("Content-Range"), response.read()


def _write_at(dest: Path, offset: int, body: bytes) -> None:
    with dest.open("r+b" if dest.exists() else "wb") as handle:
        handle.seek(offset)
        handle.write(body)


def download(url: str, dest: Path) -> int:
    """Fetch `url` into `dest` using several ranged requests, like the real engine.

    Multi-segment is not decoration: it is the only shape in which the bug this
    suite hunts actually appears. A sequential downloader that asks for the whole
    file and receives `200` is *correct*; the corruption happens when a request
    for `bytes=N-` is answered with the whole representation and the body is
    written at offset `N`.

    `FAKE_CLI_GUARD=1` enables the §6.3 validation. It is off by default on
    purpose — the suite has to be shown to fail on a careless downloader before
    its passing on a careful one proves anything.
    """
    guard = os.environ.get("FAKE_CLI_GUARD") == "1"
    total = _head_size(url)

    if total is None:
        # Unknown length (spec §4.3): one whole-file request, and a `200` is the
        # correct answer for it. A guard that rejected this would be wrong.
        status, _, body = _fetch(url, None, None)
        if status != 200:
            print(f"unexpected status {status} for a whole-file request", file=sys.stderr)
            return EXIT_TASK_FAILED
        dest.write_bytes(body)
        return EXIT_OK

    span = max(1, total // SEGMENTS)

    # Preallocate to the expected size, as any real downloader does so that
    # segments can be written at their offsets. This is also what turns a
    # mishandled `200` into *silent* corruption rather than an obviously
    # oversized file: the result is exactly the right length and entirely wrong.
    with dest.open("wb") as handle:
        handle.truncate(total)

    try:
        for start in range(0, total, span):
            end = min(start + span - 1, total - 1)
            status, content_range, body = _fetch(url, start, end)

            if status == 200:
                # The server ignored the range request and sent everything.
                if guard:
                    print(
                        f"server ignored the range request for bytes={start}-{end} "
                        f"(200 with {len(body)} bytes)",
                        file=sys.stderr,
                    )
                    return EXIT_TASK_FAILED
                # Careless, and the most plausible way to be careless: "I asked
                # for `span` bytes, so I will take the first `span` bytes of the
                # response." The file keeps exactly the right length, every
                # checksum disagrees, and nothing reports an error. This is the
                # corruption the acceptance suite exists to catch.
                _write_at(dest, start, body[: end - start + 1])
                continue

            if guard:
                expected = f"bytes {start}-{start + len(body) - 1}/"
                if not content_range or not content_range.startswith(expected):
                    print(
                        f"range mismatch: asked for bytes={start}-{end}, "
                        f"got {content_range!r}",
                        file=sys.stderr,
                    )
                    return EXIT_TASK_FAILED
            _write_at(dest, start, body)
    except urllib.error.HTTPError as error:
        print(f"http error {error.code}", file=sys.stderr)
        return EXIT_TASK_FAILED
    except Exception as error:  # noqa: BLE001
        print(f"transport error: {error}", file=sys.stderr)
        return EXIT_TASK_FAILED

    if dest.stat().st_size != total:
        print(f"short file: {dest.stat().st_size} of {total}", file=sys.stderr)
        return EXIT_TASK_FAILED
    return EXIT_OK


def cmd_add(args: list[str]) -> int:
    if not args:
        return EXIT_USAGE
    url = args[0]
    dest = Path(args[1]) if len(args) > 1 else Path(url.rsplit("/", 1)[-1])
    data = load()
    data[str(dest.resolve())] = url
    save(data)
    return download(url, dest)


def cmd_resume_all() -> int:
    worst = EXIT_OK
    for dest, url in load().items():
        path = Path(dest)
        if path.exists() and path.stat().st_size > 0:
            code = download(url, path)
            if code != EXIT_OK:
                worst = code
    return worst


def main() -> int:
    argv = sys.argv[1:]
    if not argv:
        return EXIT_USAGE
    command, rest = argv[0], argv[1:]
    if command == "add":
        return cmd_add(rest)
    if command == "resume":
        return cmd_resume_all()
    if command == "list":
        for dest in load():
            print(dest)
        return EXIT_OK
    if command == "status":
        return EXIT_OK if rest and rest[0] in load() else EXIT_NOT_FOUND
    if command in {"pause", "remove"}:
        return EXIT_OK
    return EXIT_USAGE


if __name__ == "__main__":
    sys.exit(main())
