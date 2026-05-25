#!/usr/bin/env python3
"""Benchmark php-lsp request latency for cold and warmed index states."""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import select
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REQUEST_TIMEOUT_S = 30.0


@dataclass(frozen=True)
class Position:
    line: int
    character: int


@dataclass(frozen=True)
class RequestCase:
    name: str
    method: str
    file: Path
    position: Position
    params: dict[str, Any]


class LspSession:
    def __init__(
        self,
        server: Path,
        stderr_path: Path,
        workspace: Path,
        stubs: Path | None,
        timeout_s: int,
    ) -> None:
        self.server = server
        self.stderr_path = stderr_path
        self.workspace = workspace
        self.stubs = stubs
        self.timeout_s = timeout_s
        self.next_id = 1
        self.ready_observed = False
        self.status_events: list[dict[str, Any]] = []
        self.pending_responses: dict[int, dict[str, Any]] = {}
        self.proc: subprocess.Popen[bytes] | None = None
        self.stderr_handle = None

    def __enter__(self) -> "LspSession":
        self.stderr_path.parent.mkdir(parents=True, exist_ok=True)
        self.stderr_handle = self.stderr_path.open("wb")
        self.proc = subprocess.Popen(
            [str(self.server)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_handle,
            env=os.environ.copy(),
        )
        self.initialize()
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        self.shutdown()
        if self.stderr_handle is not None:
            self.stderr_handle.close()

    def send_message(self, payload: dict[str, Any]) -> None:
        assert self.proc is not None and self.proc.stdin is not None
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        self.proc.stdin.write(header)
        self.proc.stdin.write(body)
        self.proc.stdin.flush()

    def send_request(self, method: str, params: Any) -> int:
        request_id = self.next_id
        self.next_id += 1
        self.send_message(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        return request_id

    def send_notification(self, method: str, params: Any) -> None:
        self.send_message({"jsonrpc": "2.0", "method": method, "params": params})

    def read_message(self, timeout_s: float) -> dict[str, Any] | None:
        assert self.proc is not None and self.proc.stdout is not None
        fd = self.proc.stdout.fileno()
        deadline = time.monotonic() + timeout_s
        content_length: int | None = None

        while True:
            line = self.read_line(fd, deadline)
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

        body = self.read_exact(fd, content_length, deadline)
        if body is None:
            return None
        return json.loads(body.decode("utf-8"))

    @staticmethod
    def read_line(fd: int, deadline: float) -> bytes | None:
        line = bytearray()
        while not line.endswith(b"\r\n"):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            readable, _, _ = select.select([fd], [], [], remaining)
            if not readable:
                return None
            chunk = os.read(fd, 1)
            if not chunk:
                return None
            line.extend(chunk)
        return bytes(line)

    @staticmethod
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

    def observe_notification(self, msg: dict[str, Any]) -> None:
        if msg.get("method") != "phpLsp/indexingStatus":
            return
        params = msg.get("params", {})
        self.status_events.append(params)
        if params.get("phase") == "ready":
            self.ready_observed = True

    def wait_for_response(self, request_id: int, timeout_s: float) -> dict[str, Any] | None:
        pending = self.pending_responses.pop(request_id, None)
        if pending is not None:
            return pending

        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            msg = self.read_message(deadline - time.monotonic())
            if msg is None:
                if self.proc is not None and self.proc.poll() is not None:
                    return {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {
                            "code": -32000,
                            "message": f"server exited with code {self.proc.returncode}",
                        },
                    }
                continue
            if msg.get("id") == request_id:
                return msg
            if "id" in msg:
                self.pending_responses[int(msg["id"])] = msg
                continue
            self.observe_notification(msg)
        return None

    def initialize(self) -> None:
        root_uri = self.workspace.resolve().as_uri()
        options: dict[str, Any] = {}
        if self.stubs is not None:
            options["stubsPath"] = str(self.stubs)
        request_id = self.send_request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": root_uri,
                "rootPath": str(self.workspace),
                "workspaceFolders": [{"uri": root_uri, "name": self.workspace.name}],
                "capabilities": {"window": {"workDoneProgress": False}},
                "initializationOptions": options,
            },
        )
        response = self.wait_for_response(request_id, self.timeout_s)
        if response is None:
            raise TimeoutError("initialize response timed out")
        if "error" in response:
            raise RuntimeError(f"initialize failed: {response['error']}")
        self.send_notification("initialized", {})

    def open_case_files(self, cases: list[RequestCase]) -> None:
        opened: set[Path] = set()
        for case in cases:
            file_path = case.file.resolve()
            if file_path in opened:
                continue
            opened.add(file_path)
            self.send_notification(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": file_path.as_uri(),
                        "languageId": "php",
                        "version": 1,
                        "text": file_path.read_text(errors="replace"),
                    }
                },
            )

    def wait_ready(self) -> None:
        deadline = time.monotonic() + self.timeout_s
        while not self.ready_observed and time.monotonic() < deadline:
            msg = self.read_message(min(1.0, deadline - time.monotonic()))
            if msg is not None:
                self.observe_notification(msg)
            elif self.proc is not None and self.proc.poll() is not None:
                raise RuntimeError(f"server exited with code {self.proc.returncode}")
        if not self.ready_observed:
            raise TimeoutError("timed out waiting for phpLsp/indexingStatus phase=ready")

    def shutdown(self) -> None:
        if self.proc is None or self.proc.poll() is not None:
            return
        try:
            request_id = self.send_request("shutdown", None)
            self.wait_for_response(request_id, 5.0)
            self.send_notification("exit", None)
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def now_ms() -> float:
    return time.monotonic() * 1000.0


def timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def sanitize_name(name: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "-", name.strip())
    return cleaned.strip("-") or "workspace"


def utf16_col(line: str, codepoint_col: int) -> int:
    return len(line[:codepoint_col].encode("utf-16-le")) // 2


def offset_to_position(source: str, offset: int) -> Position:
    prefix = source[:offset]
    line = prefix.count("\n")
    line_start = prefix.rfind("\n") + 1
    line_text = source[line_start:offset]
    return Position(line=line, character=utf16_col(line_text, len(line_text)))


def find_regex_position(source: str, patterns: list[tuple[str, int, int]]) -> Position | None:
    for pattern, group, add in patterns:
        match = re.search(pattern, source, flags=re.MULTILINE)
        if match is None:
            continue
        return offset_to_position(source, match.start(group) + add)
    return None


def php_files(workspace: Path, include_vendor: bool = False) -> list[Path]:
    ignored_dirs = {".git", "node_modules", "target", "var", "cache", "logs", "tmp"}
    if not include_vendor:
        ignored_dirs.add("vendor")
    files: list[Path] = []
    for root, dirs, names in os.walk(workspace):
        dirs[:] = [d for d in sorted(dirs) if d not in ignored_dirs]
        for name in sorted(names):
            if name.endswith(".php"):
                files.append(Path(root) / name)
    return files


def score_file(source: str) -> int:
    score = 0
    for pattern, weight in [
        (r"\$this->", 10),
        (r"->\w+\(", 10),
        (r"\bnew\s+[A-Z_]\w+", 8),
        (r"\bclass\s+\w+", 8),
        (r"\bfunction\s+\w+", 4),
        (r"::", 4),
        (r"\buse\s+[\w\\]+;", 2),
    ]:
        if re.search(pattern, source):
            score += weight
    return score


def candidate_sources(workspace: Path) -> list[tuple[Path, str]]:
    files = php_files(workspace)
    if not files:
        files = php_files(workspace, include_vendor=True)
    sources: list[tuple[Path, str]] = []
    for file_path in files:
        try:
            sources.append((file_path, file_path.read_text(errors="replace")))
        except OSError:
            continue
    return sorted(sources, key=lambda item: score_file(item[1]), reverse=True)


def text_document_position_params(file_path: Path, position: Position) -> dict[str, Any]:
    return {
        "textDocument": {"uri": file_path.resolve().as_uri()},
        "position": {"line": position.line, "character": position.character},
    }


def choose_case(
    name: str,
    method: str,
    sources: list[tuple[Path, str]],
    patterns: list[tuple[str, int, int]],
    extra_params: dict[str, Any] | None = None,
) -> RequestCase | None:
    for file_path, source in sources:
        position = find_regex_position(source, patterns)
        if position is None:
            continue
        params = text_document_position_params(file_path, position)
        if extra_params:
            params.update(extra_params)
        return RequestCase(name=name, method=method, file=file_path, position=position, params=params)
    return None


