# to11y

An MCP server over stdio, and a command line, that reports subscription
usage limits, pacing, reset times, and token usage from local logs for
Claude Code, Codex, Droid, and Antigravity. One binary crate, no async
runtime, no persistent database. `README.md` states the intent and the
budgets.

## Layout

| File | Role |
|---|---|
| `src/main.rs` | Arguments, then the server on stdin and stdout, or the command line |
| `src/cli.rs` | `to11y <tool>` and `to11y tools`: flags, one JSON document, exit status |
| `src/mcp.rs` | JSON-RPC framing, the legacy `initialize` lifecycle, and 2026-07-28 |
| `src/tools.rs` | The seventeen tools, their arguments, harness detection, result shapes |
| `src/model.rs` | `Harness`, `Window`, `Snapshot`, `Credits`, `Unavailable` |
| `src/pace.rs` | Pacing and scope rollups, pure functions of a window and `now` |
| `src/cache.rs` | The per-harness file cache, single flight, last good reading |
| `src/creds.rs` | Reading stored sign-ins, the `Keyring` trait, Droid's decrypt, JWT expiry |
| `src/providers/` | One module per harness: `fetch`, a pure `parse(json, now)`, and for Claude Code and Codex the harness's own `saved` reading |
| `src/http.rs` | HTTPS over HTTP/1.1 with rustls |
| `src/dbus.rs` | The Secret Service lookup over D-Bus |
| `src/base64.rs`, `src/time.rs` | Base64 decoding, RFC 3339 and durations |
| `src/tokens/mod.rs` | `Tokens`, `Event`, `Period`, the parallel log scan and dedupe |
| `src/tokens/<harness>.rs` | One module per harness: `files(env)` and a pure `parse(reader, path)` |
| `src/tokens/report.rs` | The JSON of each token tool, project names, scope to model |
| `src/env.rs` | The environment, read once at startup |
| `tests/mcp.rs` | The built binary driven by rmcp's client and by raw transcripts |
| `tests/cli.rs` | The built binary run as a command: output and exit status |
| `tests/fixtures/` | Redacted real provider answers, captured at 2026-11-23T14:31:10Z |
| `tests/fixtures/logs/home/` | Synthetic logs for all four harnesses, with planted ids |
| `scripts/oracle_tokens.py` | `just oracle-tokens`: token totals on real logs against ccusage |

## Commands

- Gate before handing off: `just check`, which is `cargo fmt --all --check`,
  `cargo clippy --all-targets --locked -- -D warnings`, and
  `cargo test --locked`.
- Oldest supported Rust: `just msrv`.
- After changing a log parser: `just oracle-tokens <ccusage binary>` on real
  logs. It must pass for 7 and 30 days.
- The tests need `dbus-daemon` on `PATH`.

## Hard rules

- No test contacts a provider host. Tests use the fixtures, a localhost TLS
  server, and stub programs.
- Tests never touch the real keyring or the real home and XDG directories.
  Use `testing::TempDir`, `MemoryKeyring`, and the private bus in the D-Bus
  test.
- to11y never refreshes or writes a sign-in. The only renewal is the one
  `agy -p /usage` does as a side effect, and the README says so.
- No token or account identifier is ever printed, logged, cached, or put in a
  tool result. Parsers drop identifiers. `Failed` names a status or an error
  kind, never a response body. Keep tokens in `creds::Secret`.
- Log parsers keep only usage fields, times, models, and folders. Session
  ids never leave the process, and no tool result or log line names a file
  path or quotes message text.
- Never trigger a keyring unlock prompt. A locked item is `KeyringLocked`.
- Server mode: only MCP messages go to stdout, one per line. Command line:
  one JSON document on stdout. Logs and usage errors go to stderr.
- `TO11Y_NOW` and `TO11Y_FIXTURES` are read only in debug builds. Release
  builds must never serve fixture data.
- Write our own code when a crate would bring much more than the job needs,
  and check it against an oracle its author didn't write: httparse for HTTP,
  jiff for time, the base64 crate, zbus for D-Bus, and rmcp for MCP. TLS and
  cryptography always come from crates.
- Docs and comments: plain words, sentence case headings, no em dashes,
  prose at most 80 columns.
- Test names are sentences. Inline `#[cfg(test)] mod tests` blocks go last
  in their file.
- Creating the GitHub repository, pushing, tagging, and publishing to
  crates.io each need the owner's go-ahead.
