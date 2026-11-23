# to11y

to11y tells a coding agent how much of its subscription it has used, whether
it is using it too fast, and when its limits reset. It is a small MCP server
over stdio for Claude Code, Codex, Droid, and Antigravity.

The numbers come from each provider's own usage endpoint, read with the
sign-in the harness already stores. Nothing is estimated from logs.

## Setup

Install the binary with cargo:

```sh
cargo install --locked to11y
```

Then add it to each agent. to11y runs with no arguments:

```sh
claude mcp add --scope user to11y -- to11y
codex mcp add to11y -- to11y
droid mcp add to11y -- to11y
agy mcp add to11y -- to11y
```

If an agent can't find `to11y` on its `PATH`, give the full path instead,
such as `~/.cargo/bin/to11y`.

`agy mcp add` sets up the Antigravity CLI. The Antigravity IDE reads its own
list, so if you use the IDE, add to11y under `mcpServers` in
`~/.gemini/antigravity-ide/mcp_config.json` too:

```json
"to11y": { "command": "to11y" }
```

## Tools

Every tool is read-only and takes an optional `harness`: `claude-code`,
`codex`, `droid`, or `antigravity`. Without one, it reports on the agent that
is calling. `TO11Y_HARNESS` sets the default for agents to11y can't recognize.

| Tool | Answers |
|---|---|
| `usage_limits` | How much of my subscription have I used? |
| `usage_pace` | Am I using quota too fast? |
| `usage_forecast` | Will I run out before the reset, and when? |
| `usage_reset_times` | When do my limits reset? |
| `usage_remaining_quota` | How much is left, and which limit is tightest? |
| `rate_limit_status` | Am I blocked right now, and until when? |
| `usage_extra_credits` | Do I have overage or credits? |
| `subscription_plan` | Which plan am I on? |
| `usage_limits_all_harnesses` | Which coding agent has headroom left? |
| `usage_login_status` | Why is usage data missing or stale? |

Times come as durations such as `6d 19h`, so the agent does no date math.

Limits are grouped by scope: a pool such as Droid's Standard and Core, or a
model group such as Antigravity's Gemini models. Scope `all` limits every
other scope. A used-up window blocks its scope, and the other windows in that
scope then read `blocked`.

Pace compares use with the share of the window that has passed, as if use
were spread evenly over the whole window. Within 5 points either way is on
pace. Forecasts wait until 5% of a window has passed.

## What it reads and calls

| Harness | Sign-in | Call |
|---|---|---|
| Claude Code | `~/.claude/.credentials.json`, or `$CLAUDE_CONFIG_DIR` | `api.anthropic.com/api/oauth/usage` |
| Codex | `~/.codex/auth.json`, or `$CODEX_HOME` | `chatgpt.com/backend-api/wham/usage` |
| Droid | `~/.factory/auth.v2.keyring`, with its key in the system keyring | `api.factory.ai/api/billing/limits` |
| Antigravity | The system keyring | `cloudcode-pa.googleapis.com` quota summary |

None of these endpoints is documented, so any of them can change without
notice. to11y then says the call failed and shows the last good reading.

to11y never refreshes or writes a sign-in. An expired sign-in is renewed by
using that harness. The one exception is Antigravity: when its one-hour token
has expired, to11y runs Antigravity's own `agy -p /usage --output-format
json`, and agy renews the stored sign-in as a side effect.

The keyring is read through the Secret Service on the session bus. to11y
never unlocks a collection, so it never shows a prompt. A locked keyring is
reported as locked.

Requests to Anthropic, OpenAI, and Factory send `to11y/<version>` as the user
agent. Google refuses that, so the Antigravity call sends Antigravity's own.

## Privacy and the cache

No token, email, or account identifier reaches a tool result, a log line, or
the cache. They are dropped where the provider's answer is read.

The last reading of each harness is kept in
`$XDG_CACHE_HOME/to11y/<harness>.json`, or `~/.cache/to11y/`. The folder is
readable only by you. A reading is reused for 60 seconds, or 5 minutes for
Claude Code, so parallel calls cost one request. Anthropic limits how often
its usage endpoint can be asked. A failure is held just as long, or for as
long as the provider's `Retry-After` asks. The files are safe to delete at
any time.

## Limits

- Linux only for now. macOS and Windows keep sign-ins in places to11y doesn't
  read yet.
- Codex signed in with an API key has no subscription limits to show.
- Droid sign-ins kept in a file instead of the keyring aren't read yet.
- Even spending is a simple baseline. Early in a week, a normal working day
  can read as over pace.

## Budgets

Set before measuring, and measured on x86-64 Linux with the musl build:

| Budget | Measured |
|---|---|
| Handshake under 10 ms, including process start | 0.37 ms |
| Binary under 5 MB | 3.7 MB |
| Memory under 10 MB after `usage_limits_all_harnesses` | 3.4 MB |
| `tools/list` under 1,500 tokens | about 1,050 (4,217 bytes) |

Startup reads no files and makes no connections. All work happens inside a
tool call.

## Development

`just check` runs the gate: `cargo fmt --all --check`, clippy with every
warning as an error, and the tests. The tests need `dbus-daemon`, and never
contact a provider or touch your real keyring, home, or cache. See
`AGENTS.md` for the rules.

## License

MIT. See `LICENSE`.
