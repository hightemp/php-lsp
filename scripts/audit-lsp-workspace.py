#!/usr/bin/env python3
"""Audit php-lsp against a real workspace by opening PHP files in batches."""

from __future__ import annotations

import argparse
import json
import os
import re
import select
import subprocess
import sys
import time
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Position:
    line: int
    character: int


@dataclass(frozen=True)
class Probe:
    name: str
    method: str
    file: Path
    position: Position
    params: dict[str, Any]
    expect_non_null: bool = False
    expected_labels: tuple[str, ...] = ()


def now_ms() -> float:
    return time.monotonic() * 1000.0


def timestamp() -> str:
    return datetime.now(timezone.utc).isoformat()


def path_uri(path: Path) -> str:
    return path.resolve().as_uri()


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


def php_files(workspace: Path, include_vendor: bool) -> list[Path]:
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


def read_source(path: Path) -> str | None:
    try:
        return path.read_text(errors="replace")
    except OSError:
        return None


def position_params(file_path: Path, position: Position) -> dict[str, Any]:
    return {
        "textDocument": {"uri": path_uri(file_path)},
        "position": {"line": position.line, "character": position.character},
    }


def definition_probes(
    file_path: Path, source: str, limit: int, strict_parent_definitions: bool
) -> list[Probe]:
    probes: list[Probe] = []
    masked = mask_php_non_code(source)

    def add(
        name: str,
        position: Position,
        expect_non_null: bool = False,
    ) -> None:
        probes.append(
            Probe(
                name=name,
                method="textDocument/definition",
                file=file_path,
                position=position,
                params=position_params(file_path, position),
                expect_non_null=expect_non_null,
            )
        )

    for match in re.finditer(r"\bparent::([A-Za-z_]\w*)", masked):
        add(
            "definition.parentScope",
            offset_to_position(source, match.start(0)),
            expect_non_null=strict_parent_definitions,
        )
        add(
            "definition.parentMember",
            offset_to_position(source, match.start(1)),
            expect_non_null=strict_parent_definitions,
        )
        if len(probes) >= limit:
            return probes

    qualified_class = r"\\?(?:[A-Z_][A-Za-z0-9_]*\\)*[A-Z_][A-Za-z0-9_]*"
    qualified_name = r"\\?(?:[A-Za-z_][A-Za-z0-9_]*\\)+[A-Za-z_][A-Za-z0-9_]*"
    identifier = r"[A-Za-z_][A-Za-z0-9_]*"

    for pattern, name, group in [
        (rf"\bnew\s+({qualified_class})", "definition.newClass", 1),
        (rf"->({identifier})\s*\(", "definition.memberCall", 1),
        (rf"\buse\s+(?:function\s+|const\s+)?({qualified_name})", "definition.useImport", 1),
    ]:
        for match in re.finditer(pattern, masked):
            add(name, offset_to_position(source, match.start(group)))
            if len(probes) >= limit:
                return probes

    for match in re.finditer(rf"\b(self|static|parent|{qualified_class})::({identifier})", masked):
        member = match.group(2)
        if member == "class":
            add("definition.staticScope", offset_to_position(source, match.start(1)))
        else:
            add("definition.staticScope", offset_to_position(source, match.start(1)))
            add("definition.staticMember", offset_to_position(source, match.start(2)))
        if len(probes) >= limit:
            return probes

    return probes