def build_cases(workspace: Path) -> list[RequestCase]:
    sources = candidate_sources(workspace)
    cases: list[RequestCase] = []

    hover = choose_case(
        "hover",
        "textDocument/hover",
        sources,
        [
            (r"->(\w+)\(", 1, 0),
            (r"\bclass\s+(\w+)", 1, 0),
            (r"\bfunction\s+(\w+)", 1, 0),
        ],
    )
    if hover is not None:
        cases.append(hover)

    completion = choose_case(
        "completion",
        "textDocument/completion",
        sources,
        [
            (r"\$this->", 0, len("$this->")),
            (r"\b[A-Z_]\w*::", 0, 0),
            (r"\$\s*(?:\n|$)", 0, 1),
        ],
        {"context": {"triggerKind": 1}},
    )
    if completion is not None:
        cases.append(completion)

    definition = choose_case(
        "definition",
        "textDocument/definition",
        sources,
        [
            (r"->(\w+)\(", 1, 0),
            (r"\bnew\s+([A-Z_]\w*)", 1, 0),
            (r"\buse\s+([\w\\]+);", 1, 0),
        ],
    )
    if definition is not None:
        cases.append(definition)

    references = choose_case(
        "references",
        "textDocument/references",
        sources,
        [(r"\bclass\s+(\w+)", 1, 0), (r"\bfunction\s+(\w+)", 1, 0)],
        {"context": {"includeDeclaration": True}},
    )
    if references is not None:
        cases.append(references)

    prepare_rename = choose_case(
        "prepareRename",
        "textDocument/prepareRename",
        sources,
        [(r"\bclass\s+(\w+)", 1, 0), (r"\bfunction\s+(\w+)", 1, 0)],
    )
    if prepare_rename is not None:
        cases.append(prepare_rename)

    rename = choose_case(
        "renameDryRun",
        "textDocument/rename",
        sources,
        [(r"\bclass\s+(\w+)", 1, 0), (r"\bfunction\s+(\w+)", 1, 0)],
        {"newName": "__PhpLspLatencyBenchTmp"},
    )
    if rename is not None:
        cases.append(rename)

    return cases


def result_shape(response: dict[str, Any] | None) -> str:
    if response is None:
        return "timeout"
    if "error" in response:
        return "error"
    result = response.get("result")
    if result is None:
        return "null"
    if isinstance(result, list):
        return f"array:{len(result)}"
    if isinstance(result, dict):
        return "object"
    return type(result).__name__


def is_cancelled_response(response: dict[str, Any] | None) -> bool:
    if response is None:
        return False
    error = response.get("error")
    return isinstance(error, dict) and error.get("code") == -32800


def run_group(
    session: LspSession,
    phase: str,
    open_state: str,
    cases: list[RequestCase],
    iterations: int,
) -> list[dict[str, Any]]:
    measurements: list[dict[str, Any]] = []
    ready_before_group = session.ready_observed
    for iteration in range(iterations):
        for case in cases:
            started = now_ms()
            request_id = session.send_request(case.method, case.params)
            response = session.wait_for_response(request_id, REQUEST_TIMEOUT_S)
            duration = now_ms() - started
            measurements.append(
                {
                    "phase": phase,
                    "openState": open_state,
                    "iteration": iteration + 1,
                    "case": case.name,
                    "method": case.method,
                    "file": str(case.file),
                    "position": {
                        "line": case.position.line,
                        "character": case.position.character,
                    },
                    "durationMs": round(duration, 3),
                    "resultShape": result_shape(response),
                    "ok": response is not None and "error" not in response,
                    "error": response.get("error") if response and "error" in response else None,
                    "readyBeforeGroup": ready_before_group,
                    "readyObservedAfterRequest": session.ready_observed,
                }
            )
    return measurements


