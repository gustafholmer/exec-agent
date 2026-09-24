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
# Sessions a day, counted from the `runs` table over *your* day (the
# `[notify] time_zone` below), so a restart does not refund what the day
# already spent. A ceiling on spend, not a target.
#
# What it does when it runs out: triage drops to tier 0 and keeps filtering
# for free, the briefings are skipped and say so, and `ea chat` answers you
# anyway with a note — refusing the owner's own typed message is the one
# failure this budget must not cause. Chat still counts against it, so a
# chatty day shuts down the background work first. `ea status` shows
# `sessions_today: 7/60`.
daily_session_budget = 60

# The model `ea chat` runs on. Set explicitly on purpose: left unset, the CLI
# inherits whatever you last picked interactively (opus-5[1m] here, ~30x tier
# 1's rate) and charges it against the budget above. Triage is separately
# pinned to claude-haiku-4-5 in code and is not affected by this.
chat_model = "claude-sonnet-4-5"

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

[retention]
events_days        = 90       # triaged events; an untriaged one is never pruned
runs_days          = 90       # finished runs; a `running` row is kept
actions_days       = 180      # terminal actions only — see below
conversations_days = 90       # never the one you are currently talking in
interval_hours     = 24
log_max_bytes      = 8388608  # rotate daemon.{out,err}.log past 8 MiB
TOML
```

### Retention

Everything else in this system is append-only, which over years of unattended
running is a leak rather than an audit trail. A `retention` job runs daily and
deletes **terminal rows only**:

| table | kept | never deleted |
|---|---|---|
| `events` | 90 days after creation, once triaged | anything still untriaged |
| `runs` | 90 days, once finished | a `running` row — the only lead on a killed session |
| `actions` | 180 days, and only `executed`/`rejected`/`expired`/`failed` | anything `proposed` or `approved`: a question you have not answered is not old data |
| `conversations`, `messages` | 90 days | the newest conversation, and any with a message inside the window |

`actions` keeps twice as long as the rest because it is the record of what this
system actually did to the world.

The same job rotates the `launchd` logs, which have no rotation of their own:
past `log_max_bytes` the file is copied to `<name>.1` and truncated **in place**
— the same inode, because `launchd` holds the descriptor and a rename would
leave the daemon writing into the archived file. One generation is kept, so the
ceiling is two files per stream.

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
| `ea status` | up? paused? how many proposals are waiting? which jobs have tripped, and why; how many events triage gave up on; `sessions_today: 7/60` and `digest_pending: 4` |
| `ea queue` | the proposals waiting for a decision |
| `ea approve <id>` | approve one and run it |
| `ea reject <id> [--reason "..."]` | reject one; nothing is called |
| `ea log [-n 20]` | recent runs: sessions and executed actions, with cost |
| `ea pause` | stop the scheduler starting new work (in-flight work finishes) |
| `ea resume` | undo `pause` |
| `ea resume <job>` | clear one job's tripped circuit breaker at once |
| `ea chat [message...]` | say something to the assistant; with no message, an interactive loop until Ctrl-D |
| `ea facts` | what the assistant has been told to remember |
| `ea forget <id>` | delete one remembered fact |

`ea queue` and `ea facts` are formatted for reading; everything else prints the
daemon's JSON.

That is the whole command surface. There is no `ea restart`, no `ea reauth` and
no `ea breaker`; where one of those would be the obvious thing to reach for,
the section below says what to do instead.

## Operations

This is the part to read at 08:00 when something is wrong.

### Reading `ea status`

`ea status` prints the daemon's JSON, unformatted opinion-free. Every field,
and what a bad value looks like:

| field | what it means |
|---|---|
| `status` | always `"ok"` when you get an answer at all. The real health check is whether the command answers: a refused connection to `~/.local/state/exec-agent/daemon.sock` means the daemon is not running. |
| `paused` | `true` after `ea pause` — the scheduler is starting nothing new. Nothing else in the system sets this, so if it is `true` and you did not do it, somebody did. |
| `pending_actions` | proposals sitting in the queue. A number that only grows means you are not answering Telegram; `ea queue` shows them. |
| `unscorable_events` | events triage gave up on after repeated failures to score them. Nonzero is the daemon admitting it decided not to look at something. |
| `digest_pending` | scores held back by the threshold, quiet hours or the rate limit, waiting for the morning briefing. Normally the hours since the last briefing. **Climbing past a day means the briefing has stopped running** — check `schedules` below. |
| `digest_dropped` | how many held lines the 200-line digest cap has destroyed. Nonzero means the briefing stopped long enough to lose things. |
| `jobs[]` | one entry per scheduled job, with `name`, `tripped`, `last_error` and `retry_in_secs`. `last_error` is present whether or not the breaker has tripped, so a connector that is failing but not yet tripped is visible here too. |
| `schedules[]` | the cron entries — `name`, `cron`, `time_zone`, `enabled`, `last_run_at`, `next_run_at`. A `next_run_at` of `null` means the expression no longer parses, which is the difference between "not due until tomorrow" and "silently dead". |
| `connectors` | the connectors discovered at startup. A connector you installed but do not see here was not discovered — see *Adding a connector*. |
| `sessions_available` | whether a `claude` binary was found at startup. `false` is a degraded daemon: it still polls and still gates actions, it just does no thinking. |
| `notifier_configured` | whether Telegram is wired up. `false` means nothing can reach your phone. |
| `sessions_today` | `"7/60"` — model sessions started today against the ceiling, counted over *your* day in `[notify] time_zone`, from the `runs` table rather than an in-memory counter, so a restart does not refund what the day already spent. |
| `sessions_spent_today` | the same count, as a number. |
| `daily_session_budget` | the ceiling `sessions_today` is measured against, i.e. `daily_session_budget` from `config.toml`. |
| `chat_model` | which model `ea chat` runs on. |
| `facts` | how many things the assistant has been told to remember. `ea facts` lists them. |
| `started_at` | when this daemon process came up. A recent timestamp you did not cause means it crashed and `launchd` restarted it — look in `daemon.err.log`. |

The two numbers that say at a glance whether the thing is working rather than
quietly stuck are `sessions_today` and `digest_pending`. A budget at its ceiling
means triage has stopped scoring and the briefings have stopped writing; a
digest climbing past a day means the briefing that drains it is not running.

### When a job's breaker trips

A job that fails `breaker_threshold` times in a row (five by default) stops
running, and `ea status` shows it as `tripped` with the reason in `last_error`.
Any failure counts: a connector that is down, a lapsed token, a reply that is
not the documented shape, or a panic. A poll that returns an empty array is a
success, which is why every connector here is written to fail loudly rather
than return `[]`.

You are told, once: the transition into tripped is pushed to Telegram. That
push ignores quiet hours on purpose — the message is "I have stopped watching",
which is the one whose value decays fastest while it waits — and is repeated at
most once per job per six hours while it stays tripped. A half-open retry that
fails again does not re-push, so *silence after the first message is not
recovery*. `ea status` is the thing that knows.

A tripped breaker does **not** stop forever: after a cooldown of five minutes —
doubling on each failed retry, up to an hour — the breaker goes half-open and
allows one attempt. A success closes it and the job goes back on its own
schedule; a failure re-opens it and waits longer. `retry_in_secs` in
`ea status` says how long that is. So a Canvas outage, an expired token that
gets refreshed, or a night without network heals with nobody watching.

To reset one by hand: **`ea resume <job>`**, naming the job exactly as
`ea status`'s `jobs[].name` does. It clears the failure count, the stored
error, the backed-off cooldown and the schedule the job was waiting on, and
re-arms the Telegram push, so the job runs on its next tick. It is for when you
have just fixed the cause yourself and do not want to wait out an hour of
cooldown; you never *have* to use it. Note that `ea resume` with no argument is
a different command — it undoes `ea pause` and touches no breaker.

Restarting the daemon also clears every breaker, because the state is in
memory. That is a side effect, not the procedure: use `ea resume <job>`.

The bounds are `breaker_threshold`, `breaker_cooldown_secs` (default 300) and
`breaker_max_cooldown_secs` (default 3600) in `config.toml`. The cooldown is a
floor on the retry rate, not a new schedule: a connector that polls every 30
minutes is never polled more often while broken than while healthy.

### The logs

Two files, written by `launchd` and rotated by the daemon's own daily retention
job (one generation each, past `retention.log_max_bytes`):

```bash
tail -f ~/.local/state/exec-agent/daemon.err.log   # where the errors are
tail -f ~/.local/state/exec-agent/daemon.out.log
```

`daemon.err.log` is the one to read first: `RUST_LOG=info` is set in the plist,
and `tracing` writes to stderr. A daemon that will not start at all writes its
reason there and then `launchd` restarts it every 120 seconds — so a
`ThrottleInterval`-paced repetition of the same error in that file is a fatal
startup problem (a `policy.toml` naming a foreign connector, a corrupt
database, a state directory that cannot be locked), not a transient one.

For what the daemon *did* rather than what it logged, use `ea log`:

```bash
ea log -n 30
```

It prints the most recent `runs` rows as JSON, newest first: `kind`, `prompt`,
`outcome`, `detail`, `action_ids`, `cost_usd`, `duration_ms`, `started_at`,
`finished_at`. One row per model session and one per executed action. This is
where the money is visible — sum `cost_usd` — and where a session that timed
out or returned nonsense still appears, because the row is opened before the
child is spawned and closed on every path out.

### Re-authorising a connector

No connector renews itself past the limits below; every one of these ends with
a person at a browser.

| connector | how | daemon restart? |
|---|---|---|
| **google** | `./target/release/ea-google-authorize <account>`, once per identity (`work`, `private`). | No — accounts are re-read on every poll. |
| **kth** | `./target/release/ea-kth-authorize <account>`. | No — same. |
| **fortnox** | `./target/release/ea-fortnox-authorize`. See below. | No — tokens are re-read on every call. |
| **notion** | Rewrite `~/.config/exec-agent/notion/<workspace>.json` with a fresh Internal Integration Secret from <https://www.notion.so/my-integrations>, mode `0600`. | No — files are read fresh on every call. |
| **canvas** | Mint a new token in Canvas → Account → Settings → *+ New Access Token*, write it to `~/.config/exec-agent/canvas/credentials.json`, mode `0600`. | **Yes.** `ea-canvas` loads its credentials once at process start, so the connector child has to be respawned. |

Two of these have a failure mode worth knowing before it happens.

**Google's seven-day token.** A Cloud OAuth client left in *Testing* publishing
status issues refresh tokens that expire seven days after consent, so the
connector works for a week and then fails every poll with `invalid_grant`.
There is no fix in this repo — it is a property of the Cloud project — and the
options (stay in Testing and re-authorise weekly; make the app Internal if the
account is on a Workspace domain; publish External unverified; submit for
verification and CASA) are laid out with their real costs in
`connectors/google/README.md`. Decide *before* the first authorise run: the
publishing status at consent time is what decides which kind of token you mint.

**Fortnox's stored refresh token is dead, today.** Every Fortnox refresh
returns a new refresh token and kills the one presented, and a refresh token
unused for 45 days lapses. The old integration's tokens at
`~/.config/fortnox-mcp/tokens.json` were last written **98 days ago** against
that 45-day limit; they lapsed long before anyone noticed, because nothing was
asking. They cannot be revived. Only re-consent through the Fortnox Developer
Portal can fix this, and it has to be you at the browser.

The credentials for that are also not lying around: the old `dev0` project's
`.env` (`~/dev/tryffle/dev0/apps/fortnox-mcp/.env`) declares
`FORTNOX_CLIENT_ID` and `FORTNOX_CLIENT_SECRET` and leaves **both of them
empty** — the lines are the key, an `=`, and nothing. So you supply your own:
open <https://developer.fortnox.se/>, open the existing integration (do not
register a second one), copy its Client ID and Client Secret into
`~/.config/exec-agent/fortnox/app.json` at mode `0600`, then run
`ea-fortnox-authorize` and **check the company name on the consent screen** —
an account with access to more than one company can approve for the wrong one,
and nothing afterwards will say so. The full procedure, including the scopes to
enable and why the redirect URI must be exactly
`http://localhost:8910/callback`, is in `connectors/fortnox/README.md`.