def mask_php_non_code(source: str) -> str:
    """Replace strings and comments with spaces while preserving offsets."""
    chars = list(source)
    i = 0
    state = "code"
    while i < len(chars):
        c = chars[i]
        n = chars[i + 1] if i + 1 < len(chars) else ""

        if state == "code":
            if c == "/" and n == "/":
                chars[i] = chars[i + 1] = " "
                i += 2
                state = "line_comment"
                continue
            if c == "#":
                chars[i] = " "
                i += 1
                state = "line_comment"
                continue
            if c == "/" and n == "*":
                chars[i] = chars[i + 1] = " "
                i += 2
                state = "block_comment"
                continue
            if c == "'":
                chars[i] = " "
                i += 1
                state = "single_quote"
                continue
            if c == '"':
                chars[i] = " "
                i += 1
                state = "double_quote"
                continue
            i += 1
            continue

        if state == "line_comment":
            if c == "\n":
                state = "code"
            else:
                chars[i] = " "
            i += 1
            continue

        if state == "block_comment":
            if c == "*" and n == "/":
                chars[i] = chars[i + 1] = " "
                i += 2
                state = "code"
            else:
                if c != "\n":
                    chars[i] = " "
                i += 1
            continue

        if state == "single_quote":
            if c == "\\":
                chars[i] = " "
                if i + 1 < len(chars) and chars[i + 1] != "\n":
                    chars[i + 1] = " "
                i += 2
                continue
            if c == "'":
                chars[i] = " "
                i += 1
                state = "code"
            else:
                if c != "\n":
                    chars[i] = " "
                i += 1
            continue

        if state == "double_quote":
            if c == "\\":
                chars[i] = " "
                if i + 1 < len(chars) and chars[i + 1] != "\n":
                    chars[i + 1] = " "
                i += 2
                continue
            if c == '"':
                chars[i] = " "
                i += 1
                state = "code"
            else:
                if c != "\n":
                    chars[i] = " "
                i += 1
            continue

    return "".join(chars)


def completion_probe(
    name: str,
    file_path: Path,
    position: Position,
    expected_label: str,
) -> Probe | None:
    if not expected_label or expected_label == "__halt_compiler":
        return None
    return Probe(
        name=name,
        method="textDocument/completion",
        file=file_path,
        position=position,
        params=position_params(file_path, position),
        expected_labels=(expected_label,),
    )


def completion_label_probes(file_path: Path, source: str) -> list[Probe]:
    masked = mask_php_non_code(source)
    probes: list[Probe] = []
    seen: set[tuple[str, int, int, str]] = set()

    def add(name: str, end_offset: int, expected_label: str) -> None:
        position = offset_to_position(source, end_offset)
        key = (name, position.line, position.character, expected_label)
        if key in seen:
            return
        probe = completion_probe(name, file_path, position, expected_label)
        if probe is None:
            return
        seen.add(key)
        probes.append(probe)

    qualified_class = r"\\?(?:[A-Z_][A-Za-z0-9_]*\\)*[A-Z_][A-Za-z0-9_]*"
    identifier = r"[A-Za-z_][A-Za-z0-9_]*"

    for match in re.finditer(rf"\bnew\s+({qualified_class})", masked):
        expected = match.group(1).lstrip("\\").split("\\")[-1]
        if expected.lower() != "class":
            add("completion.simpleNewClass", match.end(1), expected)

    for match in re.finditer(rf"\bextends\s+({qualified_class})", masked):
        add(
            "completion.extendsClass",
            match.end(1),
            match.group(1).lstrip("\\").split("\\")[-1],
        )

    for match in re.finditer(rf"\bimplements\s+({qualified_class})", masked):
        add(
            "completion.implementsClass",
            match.end(1),
            match.group(1).lstrip("\\").split("\\")[-1],
        )

    for match in re.finditer(rf"\b({qualified_class}|self|static|parent)::class\b", masked):
        add("completion.staticClassLiteral", match.end(0), "class")

    for match in re.finditer(rf"\b(self|static|parent)::({identifier})", masked):
        member = match.group(2)
        if member == "class":
            continue
        add(f"completion.static{match.group(1).title()}Member", match.end(2), member)

    for match in re.finditer(rf"\b({qualified_class})::({identifier})", masked):
        member = match.group(2)
        if member == "class":
            continue
        add("completion.staticNamedMember", match.end(2), member)

    for match in re.finditer(rf"\$this\??->({identifier})", masked):
        add("completion.thisMember", match.end(1), match.group(1))

    for match in re.finditer(rf"\$([A-Za-z_][A-Za-z0-9_]*)\?->({identifier})", masked):
        if match.group(1) == "this":
            continue
        add("completion.nullableVariableMember", match.end(2), match.group(2))

    for match in re.finditer(rf"\$([A-Za-z_][A-Za-z0-9_]*)->({identifier})", masked):
        if match.group(1) == "this":
            continue
        add("completion.variableMember", match.end(2), match.group(2))

    for match in re.finditer(rf"\)\??->({identifier})", masked):
        add("completion.chainedMember", match.end(1), match.group(1))

    for match in re.finditer(rf"->{identifier}\??->({identifier})", masked):
        add("completion.propertyChainMember", match.end(1), match.group(1))

    for match in re.finditer(rf"\(\s*new\s+{qualified_class}[^;\n]*?\)\??->({identifier})", masked):
        add("completion.newExpressionMember", match.end(1), match.group(1))

    return probes