def percentile(values: list[float], q: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    rank = math.ceil((q / 100.0) * len(ordered)) - 1
    rank = max(0, min(rank, len(ordered) - 1))
    return round(ordered[rank], 3)


def summarize(measurements: list[dict[str, Any]]) -> dict[str, Any]:
    groups: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    for item in measurements:
        key = (item["phase"], item["openState"], item["case"])
        groups.setdefault(key, []).append(item)

    summary: dict[str, Any] = {}
    for (phase, open_state, case_name), items in sorted(groups.items()):
        durations = [float(item["durationMs"]) for item in items if item.get("ok")]
        key = f"{phase}.{open_state}.{case_name}"
        summary[key] = {
            "count": len(items),
            "ok": sum(1 for item in items if item.get("ok")),
            "errors": sum(1 for item in items if not item.get("ok")),
            "p50Ms": percentile(durations, 50),
            "p95Ms": percentile(durations, 95),
            "p99Ms": percentile(durations, 99),
            "minMs": round(min(durations), 3) if durations else None,
            "maxMs": round(max(durations), 3) if durations else None,
        }
    return summary


def summarize_heavy(measurements: list[dict[str, Any]]) -> dict[str, Any]:
    groups: dict[tuple[str, str], list[dict[str, Any]]] = {}
    heavy_groups: dict[str, list[dict[str, Any]]] = {}
    for item in measurements:
        if item["kind"] == "fast":
            key = (item["heavyCase"], item["case"])
            groups.setdefault(key, []).append(item)
        elif item["kind"] == "heavy":
            heavy_groups.setdefault(item["case"], []).append(item)

    summary: dict[str, Any] = {}
    for (heavy_case, fast_case), items in sorted(groups.items()):
        durations = [float(item["durationMs"]) for item in items if item.get("ok")]
        key = f"while.{heavy_case}.{fast_case}"
        summary[key] = {
            "count": len(items),
            "ok": sum(1 for item in items if item.get("ok")),
            "errors": sum(1 for item in items if not item.get("ok")),
            "p50Ms": percentile(durations, 50),
            "p95Ms": percentile(durations, 95),
            "p99Ms": percentile(durations, 99),
            "minMs": round(min(durations), 3) if durations else None,
            "maxMs": round(max(durations), 3) if durations else None,
        }

    for heavy_case, items in sorted(heavy_groups.items()):
        durations = [float(item["durationMs"]) for item in items if item.get("ok")]
        key = f"heavy.{heavy_case}"
        summary[key] = {
            "count": len(items),
            "ok": sum(1 for item in items if item.get("ok")),
            "errors": sum(1 for item in items if not item.get("ok")),
            "p50Ms": percentile(durations, 50),
            "p95Ms": percentile(durations, 95),
            "p99Ms": percentile(durations, 99),
            "minMs": round(min(durations), 3) if durations else None,
            "maxMs": round(max(durations), 3) if durations else None,
        }

    cancel_groups: dict[str, list[dict[str, Any]]] = {}
    for item in measurements:
        if item["kind"] == "cancel":
            cancel_groups.setdefault(item["case"], []).append(item)

    for heavy_case, items in sorted(cancel_groups.items()):
        durations = [float(item["durationMs"]) for item in items if item.get("responded")]
        key = f"cancel.{heavy_case}"
        summary[key] = {
            "count": len(items),
            "responded": sum(1 for item in items if item.get("responded")),
            "cancelled": sum(1 for item in items if item.get("cancelled")),
            "completedBeforeCancel": sum(1 for item in items if item.get("completedBeforeCancel")),
            "errors": sum(1 for item in items if item.get("error") and not item.get("cancelled")),
            "p50Ms": percentile(durations, 50),
            "p95Ms": percentile(durations, 95),
            "p99Ms": percentile(durations, 99),
            "minMs": round(min(durations), 3) if durations else None,
            "maxMs": round(max(durations), 3) if durations else None,
        }
    return summary


def run_heavy_responsiveness_group(
    session: LspSession,
    cases: list[RequestCase],
    iterations: int,
) -> list[dict[str, Any]]:
    heavy_cases = [case for case in cases if case.name in {"references", "renameDryRun"}]
    fast_cases = [case for case in cases if case.name in {"hover", "completion"}]
    if not heavy_cases:
        raise RuntimeError("no references/renameDryRun cases available for heavy benchmark")
    if not fast_cases:
        raise RuntimeError("no hover/completion cases available for heavy benchmark")

    measurements: list[dict[str, Any]] = []
    for iteration in range(iterations):
        for heavy_case in heavy_cases:
            heavy_started = now_ms()
            heavy_id = session.send_request(heavy_case.method, heavy_case.params)
            for fast_case in fast_cases:
                fast_started = now_ms()
                fast_id = session.send_request(fast_case.method, fast_case.params)
                fast_response = session.wait_for_response(fast_id, REQUEST_TIMEOUT_S)
                fast_duration = now_ms() - fast_started
                measurements.append(
                    {
                        "kind": "fast",
                        "iteration": iteration + 1,
                        "heavyCase": heavy_case.name,
                        "case": fast_case.name,
                        "method": fast_case.method,
                        "file": str(fast_case.file),
                        "durationMs": round(fast_duration, 3),
                        "resultShape": result_shape(fast_response),
                        "ok": fast_response is not None and "error" not in fast_response,
                        "error": fast_response.get("error")
                        if fast_response and "error" in fast_response
                        else None,
                    }
                )

            heavy_response = session.wait_for_response(heavy_id, REQUEST_TIMEOUT_S)
            heavy_duration = now_ms() - heavy_started
            measurements.append(
                {
                    "kind": "heavy",
                    "iteration": iteration + 1,
                    "case": heavy_case.name,
                    "method": heavy_case.method,
                    "file": str(heavy_case.file),
                    "durationMs": round(heavy_duration, 3),
                    "resultShape": result_shape(heavy_response),
                    "ok": heavy_response is not None and "error" not in heavy_response,
                    "error": heavy_response.get("error")
                    if heavy_response and "error" in heavy_response
                    else None,
                }
            )

    for iteration in range(iterations):
        for heavy_case in heavy_cases:
            started = now_ms()
            request_id = session.send_request(heavy_case.method, heavy_case.params)
            session.send_notification("$/cancelRequest", {"id": request_id})
            response = session.wait_for_response(request_id, REQUEST_TIMEOUT_S)
            duration = now_ms() - started
            cancelled = is_cancelled_response(response)
            completed = response is not None and "error" not in response
            measurements.append(
                {
                    "kind": "cancel",
                    "iteration": iteration + 1,
                    "case": heavy_case.name,
                    "method": heavy_case.method,
                    "file": str(heavy_case.file),
                    "durationMs": round(duration, 3),
                    "responded": response is not None,
                    "cancelled": cancelled,
                    "completedBeforeCancel": completed,
                    "resultShape": result_shape(response),
                    "error": response.get("error")
                    if response and "error" in response
                    else None,
                }
            )
    return measurements


def benchmark_heavy_responsiveness(args: argparse.Namespace) -> dict[str, Any]:
    workspace = Path(args.workspace).resolve()
    server = Path(args.server).resolve()
    stubs = Path(args.stubs).resolve() if args.stubs else None
    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    if not workspace.exists():
        raise FileNotFoundError(f"workspace does not exist: {workspace}")
    if not server.exists():
        raise FileNotFoundError(f"server binary does not exist: {server}")
    if stubs is not None and not stubs.exists():
        raise FileNotFoundError(f"stubs path does not exist: {stubs}")

    cases = build_cases(workspace)
    started = now_ms()
    stderr_path = out_dir / f"{sanitize_name(args.scenario)}-heavy-responsiveness.stderr.log"
    with LspSession(server, stderr_path, workspace, stubs, args.timeout) as session:
        session.open_case_files(cases)
        session.wait_ready()
        measurements = run_heavy_responsiveness_group(session, cases, args.iterations)
        status_events = session.status_events

    result = {
        "schemaVersion": 1,
        "timestamp": timestamp(),
        "scenario": args.scenario,
        "workspaceRoot": str(workspace),
        "serverPath": str(server),
        "stubsPath": str(stubs) if stubs else None,
        "iterations": args.iterations,
        "status": "pass",
        "durationMs": round(now_ms() - started, 3),
        "cases": [
            {
                "case": case.name,
                "method": case.method,
                "file": str(case.file),
                "position": {
                    "line": case.position.line,
                    "character": case.position.character,
                },
            }
            for case in cases
            if case.name in {"hover", "completion", "references", "renameDryRun"}
        ],
        "summary": summarize_heavy(measurements),
        "measurements": measurements,
        "lastReadyStatus": next(
            (event for event in reversed(status_events) if event.get("phase") == "ready"),
            None,
        ),
    }

    output_path = out_dir / f"{sanitize_name(args.scenario)}-heavy-responsiveness.json"
    output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    result["outputPath"] = str(output_path)
    return result


def run_session(
    args: argparse.Namespace,
    scenario: str,
    workspace: Path,
    server: Path,
    stubs: Path | None,
    out_dir: Path,
    cases: list[RequestCase],
    open_state: str,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    stderr_path = out_dir / f"{sanitize_name(scenario)}-latency-{open_state}.stderr.log"
    with LspSession(server, stderr_path, workspace, stubs, args.timeout) as session:
        if open_state == "open":
            session.open_case_files(cases)
        cold = run_group(session, "cold", open_state, cases, args.iterations)
        session.wait_ready()
        warm = run_group(session, "warm", open_state, cases, args.iterations)
        return cold + warm, session.status_events


def benchmark(args: argparse.Namespace) -> dict[str, Any]:
    workspace = Path(args.workspace).resolve()
    server = Path(args.server).resolve()
    stubs = Path(args.stubs).resolve() if args.stubs else None
    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    if not workspace.exists():
        raise FileNotFoundError(f"workspace does not exist: {workspace}")
    if not server.exists():
        raise FileNotFoundError(f"server binary does not exist: {server}")
    if stubs is not None and not stubs.exists():
        raise FileNotFoundError(f"stubs path does not exist: {stubs}")

    cases = build_cases(workspace)
    if not cases:
        raise RuntimeError(f"no benchmark request positions found in {workspace}")

    all_measurements: list[dict[str, Any]] = []
    status_events: list[dict[str, Any]] = []
    started = now_ms()
    for open_state in ["unopened", "open"]:
        measurements, events = run_session(
            args,
            args.scenario,
            workspace,
            server,
            stubs,
            out_dir,
            cases,
            open_state,
        )
        all_measurements.extend(measurements)
        status_events.extend(events)

    result = {
        "schemaVersion": 1,
        "timestamp": timestamp(),
        "scenario": args.scenario,
        "workspaceRoot": str(workspace),
        "serverPath": str(server),
        "stubsPath": str(stubs) if stubs else None,
        "iterations": args.iterations,
        "status": "pass",
        "durationMs": round(now_ms() - started, 3),
        "cases": [
            {
                "case": case.name,
                "method": case.method,
                "file": str(case.file),
                "position": {
                    "line": case.position.line,
                    "character": case.position.character,
                },
            }
            for case in cases
        ],
        "summary": summarize(all_measurements),
        "measurements": all_measurements,
        "lastReadyStatus": next(
            (event for event in reversed(status_events) if event.get("phase") == "ready"),
            None,
        ),
    }

    output_path = out_dir / f"{sanitize_name(args.scenario)}-latency.json"
    output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    result["outputPath"] = str(output_path)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True, help="Scenario name")
    parser.add_argument("--workspace", required=True, help="Workspace root")
    parser.add_argument("--server", required=True, help="php-lsp server binary")
    parser.add_argument("--stubs", default=None, help="Bundled stubs directory")
    parser.add_argument("--out", required=True, help="Output directory")
    parser.add_argument("--timeout", type=int, default=120, help="Server ready timeout in seconds")
    parser.add_argument("--iterations", type=int, default=5, help="Iterations per case and phase")
    parser.add_argument(
        "--heavy-responsiveness",
        action="store_true",
        help="Measure hover/completion latency while references/rename requests are outstanding",
    )
    args = parser.parse_args()

    try:
        result = benchmark_heavy_responsiveness(args) if args.heavy_responsiveness else benchmark(args)
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    compact = {
        "scenario": result["scenario"],
        "status": result["status"],
        "outputPath": result["outputPath"],
        "cases": len(result["cases"]),
        "iterations": result["iterations"],
        "summary": result["summary"],
    }
    print(json.dumps(compact, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
