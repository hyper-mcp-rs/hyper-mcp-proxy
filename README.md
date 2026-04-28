# hyper-mcp-proxy

A lightweight proxy that bridges MCP clients speaking **Streamable HTTP** to
backend MCP servers that communicate over **stdio**.

For every incoming MCP session the proxy spawns a dedicated child process,
performs the MCP handshake on its behalf, and then transparently forwards all
subsequent requests, responses, and notifications between the two sides.

```
MCP Client ──Streamable HTTP──▶ hyper-mcp-proxy ──stdio──▶ Child MCP Server
                                 (one child per session)
```

## Features

- **Full MCP protocol support** — tools, resources, resource templates,
  prompts, completions, logging, subscriptions, cancellation, and progress
  notifications are all forwarded.
- **One child per session** — each `Mcp-Session-Id` gets its own isolated
  child process. Sessions never share state.
- **Automatic capability discovery** — on connect the proxy probes the child
  server and advertises only the capabilities it actually supports.
- **No authentication** — the proxy itself performs no auth. Layer it in front
  of a reverse proxy or middleware if you need it.
- **Configurable endpoint** — defaults to `/mcp` but can be changed via CLI.
- **Graceful shutdown** — `Ctrl-C` cancels all active sessions and waits for
  in-flight requests to drain.

## Installation

```sh
cargo install --path .
```

Or build from source:

```sh
cargo build --release
# Binary is at target/release/hyper-mcp-proxy
```

## Usage

```
hyper-mcp-proxy [OPTIONS] -- <COMMAND>...
```

Everything after `--` is treated as the stdio MCP server command (program +
arguments) that will be spawned once per session.

### Options

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--host` | `-H` | `127.0.0.1` | Address to bind the HTTP listener to |
| `--port` | `-p` | `8080` | Port to bind to |
| `--endpoint` | `-e` | `/mcp` | URL path the MCP service is mounted on |

### Examples

Proxy a Node-based MCP server:

```sh
hyper-mcp-proxy -- npx -y @anthropic-ai/mcp-server-memory
```

Bind to all interfaces on port 3000 with a custom endpoint:

```sh
hyper-mcp-proxy -H 0.0.0.0 -p 3000 -e /v1/mcp -- my-mcp-server --flag value
```

Point the [MCP Inspector](https://github.com/modelcontextprotocol/inspector)
at the running proxy:

```sh
# Terminal 1
hyper-mcp-proxy -p 8080 -- my-mcp-server

# Terminal 2 — connect the inspector
npx @anthropic-ai/mcp-inspector --transport streamableHttp --url http://localhost:8080/mcp
```

### Logging

The proxy uses [`tracing`](https://crates.io/crates/tracing) with an
env-filter. Control verbosity via the `RUST_LOG` environment variable:

```sh
# Default (info for most crates, debug for the proxy)
RUST_LOG=info,hyper_streamable_http=debug hyper-mcp-proxy -- my-server

# Silence everything except errors
RUST_LOG=error hyper-mcp-proxy -- my-server

# Full trace output (very verbose)
RUST_LOG=trace hyper-mcp-proxy -- my-server
```

## Architecture

### Session lifecycle

```
 HTTP Client                  Proxy                        Child Process
     │                          │                               │
     │──POST /mcp (initialize)─▶│                               │
     │                          │──spawn + stdio handshake─────▶│
     │                          │◀──InitializeResult────────────│
     │                          │  (probe capabilities)         │
     │◀─200 InitializeResult────│                               │
     │                          │                               │
     │──POST /mcp (tools/list)─▶│──tools/list──────────────────▶│
     │◀─200 ListToolsResult─────│◀─ListToolsResult──────────────│
     │                          │                               │
     │──POST /mcp (tools/call)─▶│──tools/call──────────────────▶│
     │◀─200 CallToolResult──────│◀─CallToolResult───────────────│
     │                          │                               │
     │──DELETE /mcp────────────▶│  (session torn down)          │
     │                          │──kill child──────────────────▶│✗
     │◀─200 OK──────────────────│                               │
```

### Key components

| Module | Responsibility |
|--------|---------------|
| `src/main.rs` | CLI parsing (`clap`), HTTP server (`axum`), graceful shutdown |
| `src/proxy.rs` | `ProxyHandler` — implements `rmcp::ServerHandler`, one instance per session. Spawns the child process, holds the `Peer<RoleClient>` for forwarding |

### Crate dependencies

| Crate | Role |
|-------|------|
| [`rmcp`](https://crates.io/crates/rmcp) | MCP protocol types, `StreamableHttpService`, `TokioChildProcess` transport |
| [`axum`](https://crates.io/crates/axum) | HTTP framework (native `tower::Service` integration) |
| [`tokio`](https://crates.io/crates/tokio) | Async runtime |
| [`clap`](https://crates.io/crates/clap) | CLI argument parsing |
| [`tracing`](https://crates.io/crates/tracing) | Structured logging with spans |

## Limitations

- **Server → client notifications** (e.g. `notifications/tools/list_changed`)
  from the child process are currently **not** forwarded back to the HTTP
  client. The child's notifications are received by the internal rmcp client
  handler but are silently dropped. A future version will add a
  `ClientHandler` implementation that bridges these back through the server
  peer.

- **Custom JSON-RPC methods** that are not part of the standard MCP protocol
  cannot be proxied because the proxy operates at the typed-message level
  rather than raw JSON-RPC. They will receive a `METHOD_NOT_FOUND` error.

- **Sampling / elicitation requests** from the child server (server → client
  direction) are not currently forwarded.

## License

[MIT](LICENSE)
