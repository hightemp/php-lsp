#!/usr/bin/env python3
"""Quick LSP client to test go-to-definition on a real project."""

import json
import subprocess
import sys
import os
import time

SERVER_BIN = os.path.join(os.path.dirname(__file__), '..', 'client', 'bin', 'linux-x64', 'php-lsp')
STUBS_PATH = os.path.join(os.path.dirname(__file__), '..', 'client', 'stubs')
# Use parent dir (bdpn-ui/) not app/ to test composer.json auto-discovery
WORKSPACE_ROOT = '/home/apanov/Projects/bdpn-ui'
PROJECT_ROOT = '/home/apanov/Projects/bdpn-ui/app'
TEST_FILE = os.path.join(PROJECT_ROOT, 'tests', 'Soap', 'SoapHandlerTest.php')

def send_request(proc, method, params, req_id):
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params}
    body = json.dumps(msg)
    header = f"Content-Length: {len(body)}\r\n\r\n"
    proc.stdin.write(header.encode())
    proc.stdin.write(body.encode())
    proc.stdin.flush()

def send_notification(proc, method, params):
    msg = {"jsonrpc": "2.0", "method": method, "params": params}
    body = json.dumps(msg)
    header = f"Content-Length: {len(body)}\r\n\r\n"
    proc.stdin.write(header.encode())
    proc.stdin.write(body.encode())
    proc.stdin.flush()

def read_response(proc, timeout=30):
    """Read one JSON-RPC message from stdout."""
    import select
    buf = b""
    content_length = None

    deadline = time.time() + timeout

    while time.time() < deadline:
        # Read headers
        while True:
            if time.time() > deadline:
                return None
            line = b""
            while not line.endswith(b"\r\n"):
                ch = proc.stdout.read(1)
                if not ch:
                    return None
                line += ch
            line = line.decode().strip()
            if line == "":
                break  # End of headers
            if line.startswith("Content-Length:"):
                content_length = int(line.split(":")[1].strip())

        if content_length is not None:
            body = proc.stdout.read(content_length)
            return json.loads(body.decode())

    return None