So: if the assistant goes quiet about accounting — no invoice nags, no tax
deadlines, nothing when you ask how the company is doing — that is the first
thing to check.

### Going on holiday

Three mechanisms, and they stop different things. Pick by what you want to be
true while you are away.

**`daily_session_budget = 0` in `config.toml`, then restart.** A ceiling of
zero is spent before anything runs; this is the documented way to turn every
unattended session off without uninstalling the daemon. What stops: tier-1
triage scoring, and with it every salience score and therefore every
notification triage would have produced; and all three briefings, which are
skipped and say so. What does **not** stop: connectors keep polling on their
own intervals, events keep being recorded, tier-0 filtering keeps running for
free, breaker health pushes still reach Telegram if a connector breaks, and
`ea chat` still answers you — refusing the owner's own typed message is the one
failure this budget must not cause, and a chat session still counts, which is
what makes a chatty day shut down the background work first.

The cost of this one: untriaged events accumulate for the whole holiday, and
retention never prunes an untriaged event. Triage works through them at one
batch per pass once the budget comes back.

**`ea pause`.** Stops the scheduler starting anything new — no polls, no
triage passes, no cron briefings. Work already in flight finishes. Nothing
reaches a connector, so no breaker can trip and nothing accumulates. But
`paused` is an in-memory flag: **a daemon restart clears it**, and the plist has
`KeepAlive`, so a crash or a reboot silently un-pauses you. Good for an
afternoon; not something to trust for two weeks.

