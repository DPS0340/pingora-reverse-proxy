#!/usr/bin/env python3
"""Run one command in a bounded process group while teeing combined output."""

from __future__ import annotations

import argparse
import errno
import os
import re
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path


def duration(value: str) -> float:
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([smh]?)", value)
    if match is None:
        raise argparse.ArgumentTypeError(f"invalid duration: {value}")
    scale = {"": 1.0, "s": 1.0, "m": 60.0, "h": 3600.0}[match.group(2)]
    seconds = float(match.group(1)) * scale
    if seconds <= 0:
        raise argparse.ArgumentTypeError("duration must be positive")
    return seconds


def group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def signal_group(process_group: int, signum: int) -> None:
    try:
        os.killpg(process_group, signum)
    except ProcessLookupError:
        pass


def emit(chunk: bytes, log_file: object) -> None:
    log_file.write(chunk)
    log_file.flush()
    try:
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
    except BrokenPipeError:
        pass


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--timeout", required=True, type=duration)
    parser.add_argument("--kill-after", required=True, type=duration)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    command = arguments.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a command is required after --")

    arguments.log.parent.mkdir(parents=True, exist_ok=True)
    with arguments.log.open("wb") as log_file:
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            emit(f"run-bounded: {error}\n".encode(), log_file)
            return 127

        assert process.stdout is not None
        process_group = process.pid
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        deadline = time.monotonic() + arguments.timeout
        kill_deadline: float | None = None
        timed_out = False
        kill_sent = False

        def forward_signal(signum: int, _frame: object) -> None:
            signal_group(process_group, signum)

        previous_int = signal.signal(signal.SIGINT, forward_signal)
        previous_term = signal.signal(signal.SIGTERM, forward_signal)
        try:
            pipe_open = True
            while True:
                now = time.monotonic()
                if not timed_out and now >= deadline:
                    timed_out = True
                    kill_deadline = now + arguments.kill_after
                    signal_group(process_group, signal.SIGTERM)
                if (
                    timed_out
                    and not kill_sent
                    and kill_deadline is not None
                    and now >= kill_deadline
                ):
                    signal_group(process_group, signal.SIGKILL)
                    kill_sent = True

                wait_for = 0.05
                next_deadline = kill_deadline if timed_out and not kill_sent else deadline
                wait_for = max(0.0, min(wait_for, next_deadline - now))
                for key, _ in selector.select(wait_for):
                    try:
                        chunk = os.read(key.fd, 65_536)
                    except OSError as error:
                        if error.errno == errno.EINTR:
                            continue
                        raise
                    if chunk:
                        emit(chunk, log_file)
                    else:
                        selector.unregister(key.fileobj)
                        pipe_open = False

                status = process.poll()
                if status is not None and not pipe_open:
                    if not group_exists(process_group) or kill_sent:
                        break
        finally:
            signal.signal(signal.SIGINT, previous_int)
            signal.signal(signal.SIGTERM, previous_term)
            selector.close()
            # A successful group KILL is final. On macOS, probing the drained
            # group again can briefly report an unsignalable (EPERM) zombie
            # group, so do not issue a redundant second KILL after the pipe
            # has closed and the direct child has been reaped.
            if not kill_sent and group_exists(process_group):
                signal_group(process_group, signal.SIGKILL)
            status = process.wait()

    if timed_out:
        return 124
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    raise SystemExit(main())
