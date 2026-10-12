# snifrig AI integrations

`snifrig-mcp.exe` is a tiny MCP server (stdio, no network, no dependencies) that lets an AI assistant look at what snifrig measures and explain why your PC is slow. It starts only when the AI app launches it and stops when the app closes.

What the AI can do:

| Tool | What it does | Needs |
| --- | --- | --- |
| `snifrig_status` | Headline, causes, live CPU/RAM, GPU, what the governor is easing | free |
| `snifrig_why_slow` | snifrig's own diagnosis | free |
| `snifrig_report` | Slowdown, alert and leak summary for N days | free |
| `snifrig_top_processes` | Live per-process CPU, memory, handles | free |
| `snifrig_recent_activity` | What the fixer and governor recently did | free |
| `snifrig_alerts` | Recent alerts | free |
| `snifrig_rules` | Current fix rules and governor settings | free |
| `snifrig_pending` | Fixes waiting for your OK (read-only) | free |
| `snifrig_suggest` | Propose a rule or setting | snifrig fix |

The AI cannot change anything. There is no tool that applies a fix, approves a pending fix, or edits a rule. `snifrig_suggest` only writes a suggestion to `ai-suggestions.json` in the snifrig data folder; you see it in the snifrig window and choose Accept or Dismiss.

The server reads `%LOCALAPPDATA%\snifrig` (override with `--dir PATH`) and runs `snifrig.exe` and `snifrig-fix.exe` from its own folder (normally `%LOCALAPPDATA%\snifrig\bin`).

## Claude Desktop

Build `snifrig.mcpb` with `claude-desktop\build-extension.ps1` (needs Node.js; it copies `snifrig-mcp.exe` into `server\` and runs `npx -y @anthropic-ai/mcpb pack`). Then double-click `snifrig.mcpb` and click Install.

## Claude Code

Plugin (adds the server and a skill that teaches Claude how to diagnose a slow PC):

```
claude --plugin-dir X:\snifrig\integrations\claude-code
```

The plugin's `.mcp.json` points at `${LOCALAPPDATA}\snifrig\bin\snifrig-mcp.exe`. Claude Code expands `${VAR}` in `command`; if your setup does not, or snifrig is installed elsewhere, skip the plugin's server and register it directly:

```
claude mcp add snifrig -- "C:\Users\<you>\AppData\Local\snifrig\bin\snifrig-mcp.exe"
```

## ChatGPT and other AI apps

Any MCP client that can launch a local stdio server works. Point it at `snifrig-mcp.exe` with no arguments. Typical JSON config:

```json
{ "mcpServers": { "snifrig": { "command": "C:\\Users\\<you>\\AppData\\Local\\snifrig\\bin\\snifrig-mcp.exe", "args": [] } } }
```

ChatGPT's own connectors need a remote (HTTPS) server, so snifrig does not work there yet.

## Privacy

Nothing leaves your PC except what the AI app itself sends to its model: the text the tools return (process names, alert text). The license key file is never read or printed.