**Unloading the launchd job** (`./scripts/install-launchd.sh --uninstall`, or
`launchctl unload ~/Library/LaunchAgents/dev.gustaf.exec-agent.plist`) stops
the daemon entirely and survives a reboot. It also stops the daily retention
job, and — the thing that bites — it stops the Fortnox poll that keeps that
45-day refresh token warm. Two weeks is fine; six is a dead grant and a trip
back to the Developer Portal.

The recommendation, for an actual holiday: set `daily_session_budget = 0` and
leave it running. The connectors stay warm, the grants stay alive, nothing
interrupts you, and coming back is one edit and a restart.

### Adding a connector

Three things, and then a restart.

1. **Write a crate that is a stdio MCP server.** It must expose a tool named
   exactly `watch_poll` that takes no arguments and returns a JSON array of
   `{ external_id, kind, payload }` — one entry per thing that changed — plus
   whatever other tools the connector offers. `watch_poll` must **fail loudly**:
   propagate the error rather than returning `[]`, because an empty array is
   indistinguishable from a quiet week and the breaker would never trip. It
   must not set `source`; the daemon attributes every event to the connector's
   own name, so a connector cannot write events as another one.
2. **Add a directory under `connectors/`** holding a `connector.toml` and a
   `policy.toml`. A directory with a manifest but no policy is **not** a
   connector and is skipped with a warning — an unpoliced connector would be a
   hole in the gate.
