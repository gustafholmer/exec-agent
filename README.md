# exec-agent

A personal executive assistant that runs as a background daemon on macOS. It
watches a handful of services, decides what is worth your attention, and asks
before it changes anything.

The whole system is built around one rule: **a language model proposes; it never
performs.** Every write to the outside world becomes a row in a ledger, passes a
policy gate, and — unless you have explicitly pre-authorised that exact tool —
waits for you to tap Approve on your phone.

## What it does today (Phase 1)

- Polls each configured connector on its own interval and records what changed
  as `events`. One connector ships so far: [Canvas](connectors/canvas/README.md),
  read-only, for coursework deadlines.
- Triages those events every five minutes: a free rules pass (tier 0), then one
  cheap model session (tier 1, `claude-haiku-4-5`) that scores what survives.
- Interrupts you on Telegram when something scores above the threshold, outside
  quiet hours, and within the hourly rate limit. Everything else is held for a
  digest.
- Accepts proposals from model sessions, runs the ones policy marks `auto`, and
  queues the rest for approval — in Telegram, or with `ea approve`.
- Answers `ea chat` by running a session that can read, think, and propose.

## Prerequisites

- **Rust**, stable. This workspace needs 1.88 or newer.
  (If `rustc` on your `PATH` is broken, use rustup's:
  `PATH="$HOME/.cargo/bin:$PATH" cargo …`.)
- **A logged-in `claude` CLI.** The daemon shells out to `claude -p` for triage
  and chat. Run `claude` once interactively and sign in. Without it the daemon
  still starts, still polls, and still gates actions — it just does no thinking,
  and says so at startup and in `ea status`.
- **macOS**, for the `launchd` service. Nothing else is macOS-specific.

## Build

```bash
cargo build --release
```

That produces `target/release/ea-daemon`, `target/release/ea`, and one binary
per connector (`ea-canvas`, plus the `ea-propose` MCP server). The daemon
resolves connector binaries relative to itself, so a release build needs no
install step.

## Configure

### The daemon

Everything is optional; a missing file means defaults.

```bash
mkdir -p ~/.config/exec-agent
cat > ~/.config/exec-agent/config.toml <<'TOML'
# Sessions a day, across triage and chat. A ceiling on spend, not a target.
daily_session_budget = 60

[notify]
threshold    = 60        # salience at or above which you get interrupted
quiet_start  = "22:00"   # local wall clock, in the zone below
quiet_end    = "07:00"
max_per_hour = 3
time_zone    = "Europe/Stockholm"

[tier0]
muted_sources = []
muted_kinds   = []
keywords      = ["invoice"]   # rescues a muted event that mentions one
TOML
```

### Telegram

The bot is how you approve things away from your desk.

