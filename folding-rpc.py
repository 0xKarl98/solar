#!/usr/bin/env python3
"""Local stdio folding probe; preserves responses and per-session latency samples."""
import argparse, hashlib, json, pathlib, queue, statistics, subprocess, tempfile, threading, time

class Server:
    def __init__(self, binary):
        self.child = subprocess.Popen([str(pathlib.Path(binary).resolve()), "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.messages = queue.Queue()
        self.id = 0
        threading.Thread(target=self.read, daemon=True).start()
    def read(self):
        try:
            while True:
                headers = {}
                while line := self.child.stdout.readline():
                    if line == b"\r\n": break
                    key, value = line.decode().split(":", 1)
                    headers[key.lower()] = value.strip()
                if not headers: return
                self.messages.put(json.loads(self.child.stdout.read(int(headers["content-length"]))))
        except Exception as e:
            self.messages.put(e)
    def send(self, method, params, request=False):
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if request:
            self.id += 1
            message["id"] = self.id
        body = json.dumps(message, separators=(",", ":")).encode()
        self.child.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        self.child.stdin.flush()
        return self.id
    def request(self, method, params):
        id = self.send(method, params, True)
        deadline = time.monotonic() + 60
        while True:
            message = self.messages.get(timeout=max(0.01, deadline-time.monotonic()))
            if isinstance(message, Exception): raise message
            if message.get("id") == id:
                if "error" in message: raise RuntimeError(message)
                return message["result"]
            if "method" in message and "id" in message:
                raise RuntimeError(f"unexpected server request {message}")
    def close(self):
        try:
            self.request("shutdown", None)
            self.send("exit", None)
            self.child.wait(timeout=5)
        finally:
            if self.child.poll() is None: self.child.kill()

parser = argparse.ArgumentParser()
parser.add_argument("--base", required=True)
parser.add_argument("--candidate", required=True)
parser.add_argument("--source", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--mode", choices=["open", "closed"], default="open")
parser.add_argument("--sessions", type=int, default=6)
parser.add_argument("--samples", type=int, default=12)
args = parser.parse_args()
source = pathlib.Path(args.source).read_text()
if args.mode == "open": source = "// revision 0\n" + source
results = []
expected = None
for session in range(args.sessions):
    for role in (["base", "candidate"] if session % 2 == 0 else ["candidate", "base"]):
        with tempfile.TemporaryDirectory(prefix="solar-folding-rpc-") as root:
            path = pathlib.Path(root) / "Input.sol"
            # Keep the requested closed document outside the initialized workspace so this
            # syntax-only request is measured independently of semantic background analysis.
            path.write_text(source)
            workspace = pathlib.Path(root) / "workspace"
            workspace.mkdir()
            server = Server(getattr(args, role))
            try:
                server.request("initialize", {"processId": None, "rootUri": workspace.as_uri(), "capabilities": {"textDocument": {"foldingRange": {}}}})
                server.send("initialized", {})
                if args.mode == "open":
                    server.send("textDocument/didOpen", {"textDocument": {"uri": path.as_uri(), "languageId": "solidity", "version": 1, "text": source}})
                    deadline = time.monotonic() + 60
                    while True:
                        message = server.messages.get(timeout=max(.01, deadline-time.monotonic()))
                        if message.get("method") == "textDocument/publishDiagnostics" and message["params"]["uri"] == path.as_uri(): break
                # Diagnostics can arrive before the analysis worker releases its old snapshot.
                # Settle that unmeasured setup work identically for both roles.
                time.sleep(0.1)
                samples = []
                for i in range(args.samples + 3):
                    start = time.perf_counter_ns()
                    if args.mode == "open":
                        server.send("textDocument/didChange", {"textDocument": {"uri": path.as_uri(), "version": i+2}, "contentChanges": [{"range": {"start": {"line": 0, "character": 12}, "end": {"line": 0, "character": 13}}, "text": str((i+1)%2)}]})
                    response = server.request("textDocument/foldingRange", {"textDocument": {"uri": path.as_uri()}})
                    elapsed = (time.perf_counter_ns() - start) / 1e6
                    if expected is None: expected = response
                    assert response == expected, "folding response changed"
                    if i >= 3: samples.append(elapsed)
                record = {"session": session, "role": role, "samples_ms": samples, "median_ms": statistics.median(samples), "ranges": len(response)}
                results.append(record)
                print(json.dumps(record), flush=True)
            finally: server.close()
pathlib.Path(args.output).write_text(json.dumps({"source": args.source, "mode": args.mode, "source_sha256": hashlib.sha256(source.encode()).hexdigest(), "response": expected, "sessions": results}, indent=2) + "\n")