3. **Restart the daemon.** Discovery runs once, at startup:
   `./scripts/install-launchd.sh` rebuilds, rewrites the plist and reloads.

The contract that will actually catch you out, all of it enforced at startup:

- **The directory basename must equal the `name` in `connector.toml`.** A
  mismatch is a hard startup error, not a warning. This is what stops a
  directory called `scratch` from shipping `name = "fortnox"` and a
  `[fortnox]` policy section and widening the real Fortnox connector's gate.
  Two connectors declaring the same name is likewise a hard error.
- **`policy.toml` may only declare a section for its own connector.** A file in
  `connectors/foo/` containing a `[bar]` section fails startup naming both. A
  file with *no* `[foo]` section loads but warns, and every tool then defaults
  to `approve`.
- **`watch_poll` must be rated `auto` in that section or the connector is never
  polled.** The scheduler refuses the poll and says so, which trips the breaker
  — fail-closed, and visible. Anything not listed in the policy is treated as
  `approve`, so a tool you forget about is queued for you, never run.
- **`command` is resolved** as: an absolute path; else next to the running
  `ea-daemon` binary; else one level up from there (which is how `cargo test`
  finds it); else `PATH`. A release build puts the connector binary in
  `target/release` beside `ea-daemon`, so there is no install step. An
  unresolvable command is an error at spawn time, not a silent skip.
