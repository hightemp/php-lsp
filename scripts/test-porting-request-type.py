#!/usr/bin/env python3
r"""LSP test: comprehensive go-to-definition and diagnostics for PortingRequestType.php.

Tests:
 - use statement go-to-def (including aliased `use ... as Assert`)
 - new Assert\NotBlank -- aliased qualified name in object_creation_expression
 - closure parameter type resolution ($er->createQueryBuilder, $subscriber->getLastName)
 - class references (extends, ::class, type hints)
 - diagnostics (no unresolved use statements)
"""

import json
import subprocess
import sys
import os
import time

SERVER_BIN = os.path.join(os.path.dirname(__file__), '..', 'client', 'bin', 'linux-x64', 'php-lsp')
STUBS_PATH = os.path.join(os.path.dirname(__file__), '..', 'client', 'stubs')
WORKSPACE_ROOT = '/home/apanov/Projects/bdpn-ui'
PROJECT_ROOT = '/home/apanov/Projects/bdpn-ui/app'
TEST_FILE = os.path.join(PROJECT_ROOT, 'src', 'Form', 'PortingRequestType.php')


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
                break
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
    return None


def main():
    server_path = os.path.realpath(SERVER_BIN)
    stubs_path = os.path.realpath(STUBS_PATH)

    if not os.path.exists(server_path):
        print(f"Server not found at {server_path}")
        sys.exit(1)

    print(f"Starting server: {server_path}")
    log_file = open('test-porting-lsp.log', 'w')
    proc = subprocess.Popen(
        [server_path],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=log_file,
        env=os.environ.copy(),
    )

    root_uri = f"file://{WORKSPACE_ROOT}"
    test_file_uri = f"file://{TEST_FILE}"

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
        print(f"Initialize OK")
    else:
        print("Initialize FAILED")
        proc.kill()
        sys.exit(1)
    req_id += 1

    send_notification(proc, "initialized", {})
    time.sleep(15)  # Wait for indexing

    # Open test file
    print(f"\nOpening {TEST_FILE}")
    send_notification(proc, "textDocument/didOpen", {
        "textDocument": {
            "uri": test_file_uri,
            "languageId": "php",
            "version": 1,
            "text": test_content,
        }
    })

    # Collect diagnostics
    published_diagnostics = []
    diag_deadline = time.time() + 10
    while time.time() < diag_deadline:
        msg = read_response(proc, timeout=diag_deadline - time.time())
        if msg is None:
            break
        if msg.get("method") == "textDocument/publishDiagnostics":
            params = msg.get("params", {})
            if params.get("uri") == test_file_uri:
                published_diagnostics = params.get("diagnostics", [])
                break

    if published_diagnostics:
        unresolved = [d for d in published_diagnostics if "Unresolved" in d.get("message", "")]
        print(f"\n  Diagnostics: {len(published_diagnostics)} total, {len(unresolved)} unresolved use")
        for d in published_diagnostics:
            line = d["range"]["start"]["line"] + 1
            print(f"    L{line}: {d['message']}")
    else:
        print(f"\n  Diagnostics: 0 (clean)")

    # All line numbers below are 0-based
    test_cases = [
        # === Use statement go-to-definition ===
        ("use App\\Entity\\Operator", 6, 15),              # L7: Operator
        ("use App\\Entity\\PortingRequest", 7, 15),         # L8: PortingRequest
        ("use App\\Entity\\PortingRequestTypes", 8, 15),    # L9: PortingRequestTypes
        ("use App\\Entity\\Subscriber", 9, 15),             # L10: Subscriber
        ("use App\\Repository\\PortingRequestTypesRepository", 10, 19),  # L11
        ("use Doctrine\\ORM\\EntityRepository", 11, 17),    # L12
        ("use Symfony\\...\\EntityType", 12, 38),            # L13
        ("use Symfony\\...\\AbstractType", 13, 27),          # L14
        ("use Symfony\\...\\FormBuilderInterface", 19, 27),  # L20
        ("use Symfony\\...\\OptionsResolver", 20, 22),       # L21

        # === Extends / type hints ===
        ("extends AbstractType", 23, 39),                    # L24
        ("FormBuilderInterface type hint", 25, 30),          # L26
        ("OptionsResolver type hint", 198, 37),              # L199

        # === Class::class references ===
        ("EntityType::class", 28, 33),                       # L29
        ("PortingRequestTypes::class", 29, 27),              # L30
        ("Subscriber::class", 50, 27),                       # L51
        ("PortingRequest::class", 201, 28),                  # L202

        # === Aliased use: new Assert\NotBlank ===
        ("new Assert\\NotBlank L39 (NotBlank)", 38, 31),     # L39
        ("new Assert\\NotBlank L39 (Assert)", 38, 24),       # L39 cursor on Assert
        ("new Assert\\NotBlank L71", 70, 31),                # L71
        ("new Assert\\NotBlank L179", 178, 31),              # L179
        ("new Assert\\Length L191", 190, 31),                 # L191

        # === Closure parameter type hints ===
        ("PortingRequestTypesRepository param L41", 40, 52), # L41
        ("PortingRequestTypes param L45", 44, 50),           # L45
        ("Subscriber param L52", 51, 51),                    # L52
        ("EntityRepository param L73", 72, 52),              # L73

        # === Method calls on closure params ===
        ("$er->createQueryBuilder L42", 41, 32),             # L42
        ("$type->getProcessTypeCode L46", 45, 63),           # L46
        ("$subscriber->getType L53", 52, 51),                # L53
        ("$subscriber->getOrganizationName L53", 52, 77),    # L53
        ("$subscriber->getLastName L59", 58, 37),            # L59
        ("$subscriber->getFirstName L60", 59, 37),           # L60
        ("$er->createQueryBuilder L74", 73, 32),             # L74
    ]

    print(f"\n{'='*80}")
    print(f"Testing go-to-definition on PortingRequestType.php ({len(test_cases)} cases)")
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
    diag_ok = len(published_diagnostics) == 0
    print(f"  Go-to-def: {ok}/{len(results)} passed, {fail} failed")
    print(f"  Diagnostics: {'0 (clean)' if diag_ok else f'{len(published_diagnostics)} issues'}")
    print(f"{'='*80}")

    if fail > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