1. Talk to [@BotFather](https://t.me/botfather), `/newbot`, copy the token.
2. Message your new bot once, then read your own numeric user id (for example
   from [@userinfobot](https://t.me/userinfobot)).

```bash
cd ~/.config/exec-agent
printf '%s\n' '123456:YOUR-BOT-TOKEN' > telegram.token && chmod 600 telegram.token
printf '%s\n' '111111111' > telegram.chat_id    # where messages are sent
printf '%s\n' '111111111' > telegram.owner_id   # who may press the buttons
```

`telegram.owner_id` is a **user** id and `telegram.chat_id` is a **chat** id.
They are the same number in a one-to-one chat with the bot and different in a
group, which is exactly why they are separate files: the owner id is the
allow-list, and only callbacks whose `from.id` matches it are acted on.

The token file must be mode `0600` or the daemon refuses to start. (You may
instead put `chat_id`/`owner_id`, and even `token`, in a `[telegram]` block in
`config.toml` — but a `config.toml` containing a token is held to the same
`0600` standard.)

### Connectors

Each connector is a directory under `connectors/` holding a `connector.toml`
and a `policy.toml`. **A directory without a `policy.toml` is not a connector**
and is skipped: an unpoliced connector would be a hole in the gate.

Credentials live in `~/.config/exec-agent/<connector>/`, never in the
repository. See each connector's own README:

- [Canvas](connectors/canvas/README.md) — a Canvas access token.

## Install as a service

```bash
./scripts/install-launchd.sh
```

Builds `--release`, writes `~/Library/LaunchAgents/dev.gustaf.exec-agent.plist`
with `RunAtLoad` and `KeepAlive`, and loads it. Run it again after any change;
`--uninstall` removes it.

The plist's `PATH` is load-bearing and is filled in from whatever shell you run
the script in: `launchd` gives a job a minimal environment, and the daemon
shells out to `claude`. If `claude` is not on your `PATH` when you run the
script, it refuses rather than installing a daemon that cannot think.

## The `ea` command

| command | what it does |
|---|---|
| `ea status` | up? paused? how many proposals are waiting? which jobs have tripped, and why |
| `ea queue` | the proposals waiting for a decision |
| `ea approve <id>` | approve one and run it |
| `ea reject <id> [--reason "..."]` | reject one; nothing is called |
| `ea log [-n 20]` | recent runs: sessions and executed actions, with cost |
| `ea pause` | stop the scheduler starting new work (in-flight work finishes) |
| `ea resume` | undo `pause` |
| `ea chat <message...>` | say something to the assistant |

`ea queue` is formatted for reading; everything else prints the daemon's JSON.

## Where things live

| what | where |
|---|---|
| configuration and credentials | `~/.config/exec-agent/` |
| database (`events`, `actions`, `runs`, …) | `~/.local/state/exec-agent/state.db` |
| control socket (mode `0600`) | `~/.local/state/exec-agent/daemon.sock` |
| daemon logs | `~/.local/state/exec-agent/daemon.{out,err}.log` |
| session working directory | `~/.local/state/exec-agent/sessions/` |

Both directories are created mode `0700`. `$EA_CONFIG_DIR` and `$EA_STATE_DIR`
override them, which is how the tests avoid touching yours.

## How the gate works

Read this before changing anything.

A model session runs as a `claude -p` subprocess with `Write`, `Edit` and `Bash`
removed and exactly one MCP tool allowed: `propose_action`, served by the
`ea-propose` binary. That tool does no work. It forwards the proposal to the
daemon over the control socket, and the daemon puts it through
`Policy::decide`, which reads the merged `policy.toml` of every discovered
connector:

- **`auto`** — execute now, record the result;
- **`approve`** — record it as `proposed` and push it to Telegram; nothing
  happens until a human taps;
- **`deny`** — reject it on the spot;
- **anything not listed** — treated as `approve`. An unknown connector or a
  hallucinated tool is therefore queued for a person to look at, never run and
  never an error.

The decision is taken *before* the action row exists, so nothing about the
stored row can influence whether a connector is reached. Execution then takes
two claims — an in-memory one against a double tap, and a conditional `UPDATE`
in SQLite that is never released — so an action can reach a connector at most
once even across a crash.

**The control socket has no method that calls a connector.** `status`, `queue`
and `log` read local state; `pause` and `resume` set a flag; `reject` is a
status transition; `approve` and `propose` go through the executor; `chat`
starts a session whose only write tool comes back through `propose`. A method
named something like `connectors.call` was on this socket once and was removed
for exactly this reason. Do not add it back: anything that can reach the socket
could then call any tool on any connector with the gate bypassed.

The one connector call with no `actions` row behind it is the scheduler's
`watch_poll`, and it is constrained three ways: the tool name is a constant,
the policy must rate it `auto` for that connector, and nothing reachable from
the socket chooses any part of it.

## Development

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

No test in the default suite touches Telegram, Canvas, or spawns a real
`claude`. The few that would are marked `#[ignore]` and are listed in
`.superpowers/sdd/`'s task reports along with how to run them.