- **`watch_interval_secs`** defaults to 300 if the manifest omits it.
- **Credentials go in `~/.config/exec-agent/<connector>/`**, never in the
  connector directory and never in the repository.
- A connector with no credentials should still complete the MCP handshake and
  fail each *call* with instructions. Exiting at startup reaches the daemon as
  "handshake failed", which tells nobody what to do.

The connectors root is `$EA_CONNECTORS_DIR` if set, else `./connectors` if it
exists — which is why the plist's `WorkingDirectory` is the repository — else
`~/.config/exec-agent`.

### Tuning the numbers

**The shipped notification and budget values are defaults, not tuned values.**
They were set at implementation time from argument, not from a week of this
machine's own traffic, and nobody has run the system long enough to know
whether they are right for you. Tuning them is your job, and it needs a fortnight
of ordinary use rather than an afternoon.

Count two things, daily, and change nothing while you count:

1. **How many notifications arrived.** Telegram messages that interrupted you,
   plus what the morning briefing carried.
2. **How many you actually acted on.** Opened the thing, answered the mail,
   tapped Approve — not "read and dismissed".

Then:

| what you observed | the constant | direction |
|---|---|---|
| Most notifications arrive and get ignored | `[notify] threshold` (default 60) | **Raise it.** Fewer things clear the bar; the rest still reach you in the morning digest rather than being lost. |
| Genuinely urgent things are showing up in the morning digest instead of interrupting you | `[notify] threshold` | **Lower it.** This is the failure that costs you something; the other one only costs attention. |
| More than a handful of interruptions in any one hour | `[notify] max_per_hour` (default 3) | **Lower it.** The overflow is held for the digest, not dropped. |
| `sessions_today` hits its ceiling before evening | `daily_session_budget` (default 60) | Raise it if the spend is worth it — check `ea log`'s `cost_usd` first — or leave it and accept degraded triage after the ceiling. |

Change one at a time and give it a week. The number that matters for
`max_per_hour` is the one you stop resenting, and there is no way to find it
except by living with a few.

## Where things live

| what | where |
|---|---|
| configuration and credentials | `~/.config/exec-agent/` |
| database (`events`, `actions`, `runs`, …) | `~/.local/state/exec-agent/state.db` |
| control socket (mode `0600`) | `~/.local/state/exec-agent/daemon.sock` |
| daemon logs | `~/.local/state/exec-agent/daemon.{out,err}.log` (plus one rotated `.1` each) |
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

**The control socket has no method that calls a connector.** Thirteen methods:
`status`, `queue` and `log` read local state; `pause`, `resume` and
`resume_job` set flags in the scheduler; `reject` is a status transition;
`approve` and `propose` go through the executor; `remember`, `facts` and
`forget` read and write the local `facts` table and touch nothing else; `chat`
starts a session whose only write tools are `propose_action`, which comes back
through `propose` and the gate, and `remember`. A method
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
