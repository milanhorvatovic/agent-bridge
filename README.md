# agent-bridge — the AI CLI runtime

[![ci](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml)

A cross-platform runtime layer that lets tools and agents drive **interactive AI CLIs** (Claude Code and Codex CLI in v1) over a structured, event-normalized JSON-RPC protocol — while the CLIs keep running as the real, subscription-billed, human-visible interactive sessions their vendors expect.

It is infrastructure, not an orchestration platform: sessions are hosted under a PTY, approvals always surface to a human (no silent auto-approval), and events — tokens, tool-call lifecycle, approval prompts with correlation IDs, lifecycle, errors — arrive as a versioned, namespaced taxonomy instead of raw terminal bytes. For CLIs that expose in-session structured channels (Claude Code's hooks and session transcript), those are the primary event sources; the terminal is host, input, interrupt, and fallback.

**Status: validation.** Written in Rust. There is no runtime yet — what exists is the CI matrix (Linux / macOS / Windows), the workspace scaffold, and the first probe: `tools/pty-probe` allocates a real PTY (ConPTY on Windows), spawns a child under it, reads the output back, and tears down cleanly on every supported OS. Interactive-CLI, interrupt, resize, and cleanup probes land next; a public schema and conformance-trace artifacts follow before the runtime work begins.

## License

[MIT](LICENSE)