def labels_from_completion_result(result: Any) -> list[str]:
    if result is None:
        return []
    if isinstance(result, list):
        items = result
    elif isinstance(result, dict):
        items = result.get("items", [])
    else:
        return []
    labels: list[str] = []
    for item in items:
        if isinstance(item, dict) and isinstance(item.get("label"), str):
            labels.append(item["label"])
    return labels


class LspSession:
    def __init__(
        self,
        server: Path,
        workspace: Path,
        stubs: Path | None,
        stderr_path: Path,
        ready_timeout_s: float,
        diagnostics_mode: str,
        diagnostics_severity: Any,
        index_vendor: bool,
    ) -> None:
        self.server = server
        self.workspace = workspace
        self.stubs = stubs
        self.stderr_path = stderr_path
        self.ready_timeout_s = ready_timeout_s
        self.diagnostics_mode = diagnostics_mode
        self.diagnostics_severity = diagnostics_severity
        self.index_vendor = index_vendor
        self.next_id = 1
        self.proc: subprocess.Popen[bytes] | None = None
        self.stderr_handle = None
        self.status_events: list[dict[str, Any]] = []
        self.ready_status: dict[str, Any] | None = None

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
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        )
        return request_id

    def send_notification(self, method: str, params: Any) -> None:
        self.send_message({"jsonrpc": "2.0", "method": method, "params": params})

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

    def observe_notification(self, msg: dict[str, Any]) -> None:
        if msg.get("method") != "phpLsp/indexingStatus":
            return
        params = msg.get("params", {})
        self.status_events.append(params)
        if params.get("phase") == "ready":
            self.ready_status = params

    def wait_for_response(self, request_id: int, timeout_s: float) -> dict[str, Any] | None:
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
            self.observe_notification(msg)
        return None

    def initialize(self) -> None:
        root_uri = path_uri(self.workspace)
        options: dict[str, Any] = {
            "diagnosticsMode": self.diagnostics_mode,
            "indexVendor": self.index_vendor,
            "phpVersion": "8.4",
        }
        if self.diagnostics_severity is not None:
            options["diagnosticsSeverity"] = self.diagnostics_severity
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
        response = self.wait_for_response(request_id, self.ready_timeout_s)
        if response is None:
            raise TimeoutError("initialize response timed out")
        if "error" in response:
            raise RuntimeError(f"initialize failed: {response['error']}")
        self.send_notification("initialized", {})

    def wait_ready(self) -> None:
        deadline = time.monotonic() + self.ready_timeout_s
        while self.ready_status is None and time.monotonic() < deadline:
            msg = self.read_message(min(1.0, deadline - time.monotonic()))
            if msg is None:
                if self.proc is not None and self.proc.poll() is not None:
                    raise RuntimeError(f"server exited with code {self.proc.returncode}")
                continue
            self.observe_notification(msg)
        if self.ready_status is None:
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


def diagnostic_key(diagnostic: dict[str, Any]) -> str:
    source = diagnostic.get("source") or "unknown"
    code = diagnostic.get("code")
    message = diagnostic.get("message") or ""
    if isinstance(code, dict):
        code = code.get("value")
    return f"{source}:{code or 'no-code'}:{message}"


def summarize_top(counter: Counter[str], limit: int) -> list[dict[str, Any]]:
    return [{"key": key, "count": count} for key, count in counter.most_common(limit)]


