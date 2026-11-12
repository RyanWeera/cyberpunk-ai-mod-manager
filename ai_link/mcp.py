"""MCP server on stdin/stdout, so AI apps (Claude Desktop, Claude Code, Cursor and others) can use the mod manager.
The AI app starts `CP2077 AI Mod Manager.exe --mcp`; every tool call is passed to the manager window (started if
needed), so the window stays the only process that changes the mods. Messages are JSON-RPC, one per line."""
import json
import sys

from ai_link.tools import TOOLS


def reply(msg, call, version):
    """The answer to one JSON-RPC message, or None for a notification."""
    if "id" not in msg:
        return None
    method, params = msg.get("method"), msg.get("params")
    params = params if isinstance(params, dict) else {}
    if method == "initialize":  # the tools work the same in every protocol version, so take the app's
        result = {"protocolVersion": params.get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}},
                  "serverInfo": {"name": "cp2077-ai-mod-manager", "version": version}}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        try:
            text, failed = call(params["name"], params.get("arguments") or {}), False
        except Exception as e:  # noqa: BLE001 - a failed tool is a result the AI app shows, not a dead server
            text, failed = str(e) or type(e).__name__, True
        result = {"content": [{"type": "text", "text": text}], "isError": failed}
    elif method == "ping":
        result = {}
    else:
        return {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": f"Unknown method: {method}"}}
    return {"jsonrpc": "2.0", "id": msg["id"], "result": result}


def serve(call, version, inp=None, out=None):
    """call(tool name, arguments) -> text. Runs until the AI app closes stdin."""
    inp, out = inp or sys.stdin.buffer, out or sys.stdout.buffer
    for line in inp:
        if not line.strip():
            continue
        try:
            msg = json.loads(line)
            answer = reply(msg, call, version) if isinstance(msg, dict) else None
        except ValueError:
            answer = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "Not JSON"}}
        if answer:
            out.write(json.dumps(answer).encode() + b"\n")
            out.flush()
