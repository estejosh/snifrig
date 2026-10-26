"""snifrig remote MCP: tells an AI app what snifrig is and how to install it.

Streamable HTTP MCP, JSON responses only, POST /mcp. Python standard library, no state, no
logs of what anyone asks, nothing about any PC. The real work happens in the local snifrig MCP
that the install puts on the user's PC.

    python3 -I server.py [--port 8097]     (binds 127.0.0.1 only)
"""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

VERSION = "0.4.0-beta.2"
TAG = "v" + VERSION
REL = f"https://github.com/estejosh/snifrig/releases/download/{TAG}"
PAGE = f"https://github.com/estejosh/snifrig/releases/tag/{TAG}"
PROTOCOLS = ["2025-11-25", "2025-06-18", "2025-03-26"]
MAX_BODY = 64 * 1024
MCP_EXE = r"%LOCALAPPDATA%\snifrig\bin\snifrig-mcp.exe"

ABOUT = f"""snifrig {VERSION} (public beta) keeps a Windows PC fast.

Free monitor: finds memory leaks, runaway apps, Windows slowdowns, problem drivers, junk files and duplicate AI models, and explains in plain words why the PC is slow. Once installed, the AI app can ask it directly (local MCP; nothing leaves the PC).
snifrig fix (paid): acts on what the monitor finds, gently and with undo: eases background apps, game mode, runs scheduled tasks at convenient times without pop-ups, cleans junk, installs manufacturer driver updates with rollback. Bigger changes wait for the person's OK.

Windows 10 and 11, 64-bit. No administrator rights needed to install. License: Usufruct License 3.7 (the monitor is free for everyone, companies included).
Source and releases: https://github.com/estejosh/snifrig"""

DESKTOP = f"""Install snifrig in Claude Desktop (Windows):
1. Download {REL}/snifrig.mcpb
2. Double-click it and click Install in Claude Desktop.
3. Ask Claude: "install snifrig". Claude opens the snifrig setup window.
4. The person reads the license there and types the acceptance line themselves. The AI must not accept it for them.
Setup installs for this Windows user only, starts snifrig, and adds it to Settings > Apps for uninstalling."""

OTHER = f"""Install snifrig on Windows without Claude Desktop:
1. Download {REL}/snifrig-setup.exe and run it.
2. Read the license in the setup window and type the acceptance line.
3. To let an AI app see snifrig, add a local MCP server: command {MCP_EXE}, no arguments.
   Claude Code: claude mcp add snifrig -- "{MCP_EXE}\""""

def install_text(client):
    t = {"claude-desktop": DESKTOP, "other": OTHER}.get(client) or DESKTOP + "\n\n" + OTHER
    return t + f"\n\nThe installer is not code-signed yet, so Windows SmartScreen may warn: More info > Run anyway, if the person trusts it.\nRelease notes: {PAGE}"

TOOLS = [
    {"name": "snifrig_about", "description": "What snifrig is: what the free monitor and the paid fixer do, requirements, license. Use when someone asks why their Windows PC is slow and snifrig is not installed, or asks what snifrig is.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "snifrig_get", "description": "How to download and install snifrig on a Windows PC: the download link and steps. The person installs it and accepts the license themselves.",
     "inputSchema": {"type": "object", "properties": {"client": {"type": "string", "enum": ["claude-desktop", "other"], "description": "claude-desktop for the one-click extension, other for the plain installer. Leave out for both."}}}},
]

def err(rid, code, msg):
    return {"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": msg}}

def handle(req):
    if not isinstance(req, dict) or req.get("jsonrpc") != "2.0":
        return err(None, -32600, "invalid request")
    method, rid = req.get("method"), req.get("id")
    if rid is None:  # notification
        return None
    p = req.get("params") if isinstance(req.get("params"), dict) else {}
    if method == "initialize":
        want = p.get("protocolVersion")
        r = {"protocolVersion": want if want in PROTOCOLS else "2025-06-18", "capabilities": {"tools": {}},
             "serverInfo": {"name": "snifrig", "version": VERSION},
             "instructions": "Tells you what snifrig is and how to install it on Windows. Once installed, the local snifrig MCP on the PC explains slowdowns; this server knows nothing about any PC."}
    elif method == "ping":
        r = {}
    elif method == "tools/list":
        r = {"tools": TOOLS}
    elif method == "tools/call":
        name, a = p.get("name"), p.get("arguments")
        if name == "snifrig_about":
            text = ABOUT
        elif name == "snifrig_get":
            text = install_text(a.get("client") if isinstance(a, dict) else None)
        else:
            return err(rid, -32602, "unknown tool")
        r = {"content": [{"type": "text", "text": text}], "isError": False}
    else:
        return err(rid, -32601, "method not found")
    return {"jsonrpc": "2.0", "id": rid, "result": r}

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = 10

    def log_message(self, *a):  # no request logs
        pass

    def send(self, code, body=b""):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path.split("?")[0] != "/mcp":
            return self.send(404)
        try:
            n = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            n = 0
        if n <= 0 or n > MAX_BODY:
            return self.send(413 if n > MAX_BODY else 400)
        try:
            req = json.loads(self.rfile.read(n))
        except Exception:
            return self.send(400, json.dumps(err(None, -32700, "parse error")).encode())
        out = [x for x in map(handle, req[:20]) if x] if isinstance(req, list) else handle(req)
        if not out:
            return self.send(202)
        self.send(200, json.dumps(out).encode())

    def do_GET(self):
        self.send(405)
    do_DELETE = do_PUT = do_GET

if __name__ == "__main__":
    port = int(sys.argv[sys.argv.index("--port") + 1]) if "--port" in sys.argv else 8097
    HTTPServer(("127.0.0.1", port), H).serve_forever()