def drain_batch(
    session: LspSession,
    batch_uris: set[str],
    pending_requests: dict[int, Probe],
    diagnostics_by_uri: dict[str, list[dict[str, Any]]],
    request_errors: list[dict[str, Any]],
    null_expected_results: list[dict[str, Any]],
    null_definition_results: list[dict[str, Any]],
    completion_label_misses: list[dict[str, Any]],
    completion_stats: dict[str, Counter[str]],
    deadline_s: float,
) -> set[str]:
    seen_diagnostics: set[str] = set()
    while time.monotonic() < deadline_s:
        must_wait = bool(pending_requests) or seen_diagnostics != batch_uris
        if not must_wait:
            # Drain already queued notifications so the server never blocks on
            # stdout when diagnostics are disabled and the batch has no requests.
            read_timeout = min(0.1, max(0.0, deadline_s - time.monotonic()))
        else:
            read_timeout = min(0.5, max(0.0, deadline_s - time.monotonic()))

        msg = session.read_message(read_timeout)
        if msg is None:
            if session.proc is not None and session.proc.poll() is not None:
                raise RuntimeError(f"server exited with code {session.proc.returncode}")
            if not must_wait:
                break
            continue

        if "id" in msg:
            probe = pending_requests.pop(int(msg["id"]), None)
            if probe is None:
                continue
            if "error" in msg:
                if probe.expected_labels:
                    completion_stats[probe.name]["errors"] += 1
                request_errors.append(
                    {
                        "file": str(probe.file),
                        "method": probe.method,
                        "probe": probe.name,
                        "position": probe.position.__dict__,
                        "error": msg["error"],
                    }
                )
                continue
            if probe.expect_non_null and msg.get("result") is None:
                null_expected_results.append(
                    {
                        "file": str(probe.file),
                        "method": probe.method,
                        "probe": probe.name,
                        "position": probe.position.__dict__,
                    }
                )
            if probe.method == "textDocument/definition" and msg.get("result") is None:
                null_definition_results.append(
                    {
                        "file": str(probe.file),
                        "method": probe.method,
                        "probe": probe.name,
                        "position": probe.position.__dict__,
                        "expectedNonNull": probe.expect_non_null,
                    }
                )
            if probe.expected_labels:
                labels = labels_from_completion_result(msg.get("result"))
                completion_stats[probe.name]["responses"] += 1
                completion_stats[probe.name]["items"] += len(labels)
                if not labels:
                    completion_stats[probe.name]["empty"] += 1
                missing_labels = [
                    label for label in probe.expected_labels if label not in labels
                ]
                if missing_labels:
                    completion_stats[probe.name]["missing"] += len(missing_labels)
                    completion_label_misses.append(
                        {
                            "file": str(probe.file),
                            "probe": probe.name,
                            "position": probe.position.__dict__,
                            "expectedLabels": list(probe.expected_labels),
                            "missingLabels": missing_labels,
                            "labelCount": len(labels),
                            "labels": labels[:80],
                        }
                    )
            continue

        method = msg.get("method")
        params = msg.get("params", {})
        if method == "textDocument/publishDiagnostics":
            uri = params.get("uri")
            if uri in batch_uris:
                diagnostics_by_uri[uri] = params.get("diagnostics", [])
                seen_diagnostics.add(uri)
        elif method == "phpLsp/indexingStatus":
            session.observe_notification(msg)

    for request_id, probe in pending_requests.items():
        if probe.expected_labels:
            completion_stats[probe.name]["timeouts"] += 1
        request_errors.append(
            {
                "file": str(probe.file),
                "method": probe.method,
                "probe": probe.name,
                "position": probe.position.__dict__,
                "error": {"code": "timeout", "message": f"request {request_id} timed out"},
            }
        )
    pending_requests.clear()
    return seen_diagnostics


def drain_unsolicited(session: LspSession, timeout_s: float) -> None:
    if timeout_s <= 0:
        return

    deadline_s = time.monotonic() + timeout_s
    while time.monotonic() < deadline_s:
        msg = session.read_message(min(0.05, max(0.0, deadline_s - time.monotonic())))
        if msg is None:
            if session.proc is not None and session.proc.poll() is not None:
                raise RuntimeError(f"server exited with code {session.proc.returncode}")
            return
        session.observe_notification(msg)


