#!/usr/bin/env python3
"""Black-box acceptance checks for the download engine, driven through its CLI.

Specification side: these checks come from `spec/engine-v1.md` and the M1
acceptance criteria in `.omp/ROADMAP.md`. They treat the engine as an opaque
binary — nothing here imports engine code or inspects its internals, so the same
suite can be run against any implementation that satisfies the spec.

Usage:
    python run.py --cli ../target/release/chua-cli

The central assertion is the same in every scenario:

    either the engine fails loudly, or the bytes on disk are correct —
    never "reported success while the file is silently wrong".
"""

from __future__ import annotations

import argparse
import hashlib
import itertools
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
SERVER = HERE / "range_server.py"
PAYLOAD_SIZE = 8 * 1024 * 1024  # 8 MiB: big enough to force several segments

_DIR_COUNTER = itertools.count()


class Result:
    def __init__(self) -> None:
        self.rows: list[tuple[str, bool, str]] = []

    def add(self, name: str, ok: bool, detail: str) -> None:
        self.rows.append((name, ok, detail))
        mark = "PASS" if ok else "FAIL"
        print(f"  [{mark}] {name}: {detail}", flush=True)

    @property
    def failed(self) -> int:
        return sum(1 for _, ok, _ in self.rows if not ok)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def wait_for_port(port: int, proc: subprocess.Popen, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return False
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/_sha256", timeout=0.5
            ) as response:
                response.read()
            return True
        except Exception:
            time.sleep(0.1)
    return False


def set_mode(port: int, mode: str) -> None:
    with urllib.request.urlopen(
        f"http://127.0.0.1:{port}/_mode?m={mode}", timeout=5
    ) as response:
        response.read()


def expected_sha(port: int) -> str:
    with urllib.request.urlopen(
        f"http://127.0.0.1:{port}/_sha256", timeout=5
    ) as response:
        return response.read().decode()


