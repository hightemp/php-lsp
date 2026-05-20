#!/usr/bin/env python3
"""Quick LSP client to test go-to-definition on a real project."""

import json
import subprocess
import sys
import os
import time

SERVER_BIN = os.path.join(os.path.dirname(__file__), '..', 'client', 'bin', 'linux-x64', 'php-lsp')
STUBS_PATH = os.path.join(os.path.dirname(__file__), '..', 'client', 'stubs')
DEFAULT_TEST_FILE = os.environ.get('PHP_LSP_TEST_DEFAULT_FILE', 'index.php')


def resolve_project_paths(default_test_file):
    workspace_root = os.environ.get('PHP_LSP_TEST_WORKSPACE_ROOT')
    project_root = os.environ.get('PHP_LSP_TEST_PROJECT_ROOT')
    test_file = os.environ.get('PHP_LSP_TEST_FILE')

    if not workspace_root or not project_root:
        print(
            'Set PHP_LSP_TEST_WORKSPACE_ROOT and PHP_LSP_TEST_PROJECT_ROOT '
            'before running this script.'
        )
        sys.exit(2)

    if not test_file:
        test_file = os.path.join(project_root, default_test_file)
    elif not os.path.isabs(test_file):
        test_file = os.path.join(project_root, test_file)

    return (
        os.path.realpath(workspace_root),
        os.path.realpath(project_root),
        os.path.realpath(test_file),
    )


def load_test_cases():
    cases_file = os.environ.get('PHP_LSP_TEST_CASES_FILE')
    if not cases_file:
        return []

    with open(cases_file, 'r') as f:
        raw_cases = json.load(f)

    cases = []
    for raw_case in raw_cases:
        if isinstance(raw_case, dict):
            description = raw_case['description']
            line = raw_case['line']
            character = raw_case['character']
        else:
            description, line, character = raw_case
        cases.append((description, int(line), int(character)))

    return cases


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
    workspace_root, project_root, test_file = resolve_project_paths(DEFAULT_TEST_FILE)

    if not os.path.exists(server_path):
        print(f"Server not found at {server_path}")
        sys.exit(1)
    if not os.path.exists(test_file):
        print(f"Test file not found at {test_file}")
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

    root_uri = f"file://{workspace_root}"
    test_file_uri = f"file://{test_file}"

    # Read the test file
    with open(test_file, 'r') as f:
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
    print(f"\nOpening {test_file}")
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


    test_cases = load_test_cases()

    print(f"\n{'='*80}")
    print(f"Testing go-to-definition on {os.path.basename(test_file)}")
    print(f"{'='*80}\n")

    results = []
    if not test_cases:
        print("No go-to-definition cases configured.")

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
                    short_uri = uri.replace(f"file://{project_root}/", "")
                    status = f"✓ → {short_uri}:{target_line + 1}"
                else:
                    status = "✗ empty result"
            elif isinstance(result, dict):
                uri = result.get("uri", "")
                rng = result.get("range", {}).get("start", {})
                target_line = rng.get("line", "?")
                short_uri = uri.replace(f"file://{project_root}/", "")
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