def audit(args: argparse.Namespace) -> dict[str, Any]:
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

    files = (
        [Path(file_name).resolve() for file_name in args.only_file]
        if args.only_file
        else php_files(workspace, include_vendor=args.include_vendor)
    )
    if args.start_index:
        files = files[args.start_index :]
    if args.max_files:
        files = files[: args.max_files]

    started_ms = now_ms()
    stderr_path = out_dir / f"{sanitize_name(args.scenario)}.stderr.log"
    diagnostics_by_uri: dict[str, list[dict[str, Any]]] = {}
    diagnostics_missing: list[str] = []
    request_errors: list[dict[str, Any]] = []
    null_expected_results: list[dict[str, Any]] = []
    null_definition_results: list[dict[str, Any]] = []
    completion_label_misses: list[dict[str, Any]] = []
    completion_stats: dict[str, Counter[str]] = defaultdict(Counter)
    completion_category_counts: Counter[str] = Counter()
    diagnostic_counter: Counter[str] = Counter()
    diagnostic_source_counter: Counter[str] = Counter()
    file_diagnostic_counts: Counter[str] = Counter()
    diagnostic_samples: list[dict[str, Any]] = []
    parse_read_errors: list[str] = []
    total_probes = 0
    total_completion_probes = 0

    diagnostics_severity = (
        json.loads(args.diagnostics_severity)
        if args.diagnostics_severity
        else None
    )

    with LspSession(
        server,
        workspace,
        stubs,
        stderr_path,
        args.ready_timeout,
        args.diagnostics_mode,
        diagnostics_severity,
        args.include_vendor,
    ) as session:
        session.wait_ready()
        if args.progress:
            ready = session.ready_status or {}
            print(
                json.dumps(
                    {
                        "phase": "ready",
                        "indexedFiles": ready.get("indexedFiles"),
                        "indexedSymbols": ready.get("indexedSymbols"),
                        "elapsedMs": ready.get("elapsedMs"),
                    },
                    sort_keys=True,
                ),
                flush=True,
            )

        for batch_start in range(0, len(files), args.batch_size):
            drain_unsolicited(session, args.idle_drain_timeout)

            batch = files[batch_start : batch_start + args.batch_size]
            batch_uris = {path_uri(path) for path in batch}
            wait_diagnostics = args.diagnostics_mode.lower() not in {
                "off",
                "none",
                "disabled",
            }
            pending_requests: dict[int, Probe] = {}

            for file_path in batch:
                source = read_source(file_path)
                if source is None:
                    parse_read_errors.append(str(file_path))
                    continue

                uri = path_uri(file_path)
                session.send_notification(
                    "textDocument/didOpen",
                    {
                        "textDocument": {
                            "uri": uri,
                            "languageId": "php",
                            "version": 1,
                            "text": source,
                        }
                    },
                )

                if args.document_symbol:
                    doc_probe = Probe(
                        name="documentSymbol",
                        method="textDocument/documentSymbol",
                        file=file_path,
                        position=Position(0, 0),
                        params={"textDocument": {"uri": uri}},
                    )
                    pending_requests[
                        session.send_request(doc_probe.method, doc_probe.params)
                    ] = doc_probe

                if args.semantic_tokens:
                    token_probe = Probe(
                        name="semanticTokens.full",
                        method="textDocument/semanticTokens/full",
                        file=file_path,
                        position=Position(0, 0),
                        params={"textDocument": {"uri": uri}},
                    )
                    pending_requests[
                        session.send_request(token_probe.method, token_probe.params)
                        ] = token_probe

                if args.completion_labels and (
                    args.max_completion_probes <= 0
                    or total_completion_probes < args.max_completion_probes
                ):
                    file_completion_probes = 0
                    for probe in completion_label_probes(file_path, source):
                        if (
                            args.max_completion_probes > 0
                            and total_completion_probes >= args.max_completion_probes
                        ):
                            break
                        if (
                            args.max_completion_probes_per_file > 0
                            and file_completion_probes >= args.max_completion_probes_per_file
                        ):
                            break
                        if (
                            args.max_completion_probes_per_category > 0
                            and completion_category_counts[probe.name]
                            >= args.max_completion_probes_per_category
                        ):
                            continue
                        pending_requests[
                            session.send_request(probe.method, probe.params)
                        ] = probe
                        completion_category_counts[probe.name] += 1
                        completion_stats[probe.name]["sent"] += 1
                        total_completion_probes += 1
                        file_completion_probes += 1

                remaining_probe_budget = max(0, args.max_definition_probes - total_probes)
                if remaining_probe_budget > 0:
                    definition_probe_budget = remaining_probe_budget
                    if args.max_definition_probes_per_file > 0:
                        definition_probe_budget = min(
                            definition_probe_budget, args.max_definition_probes_per_file
                        )
                    for probe in definition_probes(
                        file_path,
                        source,
                        definition_probe_budget,
                        args.strict_parent_definitions,
                    ):
                        pending_requests[
                            session.send_request(probe.method, probe.params)
                        ] = probe
                        total_probes += 1

            deadline_s = time.monotonic() + args.batch_timeout
            seen = drain_batch(
                session,
                batch_uris if wait_diagnostics else set(),
                pending_requests,
                diagnostics_by_uri,
                request_errors,
                null_expected_results,
                null_definition_results,
                completion_label_misses,
                completion_stats,
                deadline_s,
            )

            if wait_diagnostics:
                for uri in sorted(batch_uris - seen):
                    diagnostics_missing.append(uri)

            for file_path in batch:
                uri = path_uri(file_path)
                for diagnostic in diagnostics_by_uri.get(uri, []):
                    diagnostic_counter[diagnostic_key(diagnostic)] += 1
                    diagnostic_source_counter[str(diagnostic.get("source") or "unknown")] += 1
                    file_diagnostic_counts[str(file_path)] += 1
                    if len(diagnostic_samples) < args.sample_limit:
                        diagnostic_samples.append(
                            {
                                "file": str(file_path),
                                "range": diagnostic.get("range"),
                                "source": diagnostic.get("source"),
                                "code": diagnostic.get("code"),
                                "severity": diagnostic.get("severity"),
                                "message": diagnostic.get("message"),
                            }
                        )

                session.send_notification(
                    "textDocument/didClose", {"textDocument": {"uri": uri}}
                )

            drain_unsolicited(session, args.idle_drain_timeout)

            done = min(batch_start + len(batch), len(files))
            if args.progress and (done == len(files) or done % args.progress == 0):
                print(
                    json.dumps(
                        {
                            "done": done,
                            "total": len(files),
                            "diagnostics": sum(file_diagnostic_counts.values()),
                            "requestErrors": len(request_errors),
                            "parentMisses": len(null_expected_results),
                            "completionMisses": len(completion_label_misses),
                            "completionProbes": total_completion_probes,
                            "missingDiagnostics": len(diagnostics_missing),
                            "lastFile": str(batch[-1]) if batch else None,
                        },
                        sort_keys=True,
                    ),
                    flush=True,
                )

    stderr_text = stderr_path.read_text(errors="replace") if stderr_path.exists() else ""
    stderr_error_lines = [
        line
        for line in stderr_text.splitlines()
        if "ERROR" in line
        or re.search(r"panic|panicked|thread .* panicked", line, re.IGNORECASE)
    ]

    result = {
        "schemaVersion": 1,
        "timestamp": timestamp(),
        "scenario": args.scenario,
        "workspaceRoot": str(workspace),
        "serverPath": str(server),
        "stubsPath": str(stubs) if stubs else None,
        "includeVendor": args.include_vendor,
        "startIndex": args.start_index,
        "semanticTokens": args.semantic_tokens,
        "completionLabels": args.completion_labels,
        "strictParentDefinitions": args.strict_parent_definitions,
        "status": "pass"
        if (
            not request_errors
            and not null_expected_results
            and not stderr_error_lines
            and (not args.fail_on_completion_miss or not completion_label_misses)
        )
        else "fail",
        "durationMs": round(now_ms() - started_ms, 3),
        "counts": {
            "phpFiles": len(files),
            "filesWithDiagnostics": len(file_diagnostic_counts),
            "diagnostics": sum(file_diagnostic_counts.values()),
            "missingDiagnostics": len(diagnostics_missing),
            "requestErrors": len(request_errors),
            "expectedNonNullDefinitionMisses": len(null_expected_results),
            "nullDefinitionResults": len(null_definition_results),
            "definitionProbes": total_probes,
            "completionProbes": total_completion_probes,
            "completionLabelMisses": len(completion_label_misses),
            "readErrors": len(parse_read_errors),
            "stderrErrorLines": len(stderr_error_lines),
        },
        "indexing": {
            "readyStatus": session.ready_status,
            "statusEvents": session.status_events[-20:],
        },
        "diagnosticsBySource": dict(diagnostic_source_counter),
        "completionByCategory": {
            key: dict(value) for key, value in sorted(completion_stats.items())
        },
        "topDiagnostics": summarize_top(diagnostic_counter, args.top_limit),
        "topFilesWithDiagnostics": summarize_top(file_diagnostic_counts, args.top_limit),
        "diagnosticSamples": diagnostic_samples,
        "missingDiagnosticsSamples": diagnostics_missing[: args.sample_limit],
        "requestErrors": request_errors[: args.sample_limit],
        "expectedNonNullDefinitionMisses": null_expected_results[: args.sample_limit],
        "nullDefinitionResults": null_definition_results[: args.sample_limit],
        "completionLabelMisses": completion_label_misses[: args.sample_limit],
        "readErrors": parse_read_errors[: args.sample_limit],
        "stderrLog": str(stderr_path),
        "stderrErrorSamples": stderr_error_lines[: args.sample_limit],
    }

    output_path = out_dir / f"{sanitize_name(args.scenario)}.json"
    result["outputPath"] = str(output_path)
    output_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True, help="Scenario name")
    parser.add_argument("--workspace", required=True, help="Workspace root")
    parser.add_argument("--server", required=True, help="php-lsp server binary")
    parser.add_argument("--stubs", default=None, help="Bundled stubs directory")
    parser.add_argument("--out", required=True, help="Output directory")
    parser.add_argument("--ready-timeout", type=float, default=300.0)
    parser.add_argument("--diagnostics-mode", default="basic-semantic")
    parser.add_argument(
        "--diagnostics-severity",
        default=None,
        help="JSON value for diagnosticsSeverity initialization option",
    )
    parser.add_argument("--batch-timeout", type=float, default=20.0)
    parser.add_argument("--batch-size", type=int, default=25)
    parser.add_argument(
        "--idle-drain-timeout",
        type=float,
        default=0.2,
        help="Seconds to drain unsolicited server messages between batches",
    )
    parser.add_argument(
        "--start-index",
        type=int,
        default=0,
        help="Skip this many PHP files after sorting/filtering",
    )
    parser.add_argument("--max-files", type=int, default=0)
    parser.add_argument(
        "--only-file",
        action="append",
        default=[],
        help="Audit only this PHP file path; may be passed multiple times",
    )
    parser.add_argument("--max-definition-probes", type=int, default=5000)
    parser.add_argument(
        "--max-definition-probes-per-file",
        type=int,
        default=40,
        help="Maximum definition probes generated from one file; 0 means unlimited",
    )
    parser.add_argument(
        "--strict-parent-definitions",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Mark null parent:: definition probes as expected-non-null misses",
    )
    parser.add_argument(
        "--completion-labels",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Request completion at source-derived positions and verify expected labels",
    )
    parser.add_argument(
        "--max-completion-probes",
        type=int,
        default=5000,
        help="Maximum completion label probes; 0 means unlimited when enabled",
    )
    parser.add_argument(
        "--max-completion-probes-per-category",
        type=int,
        default=1000,
        help="Maximum completion label probes per category; 0 means unlimited",
    )
    parser.add_argument(
        "--max-completion-probes-per-file",
        type=int,
        default=80,
        help="Maximum completion label probes generated from one file; 0 means unlimited",
    )
    parser.add_argument(
        "--fail-on-completion-miss",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Mark report failed when expected completion labels are missing",
    )
    parser.add_argument("--sample-limit", type=int, default=200)
    parser.add_argument("--top-limit", type=int, default=50)
    parser.add_argument("--progress", type=int, default=500)
    parser.add_argument(
        "--document-symbol",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Request textDocument/documentSymbol for every opened file",
    )
    parser.add_argument(
        "--include-vendor",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Include vendor PHP files in the audit",
    )
    parser.add_argument(
        "--semantic-tokens",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Request textDocument/semanticTokens/full for every opened file",
    )
    args = parser.parse_args()

    try:
        result = audit(args)
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
                "diagnosticsBySource": result["diagnosticsBySource"],
                "completionByCategory": result["completionByCategory"],
                "stderrLog": result["stderrLog"],
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0 if result["status"] == "pass" else 2


if __name__ == "__main__":
    raise SystemExit(main())