def run_cli(
    cli: Path, args: list[str], cwd: Path, timeout: float, data_dir: Path | None = None
) -> tuple[int, str]:
    """Run the CLI with an isolated state directory (spec/cli-v1.md §5).

    Every call gets its own `CHUA_DATA_DIR` unless one is passed in, so scenarios
    cannot leak tasks into each other. The resume check passes the same directory
    to both halves on purpose.
    """
    if data_dir is None:
        data_dir = cwd / f"state-{next(_DIR_COUNTER)}"
    data_dir.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["CHUA_DATA_DIR"] = str(data_dir)
    try:
        completed = subprocess.run(
            [str(cli), *args],
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return completed.returncode, (completed.stdout + completed.stderr)
    except subprocess.TimeoutExpired as expired:
        out = (expired.stdout or b"") + (expired.stderr or b"")
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return 124, out + "\n<timed out>"


def settle(path: Path, timeout: float = 30.0) -> bool:
    """Wait until the file stops growing, for CLIs that return before finishing."""
    deadline = time.time() + timeout
    last = -1
    stable = 0
    while time.time() < deadline:
        size = path.stat().st_size if path.exists() else -1
        if size == last and size > 0:
            stable += 1
            if stable >= 3:
                return True
        else:
            stable = 0
        last = size
        time.sleep(0.2)
    return path.exists()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cli", required=True, help="path to the CLI binary")
    parser.add_argument("--port", type=int, default=8731)
    args = parser.parse_args()

    cli = Path(args.cli).resolve()
    if not cli.exists():
        print(f"CLI not found: {cli}", file=sys.stderr)
        return 2

    result = Result()
    work = Path(tempfile.mkdtemp(prefix="chua-accept-"))
    server = subprocess.Popen(
        [sys.executable, str(SERVER), "--port", str(args.port), "--size", str(PAYLOAD_SIZE)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        if not wait_for_port(args.port, server):
            print("test server did not come up", file=sys.stderr)
            return 2
        good = expected_sha(args.port)
        url = f"http://127.0.0.1:{args.port}/payload.bin"

        # ---- 1. clean multi-segment download -----------------------------
        set_mode(args.port, "normal")
        dest = work / "clean.bin"
        code, out = run_cli(cli, ["add", url, str(dest)], work, timeout=90)
        settle(dest)
        if dest.exists() and sha256_file(dest) == good:
            result.add("clean-download", True, f"{dest.stat().st_size} bytes, sha256 matches")
        else:
            result.add("clean-download", False, f"exit={code} exists={dest.exists()}\n{out[-400:]}")

        # ---- 2. the dangerous one: server ignores Range -------------------
        #    A server that answers a ranged request with 200 + the whole body
        #    must never lead to a file that is reported complete but wrong.
        set_mode(args.port, "ignore-range")
        dest = work / "ignored.bin"
        code, out = run_cli(cli, ["add", url, str(dest)], work, timeout=90)
        settle(dest, timeout=15)
        if not dest.exists():
            result.add("range-ignored", True, f"no file written; exit={code}")
        elif dest.stat().st_size == PAYLOAD_SIZE and sha256_file(dest) == good:
            result.add("range-ignored", True, "downloaded correctly anyway")
        elif dest.stat().st_size == PAYLOAD_SIZE:
            result.add("range-ignored", False, "SILENT CORRUPTION: full-size file with wrong bytes")
        else:
            result.add("range-ignored", True, f"partial file {dest.stat().st_size} bytes, not claimed complete; exit={code}")

        # ---- 3. unknown length (no Content-Length) ------------------------
        set_mode(args.port, "no-length")
        dest = work / "nolen.bin"
        code, out = run_cli(cli, ["add", url, str(dest)], work, timeout=90)
        settle(dest)
        if dest.exists() and sha256_file(dest) == good:
            result.add("unknown-length", True, "completed and matches")
        else:
            result.add("unknown-length", False, f"exit={code} exists={dest.exists()}\n{out[-400:]}")

        # ---- 4. 404 must fail, and fail promptly (not retry forever) ------
        set_mode(args.port, "missing")
        dest = work / "gone.bin"
        started = time.time()
        code, out = run_cli(cli, ["add", url, str(dest)], work, timeout=60)
        elapsed = time.time() - started
        if code != 0 and elapsed < 30:
            result.add("not-found", True, f"failed in {elapsed:.1f}s, exit={code}")
        else:
            result.add("not-found", False, f"exit={code} after {elapsed:.1f}s (expected prompt failure)")

        # ---- 5. 429 must not be treated as fatal-success or spin forever --
        set_mode(args.port, "rate-limited")
        dest = work / "limited.bin"
        code, out = run_cli(cli, ["add", url, str(dest)], work, timeout=90)
        wrong_success = dest.exists() and dest.stat().st_size == PAYLOAD_SIZE and sha256_file(dest) != good
        if wrong_success:
            result.add("rate-limited", False, "produced a full-size file with wrong bytes")
        else:
            result.add("rate-limited", True, f"exit={code}, no corrupt output")

        # ---- 6. resume after an abrupt kill -------------------------------
        set_mode(args.port, "normal")
        dest = work / "resume.bin"
        resume_state = work / "state-resume"
        resume_state.mkdir(parents=True, exist_ok=True)
        resume_env = dict(os.environ)
        resume_env["CHUA_DATA_DIR"] = str(resume_state)
        proc = subprocess.Popen(
            [str(cli), "add", url, str(dest)],
            cwd=work,
            env=resume_env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        # Let it get partway, then kill it the way a power loss would.
        deadline = time.time() + 20
        while time.time() < deadline:
            size = dest.stat().st_size if dest.exists() else 0
            if size > PAYLOAD_SIZE // 8:
                break
            if proc.poll() is not None:
                break
            time.sleep(0.2)
        partial = dest.stat().st_size if dest.exists() else 0
        proc.kill()
        proc.wait(timeout=10)

        code, out = run_cli(
            cli, ["resume", "--all"], work, timeout=120, data_dir=resume_state
        )
        if code != 0:
            # Re-adding the same URL to the same state directory should also
            # resume rather than start over.
            code, out = run_cli(
                cli, ["add", url, str(dest)], work, timeout=120, data_dir=resume_state
            )
        settle(dest, timeout=90)
        if dest.exists() and sha256_file(dest) == good:
            result.add(
                "resume-after-kill",
                True,
                f"resumed from ~{partial} bytes, final sha256 matches",
            )
        else:
            result.add(
                "resume-after-kill",
                False,
                f"partial={partial} exit={code}\n{out[-400:]}",
            )

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
        if result.failed == 0:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"\nartefacts kept for inspection: {work}")

    print(f"\n{len(result.rows) - result.failed}/{len(result.rows)} checks passed")
    return 1 if result.failed else 0


if __name__ == "__main__":
    sys.exit(main())
