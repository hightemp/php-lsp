#!/usr/bin/env python3
"""Profile php-lsp startup, indexing and first diagnostics for one workspace."""

from __future__ import annotations

import argparse
import json
import os
import re
import select
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def now_ms() -> float:
    return time.monotonic() * 1000.0


def wall_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def path_uri(path: Path) -> str:
    return path.resolve().as_uri()


def sanitize_name(name: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "-", name.strip())
    return cleaned.strip("-") or "workspace"


def send_message(proc: subprocess.Popen[bytes], payload: dict[str, Any]) -> None:
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    assert proc.stdin is not None
    proc.stdin.write(header)
    proc.stdin.write(body)
    proc.stdin.flush()


def send_request(
    proc: subprocess.Popen[bytes],
    request_id: int,
    method: str,
    params: Any,
) -> None:
    send_message(
        proc,
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        },
    )


def send_notification(proc: subprocess.Popen[bytes], method: str, params: Any) -> None:
    send_message(
        proc,
        {
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        },
    )


def read_available(fd: int, timeout_s: float) -> bytes | None:
    readable, _, _ = select.select([fd], [], [], max(0.0, timeout_s))
    if not readable:
        return None
    chunk = os.read(fd, 1)
    return chunk or None


def read_line(fd: int, deadline: float) -> bytes | None:
    line = bytearray()
    while not line.endswith(b"\r\n"):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        chunk = read_available(fd, remaining)
        if chunk is None:
            return None
        line.extend(chunk)
    return bytes(line)


def read_exact(fd: int, length: int, deadline: float) -> bytes | None:
    data = bytearray()
    while len(data) < length:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        readable, _, _ = select.select([fd], [], [], remaining)
        if not readable:
            return None
        chunk = os.read(fd, length - len(data))
        if not chunk:
            return None
        data.extend(chunk)
    return bytes(data)


def read_message(proc: subprocess.Popen[bytes], timeout_s: float) -> dict[str, Any] | None:
    assert proc.stdout is not None
    fd = proc.stdout.fileno()
    deadline = time.monotonic() + timeout_s
    content_length: int | None = None

    while True:
        line = read_line(fd, deadline)
        if line is None:
            return None
        stripped = line.decode("ascii", errors="replace").strip()
        if stripped == "":
            break
        key, _, value = stripped.partition(":")
        if key.lower() == "content-length":
            content_length = int(value.strip())

    if content_length is None:
        return None

    body = read_exact(fd, content_length, deadline)
    if body is None:
        return None
    return json.loads(body.decode("utf-8"))


def find_probe_file(workspace: Path) -> Path | None:
    ignored_dirs = {".git", "node_modules", "target", "var", "cache", "logs", "tmp"}
    for root, dirs, files in os.walk(workspace):
        dirs[:] = [d for d in sorted(dirs) if d not in ignored_dirs]
        for file_name in sorted(files):
            if file_name.endswith(".php"):
                return Path(root) / file_name
    return None


def count_php_files(workspace: Path) -> int:
    ignored_dirs = {".git", "node_modules", "target"}
    count = 0
    for _, dirs, files in os.walk(workspace):
        dirs[:] = [d for d in dirs if d not in ignored_dirs]
        count += sum(1 for file_name in files if file_name.endswith(".php"))
    return count


def read_rss_bytes(pid: int) -> tuple[int | None, str | None]:
    status_path = Path("/proc") / str(pid) / "status"
    try:
        values: dict[str, int] = {}
        for line in status_path.read_text().splitlines():
            if line.startswith(("VmRSS:", "VmHWM:")):
                parts = line.split()
                if len(parts) >= 2:
                    values[parts[0].rstrip(":")] = int(parts[1]) * 1024
        if "VmHWM" in values:
            return values["VmHWM"], "proc-status:VmHWM"
        if "VmRSS" in values:
            return values["VmRSS"], "proc-status:VmRSS"
    except OSError:
        return None, None
    return None, None


def start_rss_sampler(pid: int) -> tuple[threading.Event, dict[str, Any], threading.Thread]:
    stop_event = threading.Event()
    sample = {"peakRssBytes": None, "source": None}

    def run() -> None:
        peak = 0
        source = None
        while not stop_event.is_set():
            rss, rss_source = read_rss_bytes(pid)
            if rss is not None and rss > peak:
                peak = rss
                source = rss_source
            time.sleep(0.05)
        rss, rss_source = read_rss_bytes(pid)
        if rss is not None and rss > peak:
            peak = rss
            source = rss_source
        sample["peakRssBytes"] = peak or None
        sample["source"] = source

    thread = threading.Thread(target=run, name="rss-sampler", daemon=True)
    thread.start()
    return stop_event, sample, thread


