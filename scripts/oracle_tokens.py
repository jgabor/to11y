"""Check to11y's token totals against oracles it didn't write, on this
machine's real logs. Prints totals and counts only, never ids, paths, or
message text.

Usage: python3 -I scripts/oracle_tokens.py <ccusage binary> [days] [end]

`days` is 1, 7, or 30 (default 7). `end` is a UTC date, YYYY-MM-DD, and the
range is the `days` whole UTC days before it (default today). Run it from
the repository root after `cargo build`.

- Claude Code, Codex, and Antigravity are compared with ccusage. ccusage
  runs with telemetry off and, where `unshare` works, with no network.
- Droid is compared with a second reading of its log, and that reading
  with Factory's own per-session totals in `settings.json`.
"""

import glob
import json
import os
import shutil
import subprocess
import sys
from datetime import datetime, timedelta, timezone

CHANNELS = ["input", "cache_read", "cache_write", "output", "total"]
CCUSAGE = {
    "input": "inputTokens",
    "cache_read": "cacheReadTokens",
    "cache_write": "cacheCreationTokens",
    "output": "outputTokens",
    "total": "totalTokens",
}
PERIODS = {1: "24h", 7: "7d", 30: "30d"}


def ccusage(binary, agent, first, last):
    env = dict(os.environ, CCUSAGE_TELEMETRY_DISABLED="1", DO_NOT_TRACK="1")
    command = [binary, agent, "daily", "--json", "--offline", "--no-cost"]
    command += ["-z", "UTC", "-s", first, "-u", last]
    if shutil.which("unshare") and subprocess.run(
        ["unshare", "-rn", "true"], capture_output=True
    ).returncode == 0:
        command = ["unshare", "-rn"] + command
    out = subprocess.run(command, env=env, capture_output=True, check=True)
    totals = json.loads(out.stdout)["totals"]
    return {c: int(totals.get(CCUSAGE[c]) or 0) for c in CHANNELS}


def to11y(harness, period, now):
    env = dict(os.environ, TO11Y_NOW=now)
    server = subprocess.Popen(
        ["target/debug/to11y"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=env,
        text=True,
    )

    def send(message):
        server.stdin.write(json.dumps(message) + "\n")
        server.stdin.flush()

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "oracle", "version": "0"}}})
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "token_usage_recent",
        "arguments": {"harness": harness, "period": period}}})
    # The server stops reading at end of input, so stdin stays open until
    # the reply arrives.
    for line in server.stdout:
        reply = json.loads(line)
        if reply.get("id") == 2:
            break
    server.stdin.close()
    server.wait()
    tokens = json.loads(reply["result"]["content"][0]["text"])["tokens"]
    return {c: tokens.get(c, 0) for c in CHANNELS}


def droid_log():
    """Droid's requests, deduplicated the way to11y does: by response id,
    keeping the largest value of each channel."""
    fields = ["inputTokens", "cacheReadInputTokens", "cachedTokensWritten",
              "outputTokens"]
    requests, anonymous = {}, 0
    pattern = os.path.expanduser("~/.factory/logs/droid-log-single.log*")
    for path in glob.glob(pattern):
        with open(path, "rb") as log:
            for raw in log:
                if b"Streaming result" not in raw:
                    continue
                line = raw.decode("utf-8", "replace")
                try:
                    at = datetime.fromisoformat(line[1:line.index("]")])
                    context = json.loads(line[line.index("Context: ") + 9:])
                except ValueError:
                    continue
                counts = [int(context.get(f) or 0) for f in fields]
                session = (context.get("tags") or {}).get("sessionId") or ""
                key = context.get("upstreamResponseId")
                if key is None:
                    anonymous += 1
                    key = ("anonymous", anonymous)
                if key in requests:
                    old = requests[key][2]
                    requests[key][2] = [max(a, b) for a, b in zip(old, counts)]
                else:
                    requests[key] = [at.timestamp(), session, counts]
    return list(requests.values())


def droid_range(requests, since, until):
    sums = [0, 0, 0, 0]
    for at, _, counts in requests:
        if since <= at <= until:
            sums = [a + b for a, b in zip(sums, counts)]
    return dict(zip(CHANNELS, sums + [sum(sums)]))


def droid_sessions(requests):
    """The log's per-session sums beside Factory's, for sessions that
    started at least a day after the oldest line the log still has."""
    oldest = min(at for at, _, _ in requests)
    sessions = {}
    for at, session, counts in requests:
        first, sums = sessions.get(session, (at, [0, 0, 0, 0]))
        sessions[session] = (min(first, at),
                             [a + b for a, b in zip(sums, counts)])
    fields = ["inputTokens", "cacheReadTokens", "cacheCreationTokens",
              "outputTokens"]
    log, factory, compared = [0] * 4, [0] * 4, 0
    pattern = os.path.expanduser("~/.factory/sessions/*/*.settings.json")
    for path in glob.glob(pattern):
        session = os.path.basename(path)[: -len(".settings.json")]
        if session not in sessions or sessions[session][0] < oldest + 86400:
            continue
        try:
            with open(path) as file:
                usage = json.load(file).get("tokenUsage") or {}
        except (OSError, ValueError):
            continue
        compared += 1
        log = [a + b for a, b in zip(log, sessions[session][1])]
        factory = [a + int(usage.get(f) or 0) for a, f in zip(factory, fields)]
    return (compared, dict(zip(CHANNELS, log + [sum(log)])),
            dict(zip(CHANNELS, factory + [sum(factory)])))


def compare(name, ours, theirs, skip=()):
    print(f"{name}")
    worst = 0.0
    for channel in CHANNELS:
        if channel in skip:
            continue
        a, b = ours[channel], theirs[channel]
        gap = abs(a - b) / max(b, 1) * 100
        worst = max(worst, gap)
        print(f"  {channel:<12}{a:>16,}{b:>16,}{gap:>8.2f}%")
    return worst <= 1.0


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    binary = sys.argv[1]
    days = int(sys.argv[2]) if len(sys.argv) > 2 else 7
    if days not in PERIODS:
        sys.exit("days is 1, 7, or 30")
    if len(sys.argv) > 3:
        end = datetime.fromisoformat(sys.argv[3]).replace(tzinfo=timezone.utc)
    else:
        end = datetime.now(timezone.utc).replace(
            hour=0, minute=0, second=0, microsecond=0)
    start = end - timedelta(days=days)
    first = start.strftime("%Y%m%d")
    last = (end - timedelta(days=1)).strftime("%Y%m%d")
    now = end.strftime("%Y-%m-%dT%H:%M:%SZ")
    period = PERIODS[days]
    print(f"{days} UTC days from {start:%Y-%m-%d}, to11y then oracle")
    passed = True
    for harness, agent, skip in [
        ("claude-code", "claude", ()),
        ("codex", "codex", ()),
        # Antigravity's transcript folds thinking into output; ccusage's
        # database reading splits it out. The total includes both.
        ("antigravity", "antigravity", ("output",)),
    ]:
        ours = to11y(harness, period, now)
        passed &= compare(f"{harness} vs ccusage", ours,
                          ccusage(binary, agent, first, last), skip)
    requests = droid_log()
    ours = to11y("droid", period, now)
    rule = droid_range(requests, start.timestamp(), end.timestamp())
    passed &= compare("droid vs a second reading of its log", ours, rule)
    compared, log, factory = droid_sessions(requests)
    # Not part of the verdict: the log leaves out some uncached calls.
    compare(f"droid log vs Factory's totals, {compared} sessions", log, factory)
    print("pass" if passed else "FAIL")
    sys.exit(0 if passed else 1)


main()
