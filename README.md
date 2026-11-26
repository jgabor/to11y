# to11y

to11y reports subscription limits, pace, reset times, and token usage for
Claude Code, Codex, Droid, and Antigravity on Linux. It runs as an MCP server
over stdio or as a command that prints JSON.

## Setup

```sh
cargo install --locked to11y
```

Add it to each agent:

```sh
claude mcp add --scope user to11y -- to11y
codex mcp add to11y -- to11y
droid mcp add to11y -- to11y
agy mcp add to11y -- to11y
```

For the Antigravity IDE, add `"to11y": { "command": "to11y" }` under
`mcpServers` in `~/.gemini/antigravity-ide/mcp_config.json`. Set
`TO11Y_HARNESS` for an MCP client to11y doesn't recognize.

## Tools

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
| `token_usage_session` | How many tokens has this session used? |
| `token_usage_recent` | How many tokens did I use lately? |
| `token_usage_by_model` | Which models used the most tokens? |
| `token_usage_by_project` | Which projects used the most tokens? |
| `token_usage_limit_windows` | How many tokens went into each current limit window? |
| `token_usage_all_harnesses` | How many tokens did each coding agent use? |
| `token_usage_sources` | Why are token counts missing? |

## Command line

```sh
to11y usage_limits --harness claude-code
to11y token_usage_recent --harness codex --period 7d --pretty
```

Run `to11y tools` to list the tools, and `to11y --help` for options and exit
codes.

## Stale results

When to11y can't check a harness but has an older reading, a limit tool
still answers, and its result says so. Read `stale` first:

- Without `stale`, the result is current.
- With `stale`, the top level holds only `harness` and `stale`. The values
  from the older reading are under `last_known`, with their `age`, so none
  of them passes for a current one.
- `rate_limit_status` also gives `"blocked": "unknown"`, and each scope
  `"blocked": null`. `usage_limits_all_harnesses` does the same for each
  stale harness.
- The command line exits 3.

`stale.reason` says why the check failed:

| Reason | Meaning |
|---|---|
| `sign_in_expired` | The sign-in expired. Using the agent renews it. |
| `sign_in_refused` | Refused, or `agy` couldn't renew it. Sign in again. |
| `keyring_locked`, `keyring_missing` | The keyring can't be read. |
| `not_installed`, `not_signed_in` | The agent or its sign-in is gone. |
| `unsupported` | The account has no limits to11y can read. |
| `failed` | Anything else, such as a network error. |

`stale.message` is a sentence to pass on to the user.

## Privacy and caching

to11y uses each agent's existing sign-in and local logs, follows
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and `GEMINI_HOME`, and calls each
provider's usage endpoint. The endpoints are undocumented and can change
without notice.

to11y never refreshes or writes a sign-in, with one exception: when
Antigravity's token has expired, it runs `agy -p /usage`, which renews it. It
never unlocks the keyring, so it never shows a prompt.

No token, email, account id, session id, file path, or message text appears
in results, logs, or the cache. The last reading of each harness is cached in
`~/.cache/to11y/` and reused for a minute, or 5 minutes for Claude Code.

## Budgets

| Metric | Budget | Measured |
|---|---|---|
| Handshake, including process start | under 10 ms | 0.18 ms |
| Binary | under 5 MB | 4.1 MB |
| Memory after `usage_limits_all_harnesses` | under 10 MB | 3.7 MB |
| `tools/list` | under 2,200 tokens | about 1,900 (7,629 bytes) |
| `token_usage_recent`, 7 days of Claude Code | under 500 ms | 158 ms |
| 30 days of Claude Code logs | under 5 s | 4.3 s for 5.2 GB |
| Memory during that scan | under 64 MB | 35 MB |

Measured on x86-64 Linux with the musl build and logs in the page cache.
Startup reads no files and makes no connections.

## License

MIT