def wait_for_response(proc, expected_id, timeout=60):
    """Read messages until we get one with the expected id."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = read_response(proc, timeout=deadline - time.time())
        if msg is None:
            return None
        if msg.get("id") == expected_id:
            return msg
        # Skip notifications (no id, or different id)
    return None

def main():
    server_path = os.path.realpath(SERVER_BIN)
    stubs_path = os.path.realpath(STUBS_PATH)

    if not os.path.exists(server_path):
        print(f"Server not found at {server_path}")
        sys.exit(1)

    print(f"Starting server: {server_path}")
    env = os.environ.copy()
    log_file = open('test-lsp.log', 'w')
    proc = subprocess.Popen(
        [server_path],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=log_file,
        env=env,
    )

    root_uri = f"file://{WORKSPACE_ROOT}"
    test_file_uri = f"file://{TEST_FILE}"

    # Read the test file
    with open(TEST_FILE, 'r') as f:
        test_content = f.read()

    req_id = 1

    # Initialize
    print("Sending initialize...")
    send_request(proc, "initialize", {
        "processId": os.getpid(),
        "rootUri": root_uri,
        "capabilities": {},
        "initializationOptions": {
            "stubsPath": stubs_path,
        },
    }, req_id)

    resp = wait_for_response(proc, req_id)
    if resp:
        print(f"Initialize OK: capabilities received")
    else:
        print("Initialize FAILED: no response")
        proc.kill()
        sys.exit(1)
    req_id += 1

    # Initialized notification
    send_notification(proc, "initialized", {})
    time.sleep(15)  # Wait for indexing to complete (4000+ files)

    # Open the test file
    print(f"\nOpening {TEST_FILE}")
    send_notification(proc, "textDocument/didOpen", {
        "textDocument": {
            "uri": test_file_uri,
            "languageId": "php",
            "version": 1,
            "text": test_content,
        }
    })

    # Collect diagnostics published for our file
    published_diagnostics = []
    diag_deadline = time.time() + 10  # wait up to 10s for diagnostics
    while time.time() < diag_deadline:
        msg = read_response(proc, timeout=diag_deadline - time.time())
        if msg is None:
            break
        if msg.get("method") == "textDocument/publishDiagnostics":
            params = msg.get("params", {})
            if params.get("uri") == test_file_uri:
                published_diagnostics = params.get("diagnostics", [])
                break  # got our diagnostics

    # Report diagnostics
    if published_diagnostics:
        unresolved = [d for d in published_diagnostics if "Unresolved" in d.get("message", "")]
        print(f"\n  Diagnostics: {len(published_diagnostics)} total, {len(unresolved)} unresolved use")
        for d in unresolved:
            line = d["range"]["start"]["line"] + 1
            print(f"    L{line}: {d['message']}")
    else:
        print(f"\n  Diagnostics: 0 (clean)")


    # Define test cases: (description, line_0based, character_0based)
    test_cases = [
        # Line 35: $this->logger = $this->createStub(LoggerInterface::class);
        ("createStub (inherited from TestCase)", 34, 31),  # col 31 = 'c' in createStub

        # Line 35: $this->logger = $this->createStub(LoggerInterface::class);
        ("LoggerInterface::class", 34, 42),  # col 42 = 'L' in LoggerInterface

        # Line 25: final class SoapHandlerTest extends TestCase
        ("TestCase in extends", 24, 36),  # col 36 = 'T' in TestCase

        # Line 58: $result = $handler->callOkResponse();
        ("callOkResponse (same file)", 57, 28),  # col 28 = 'c' in callOkResponse

        # Line 60: self::assertSame(1001, $result['StatusCode']);
        ("self::assertSame (static method)", 59, 14),  # col 14 = 'a' in assertSame

        # Line 30: private TimerService $timerService;
        ("TimerService type hint", 29, 12),  # col 12 = 'T' in TimerService

        # Line 133: $repo->method('findOneBy')->willReturn($request);
        ("method on $repo (stub method)", 132, 16),  # col 16 = 'm' (after $repo->)

        # Line 133: $repo->method('findOneBy')->willReturn($request);
        ("willReturn chained", 132, 36),  # col 36 = 'w' in willReturn

        # Line 160: $qb->method('join')->willReturnSelf();
        ("method on $qb", 159, 13),  # col 13 = 'm' in method

        # Line 44: return new TestConcreteSoapHandler(
        ("TestConcreteSoapHandler (same file)", 43, 19),  # col 19 = 'T'

        # --- Property assignment fallback cases ---

        # Line 134: $this->em->method('getRepository')->willReturn($repo);
        ("method on $this->em (assignment fallback)", 133, 19),  # col 19 = 'm' after $this->em->

        # Line 134: $this->em->method('getRepository')->willReturn($repo);
        ("willReturn on $this->em chain", 133, 44),  # col 44 = 'w' in willReturn

        # Line 226: $this->workflowService->method('createOrGetDonorProcess')->willReturn($newProcess);
        ("method on $this->workflowService", 225, 32),  # col 32 = 'm' in method

        # Line 226: ...->willReturn($newProcess);
        ("willReturn on workflowService chain", 225, 67),  # col 67 = 'w' in willReturn

        # Line 317: $this->timerService->expects(self::once())
        ("expects on timerService", 316, 29),  # col 29 = 'e' in expects

        # Line 328: $this->timerService->method('start')
        ("method on timerService stub", 327, 29),  # col 29 = 'm' in method

        # --- Use statement go-to-definition ---

        # Line 15: use Doctrine\ORM\EntityManagerInterface;
        ("use Doctrine\\ORM\\EntityManagerInterface", 14, 30),  # col 30 = 'E' in EntityManagerInterface

        # Line 19: use PHPUnit\Framework\TestCase;
        ("use PHPUnit\\Framework\\TestCase", 18, 23),  # col 23 = 'T' in TestCase

        # Line 20: use Psr\Log\LoggerInterface;
        ("use Psr\\Log\\LoggerInterface", 19, 15),  # col 15 = 'L' in LoggerInterface
    ]

    print(f"\n{'='*80}")
    print(f"Testing go-to-definition on SoapHandlerTest.php")
    print(f"{'='*80}\n")

    results = []
    for desc, line, char in test_cases:
        send_request(proc, "textDocument/definition", {
            "textDocument": {"uri": test_file_uri},
            "position": {"line": line, "character": char},
        }, req_id)

        resp = wait_for_response(proc, req_id)
        req_id += 1

        if resp and resp.get("result"):
            result = resp["result"]
            if isinstance(result, list):
                if len(result) > 0:
                    loc = result[0]
                    uri = loc.get("uri", "")
                    rng = loc.get("range", {}).get("start", {})
                    target_line = rng.get("line", "?")
                    # Shorten URI for display
                    short_uri = uri.replace(f"file://{PROJECT_ROOT}/", "")
                    status = f"✓ → {short_uri}:{target_line + 1}"
                else:
                    status = "✗ empty result"
            elif isinstance(result, dict):
                uri = result.get("uri", "")
                rng = result.get("range", {}).get("start", {})
                target_line = rng.get("line", "?")
                short_uri = uri.replace(f"file://{PROJECT_ROOT}/", "")
                status = f"✓ → {short_uri}:{target_line + 1}"
            else:
                status = f"✗ unexpected: {result}"
        else:
            status = "✗ null/no result"

        results.append((desc, status))
        print(f"  L{line+1}:{char} {desc}")
        print(f"    {status}")

    # Shutdown
    send_request(proc, "shutdown", None, req_id)
    wait_for_response(proc, req_id)
    send_notification(proc, "exit", None)
    proc.wait(timeout=5)

    print(f"\n{'='*80}")
    print("Summary:")
    ok = sum(1 for _, s in results if s.startswith("✓"))
    fail = sum(1 for _, s in results if s.startswith("✗"))
    print(f"  {ok} passed, {fail} failed")
    print(f"{'='*80}")

if __name__ == "__main__":
    main()