def wait_for_initialize(
    proc: subprocess.Popen[bytes],
    request_id: int,
    timeout_s: int,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    deadline = time.monotonic() + timeout_s
    pending: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        msg = read_message(proc, deadline - time.monotonic())
        if msg is None:
            break
        if msg.get("id") == request_id:
            return msg, pending
        pending.append(msg)
    raise TimeoutError("initialize response timed out")


def shutdown_server(proc: subprocess.Popen[bytes], next_id: int) -> None:
    if proc.poll() is not None:
        return
    try:
        send_request(proc, next_id, "shutdown", None)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            msg = read_message(proc, deadline - time.monotonic())
            if msg is not None and msg.get("id") == next_id:
                break
        send_notification(proc, "exit", None)
        proc.wait(timeout=5)
    except Exception:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()


def profile_workspace(args: argparse.Namespace) -> dict[str, Any]:
    workspace = Path(args.workspace).resolve()
    server = Path(args.server).resolve()
    stubs = Path(args.stubs).resolve() if args.stubs else None
    output_dir = Path(args.out).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    if not workspace.exists():
        raise FileNotFoundError(f"workspace does not exist: {workspace}")
    if not server.exists():
        raise FileNotFoundError(f"server binary does not exist: {server}")
    if stubs is not None and not stubs.exists():
        raise FileNotFoundError(f"stubs path does not exist: {stubs}")

    probe_file = Path(args.probe).resolve() if args.probe else find_probe_file(workspace)
    php_file_count = count_php_files(workspace)
    stderr_path = output_dir / f"{sanitize_name(args.scenario)}.stderr.log"

    env = os.environ.copy()
    start_ms = now_ms()
    with stderr_path.open("wb") as stderr:
        proc = subprocess.Popen(
            [str(server)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            env=env,
        )

        rss_stop, rss_sample, rss_thread = start_rss_sampler(proc.pid)
        req_id = 1
        root_uri = path_uri(workspace)
        init_options: dict[str, Any] = {}
        if stubs is not None:
            init_options["stubsPath"] = str(stubs)

        initialize_sent_ms = now_ms()
        send_request(
            proc,
            req_id,
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": root_uri,
                "rootPath": str(workspace),
                "workspaceFolders": [{"uri": root_uri, "name": workspace.name}],
                "capabilities": {
                    "window": {"workDoneProgress": False},
                },
                "initializationOptions": init_options,
            },
        )
        initialize_response, pending = wait_for_initialize(proc, req_id, args.timeout)
        initialize_response_ms = now_ms()
        req_id += 1

        if "error" in initialize_response:
            raise RuntimeError(f"initialize failed: {initialize_response['error']}")

        send_notification(proc, "initialized", {})

        did_open_sent_ms: float | None = None
        probe_uri: str | None = None
        if probe_file is not None and probe_file.exists():
            probe_uri = path_uri(probe_file)
            source = probe_file.read_text(errors="replace")
            did_open_sent_ms = now_ms()
            send_notification(
                proc,
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": probe_uri,
                        "languageId": "php",
                        "version": 1,
                        "text": source,
                    }
                },
            )

        status_events: list[dict[str, Any]] = []
        first_loading_stubs_ms: float | None = None
        stubs_loaded_ms: float | None = None
        first_indexing_ms: float | None = None
        ready_ms: float | None = None
        ready_status: dict[str, Any] | None = None
        diagnostics_ms: float | None = None
        diagnostics_count: int | None = None
        stub_files: int | None = None
        error_message: str | None = None

        for msg in pending:
            if msg.get("method") == "phpLsp/indexingStatus":
                status_events.append(msg.get("params", {}))

        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            if ready_status is not None and (probe_uri is None or diagnostics_ms is not None):
                break
            msg = read_message(proc, min(1.0, max(0.0, deadline - time.monotonic())))
            if msg is None:
                if proc.poll() is not None:
                    error_message = f"server exited with code {proc.returncode}"
                    break
                continue

            method = msg.get("method")
            params = msg.get("params", {})

            if method == "phpLsp/indexingStatus":
                status_events.append(params)
                phase = params.get("phase")
                if phase == "loadingStubs" and first_loading_stubs_ms is None:
                    first_loading_stubs_ms = now_ms()
                elif phase == "stubsLoaded":
                    stubs_loaded_ms = now_ms()
                    stub_files = params.get("stubFiles")
                elif phase == "indexing" and first_indexing_ms is None:
                    first_indexing_ms = now_ms()
                elif phase == "ready":
                    ready_ms = now_ms()
                    ready_status = params
                elif phase == "error":
                    error_message = params.get("message", "indexing failed")
                    break
            elif method == "textDocument/publishDiagnostics" and params.get("uri") == probe_uri:
                if diagnostics_ms is None:
                    diagnostics_ms = now_ms()
                    diagnostics_count = len(params.get("diagnostics", []))

        rss_stop.set()
        rss_thread.join(timeout=1)
        shutdown_server(proc, req_id)

    end_ms = now_ms()
    if ready_status is None and error_message is None:
        error_message = "timed out waiting for phpLsp/indexingStatus phase=ready"

    indexed_files = int((ready_status or {}).get("indexedFiles") or 0)
    indexed_symbols = int((ready_status or {}).get("indexedSymbols") or 0)
    indexing_elapsed_ms = (ready_status or {}).get("elapsedMs")
    indexing_elapsed_s = (float(indexing_elapsed_ms) / 1000.0) if indexing_elapsed_ms else None

    def rate(value: int, seconds: float | None) -> float | None:
        if not seconds or seconds <= 0:
            return None
        return round(value / seconds, 2)

    result = {
        "schemaVersion": 1,
        "timestamp": wall_timestamp(),
        "scenario": args.scenario,
        "workspaceRoot": str(workspace),
        "serverPath": str(server),
        "stubsPath": str(stubs) if stubs else None,
        "stderrLog": str(stderr_path),
        "status": "pass" if error_message is None else "fail",
        "error": error_message,
        "counts": {
            "workspacePhpFiles": php_file_count,
            "indexedFiles": indexed_files,
            "indexedSymbols": indexed_symbols,
            "stubFiles": stub_files,
            "cacheFilesLoaded": (ready_status or {}).get("cacheFilesLoaded"),
            "cacheFilesStale": (ready_status or {}).get("cacheFilesStale"),
            "cacheFilesMissing": (ready_status or {}).get("cacheFilesMissing"),
        },
        "timingsMs": {
            "processStartToInitializeResponse": round(initialize_response_ms - start_ms, 2),
            "initializeRequest": round(initialize_response_ms - initialize_sent_ms, 2),
            "stubsLoad": round(stubs_loaded_ms - first_loading_stubs_ms, 2)
            if first_loading_stubs_ms is not None and stubs_loaded_ms is not None
            else None,
            "indexingServerElapsed": indexing_elapsed_ms,
            "indexingClientWall": round(ready_ms - first_indexing_ms, 2)
            if first_indexing_ms is not None and ready_ms is not None
            else None,
            "workspaceReady": round(ready_ms - start_ms, 2) if ready_ms is not None else None,
            "firstDiagnostics": round(diagnostics_ms - did_open_sent_ms, 2)
            if diagnostics_ms is not None and did_open_sent_ms is not None
            else None,
            "totalProcess": round(end_ms - start_ms, 2),
        },
        "rates": {
            "filesPerSec": rate(indexed_files, indexing_elapsed_s),
            "symbolsPerSec": rate(indexed_symbols, indexing_elapsed_s),
        },
        "memory": rss_sample,
        "diagnostics": {
            "probeFile": str(probe_file) if probe_file else None,
            "count": diagnostics_count,
        },
        "lastIndexingStatus": ready_status,
    }

    output_path = output_dir / f"{sanitize_name(args.scenario)}.json"
    output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    result["outputPath"] = str(output_path)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True, help="Scenario name for output JSON")
    parser.add_argument("--workspace", required=True, help="Workspace root to profile")
    parser.add_argument("--server", required=True, help="php-lsp server binary")
    parser.add_argument("--stubs", default=None, help="Bundled stubs directory")
    parser.add_argument("--out", required=True, help="Output directory for JSON results")
    parser.add_argument("--timeout", type=int, default=120, help="Timeout in seconds")
    parser.add_argument("--probe", default=None, help="PHP file to open for first diagnostics")
    args = parser.parse_args()

    try:
        result = profile_workspace(args)
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    print(
        json.dumps(
            {
                "scenario": result["scenario"],
                "status": result["status"],
                "outputPath": result["outputPath"],
                "counts": result["counts"],
                "timingsMs": result["timingsMs"],
                "rates": result["rates"],
                "memory": result["memory"],
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0 if result["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
