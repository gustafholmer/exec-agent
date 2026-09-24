# exec-agent

A personal executive assistant that runs as a background daemon on macOS. It
watches five services — mail, two calendars, coursework, Notion, and the
company's books — decides what is worth your attention, interrupts you on
Telegram when something is, and asks before it changes anything.

## The one idea

**A language model may only ever _propose_. Deterministic Rust disposes.**

A model session runs as a `claude -p` subprocess with the write tools stripped
out and, at most, two MCP tools allowed. The only one that can reach the outside
world is `propose_action`, and it does no work: it hands the proposal to the
daemon, which puts it through `ea_core::policy::Policy::decide` over the merged
`policy.toml` of every discovered connector. `auto` runs now. `approve` becomes
a row in the queue and a pair of buttons on your phone, and nothing happens
until you tap one. `deny` is refused on the spot.

The part that makes this safe rather than merely tidy: **anything the policy
does not name resolves to `approve`, never `auto`.** An unknown connector, a
tool the model invented, a typo — all of them queue for a human. So the worst a
confused session can do is ask you a question. See
[How the gate works](#how-the-gate-works) before you change anything.

## What it costs

**A Claude subscription, not metered tokens.** Every piece of thinking is a
`claude -p` child process using the CLI's own subscription login — which is why
the daemon never passes `--bare`, a flag that would take auth strictly from
`ANTHROPIC_API_KEY` and put a 24/7 loop on a metered key. That constraint shaped
everything downstream:

- Tier-1 triage batches up to forty events into **one** session, because the
  dominant cost is the ~16k-token cached prompt prefix, paid once per session
  rather than once per event.
- Every session names its model explicitly — Haiku for triage, Sonnet for the
  briefings and for `ea chat`. Left unset, the CLI inherits whatever you last
  picked interactively, which on this machine is `opus[1m]`, and `--setting-sources ''`
  does not clear it.
- `daily_session_budget` (60) is a hard ceiling on sessions per day, counted
  from the `runs` table over your own day, so a restart does not refund what the
  day already spent.

If you want the number anyway, `ea log` prints `cost_usd` per run.

## What ships

Five connectors, each a stdio MCP server the daemon spawns as a child process
and talks to over stdin/stdout. None of them is compiled into the daemon.

| connector | watches | can write | poll |
|---|---|---|---|
| [google](connectors/google/README.md) | Gmail and Google Calendar, several identities at once (`work`, `private`) | `create_draft` — approval | 2 min |
| [kth](connectors/kth/README.md) | the KTH mailbox, over Microsoft Graph; reads unread mail and leaves it unread | nothing | 5 min |
| [canvas](connectors/canvas/README.md) | coursework deadlines in Canvas LMS | nothing | 30 min |
| [notion](connectors/notion/README.md) | shared Notion databases, anything with a date on it | `create_page` — **auto**; `append_to_page` — approval | 1 h |
| [fortnox](connectors/fortnox/README.md) | the company's books: unpaid invoices, VAT, Skatteverket deadlines | four writes, all approval, no amount threshold | 24 h |

`notion.create_page` is the single write in the whole system that runs without a
human tap. `connectors/notion/policy.toml` argues the case at length; the short
version is reversal cost — a stray page is one click to trash, a stray paragraph
appended to a document you trust may never be found.

Around the connectors:

- **Triage**, every five minutes. Tier 0 is pure Rust — a mute list plus a
  keyword rescue — costs nothing, and must not become clever. Tier 1 is one
  `claude-haiku-4-5` session that scores a batch of what survived, 0–100 for
  salience, and calls no connector at all.
- **Notifications** on Telegram when a score clears the threshold, falls outside
  quiet hours, and fits the hourly rate limit. Everything else is held for the
  digest.
- **Three cron briefings**, all on `claude-sonnet-4-5`, all in your own time
  zone: `morning_briefing` daily at 07:00, `bookkeeping_pass` Mondays at 09:00,
  `vat_prep` at 09:00 on the first of the month.
- **An approval queue** — Telegram buttons, or `ea queue` / `ea approve` /
  `ea reject`.
- **`ea chat`**, one conversation shared by the terminal and the phone. A
  question asked on Telegram is answerable in the terminal ten seconds later,
  mid-sentence. A chat session gets no connectors: its two tools are
  `propose_action` and `remember`.
- **Daily retention**, which prunes terminal rows and rotates the logs.

## How it fits together

Two paths, and they meet only in the database.

**Something happens in the world → your phone.**

```
connector.watch_poll        every N seconds, the scheduler calls exactly this
       │                    one tool, by name, on each connector
       ▼
  events table              { source, external_id, kind, payload }; the daemon
       │                    stamps `source` itself, so a connector cannot write
       │                    events as another one
       ▼
  tier 0 (Rust)             mutes and keyword rescues. Free. Drops the rest.
       │
       ▼
  tier 1 (Haiku)            one session, up to 40 events, salience 0-100
       │
       ▼
  notification policy       threshold → quiet hours → rate limit, in that order
       │                    (each one can only hold a message back)
       ├──── clears all three ────▶  Telegram, now
       └──── held ────────────────▶  digest, drained by the morning briefing
```

**A model wants to change something → the world.**

```
  session (claude -p)       Write/Edit/Bash/NotebookEdit disallowed;
       │                    --strict-mcp-config; a dedicated empty cwd
       │  propose_action
       ▼
  ea-propose (MCP)          forwards over the control socket; does no work
       │
       ▼
  Policy::decide            merged policy.toml of every discovered connector
       │
       ├─ auto ───────────▶  execute now, record the result
       ├─ approve ────────▶  `proposed` row + Telegram buttons; waits for a tap
       ├─ deny ───────────▶  refused
       └─ not listed ─────▶  treated as approve
```

The decision is taken *before* the action row exists, so nothing about the
stored row can influence whether a connector is reached.

The two paths never touch: triage reads events and writes scores, and cannot
call a connector; the executor calls connectors and never reads events. The
briefings are the one place both meet, and they do it through the same
constraints — see [How the gate works](#how-the-gate-works).

## Before you can start it: two connectors are blocked

Read this before `install-launchd.sh`. Check `ls ~/.config/exec-agent/` first:
if it is empty — as it was when this was written — the daemon will come up,
discover five connectors, and fail every poll until all five breakers trip.
Credentials are per connector and none of them is in this repository.

Three of them you can fix yourself in a browser: **canvas** (mint a token),
**notion** (one Internal Integration Secret per workspace), **google**
(`ea-google-authorize <account>`, but read that connector's README about the
seven-day token *before* the first consent — the publishing status at consent
time is what decides which kind of refresh token you mint, and it cannot be
changed afterwards).

**Two need somebody other than you, and no change in this repository can work
around either.**

- **Fortnox: the stored grant is dead and needs re-consent at the Developer
  Portal.** The old integration's tokens at `~/.config/fortnox-mcp/tokens.json`
  were last written on 2026-06-18, against a refresh token that lapses after 45
  days unused. They cannot be revived. Worse, the client credentials are not
  lying around either: the old `dev0` project's `.env` declares
  `FORTNOX_CLIENT_ID` and `FORTNOX_CLIENT_SECRET` and leaves **both empty**. So
  you supply them yourself from <https://developer.fortnox.se/>. Full procedure
  under [Re-authorising a connector](#re-authorising-a-connector).
- **KTH: no app registration exists, and the consent may need a KTH
  administrator.** You register the application in a tenant of your own
  (multitenant, public client, redirect
  `http://127.0.0.1:8473/callback`), write `~/.config/exec-agent/kth/app.json`,
  and then run `ea-kth-authorize kth` to find out what KTH's tenant does. If it
  answers `AADSTS65001` / "Need admin approval", user consent is disabled and
  only KTH IT can unblock it. `connectors/kth/README.md` lists the options and
  their real costs.

Neither blocks the daemon: an unconfigured connector completes the MCP handshake
and fails each *call* with instructions, so its breaker trips, `ea status` says
why, and the other four keep working.

One more thing to know before the first run: **no connector here has ever
spoken to its real service.** Every test in the workspace runs against
`wiremock` on loopback, so what is proven is that this code handles the shapes
each API's *documentation* describes. Each connector's README closes with the
specific assumptions to re-check against a live account. See
[Development](#development).

## Prerequisites

- **Rust**, stable. This workspace needs 1.88 or newer.
  (If `rustc` on your `PATH` is broken, use rustup's:
  `PATH="$HOME/.cargo/bin:$PATH" cargo …`.)
- **A logged-in `claude` CLI.** The daemon shells out to `claude -p` for
  tier-1 triage, the three briefings and `ea chat`. Run `claude` once
  interactively and sign in — on a subscription, not an API key; see
  [What it costs](#what-it-costs). Without it the daemon still starts, still
  polls, and still gates actions: triage drops to tier 0, the briefings report
  that they could not run, `ea chat` cannot reply, and `ea status` shows
  `sessions_available: false`.
- **macOS**, for the `launchd` service. Nothing else is macOS-specific.

## Build

```bash
cargo build --release
```

That produces, in `target/release/`:

- `ea-daemon` and `ea` — the daemon and its CLI;
- one MCP server per connector: `ea-canvas`, `ea-google`, `ea-kth`,
  `ea-notion`, `ea-fortnox-mcp`;
- three one-shot OAuth helpers: `ea-google-authorize`, `ea-kth-authorize`,
  `ea-fortnox-authorize`;
- `ea-propose`, the MCP server that carries a session's one proposal back to
  the daemon.

The daemon resolves connector binaries relative to itself, so a release build
needs no install step.

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

- [canvas](connectors/canvas/README.md) — one Canvas access token.
- [google](connectors/google/README.md) — a Cloud OAuth client in `app.json`,
  then `ea-google-authorize <account>` per identity. **Read it before the first
  consent**; the publishing status you consent under decides whether the refresh
  token lives seven days or indefinitely.
- [kth](connectors/kth/README.md) — a Microsoft Entra app registration in
  `app.json`, then `ea-kth-authorize kth`. May be refused by KTH's tenant.
- [notion](connectors/notion/README.md) — one Internal Integration Secret per
  workspace, each under its own label. Every tool takes a required `workspace`
  argument; there is no default.
- [fortnox](connectors/fortnox/README.md) — Client ID and Secret in `app.json`
  from the Developer Portal, then `ea-fortnox-authorize`.

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
| `ea pause` | stop the scheduler starting new work (in-flight work finishes). Durable: a crash or a reboot comes back paused |
| `ea resume` | undo `pause`, including the persisted part of it |
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
| `paused` | `true` after `ea pause` — the scheduler is starting nothing new. Nothing else in the system sets this, and nothing un-pauses it on its own, so if it is `true` and you did not do it, somebody did. |
| `paused_since` | when the pause was recorded, from the durable row rather than the in-memory flag. A `paused: true` with a **null** `paused_since` is a pause that did not reach disk and will not survive the next restart. |
| `paused_for_days` | the same, as a number of days. |
| `pause_warning` | present only after 30 days paused, and only when a Fortnox connector is discovered: the Fortnox refresh token lapses after 45 days unused, and re-authorising is a manual browser round trip. It warns; it never un-pauses you. |
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
| **canvas** | Mint a new token in Canvas → Account → Settings → *+ New Access Token*, write it to `~/.config/exec-agent/canvas/credentials.json`, mode `0600`. | No — `CanvasServer` builds its client from whatever the file holds at the time of each call. The very next request carries the new token. |

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
`~/.config/fortnox-mcp/tokens.json` were last written on **2026-06-18** against
that 45-day limit; they lapsed at the beginning of August and nobody noticed,
because nothing was asking. They cannot be revived. Only re-consent through the Fortnox Developer
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
reaches a connector, so no breaker can trip and nothing accumulates.

The pause is **durable**: it is written to `kv` under `scheduler.paused` and
re-applied before the scheduler starts, so a crash or a reboot comes back
paused. That is deliberate — the plist has `KeepAlive`, and a pause the owner
thinks will outlive a reboot and does not is a worse failure than a forgotten
one. Nothing expires it. What stops a forgotten pause from being permanent is
visibility instead: `ea status` reports `paused_since` and `paused_for_days`,
and past 30 days adds a `pause_warning` naming the Fortnox refresh token's
45-day lapse — the one connector a long pause permanently breaks.

The persistence lives in the IPC handler, not in `Scheduler::pause`, because
`main` calls that primitive as the first step of its shutdown drain: persisting
there would record every clean shutdown as a pause and, under `KeepAlive`, bring
the daemon back paused and never running again. The regression test is
`daemon::tests::a_clean_shutdown_does_not_persist_a_pause`.

**Unloading the launchd job** (`./scripts/install-launchd.sh --uninstall`, or
`launchctl unload ~/Library/LaunchAgents/dev.gustaf.exec-agent.plist`) stops
the daemon entirely and survives a reboot. It also stops the daily retention
job, and — the thing that bites — it stops the Fortnox poll that keeps that
45-day refresh token warm. Two weeks is fine; six is a dead grant and a trip
back to the Developer Portal.

The recommendation, for an actual holiday: set `daily_session_budget = 0` and
leave it running. The connectors stay warm, the grants stay alive, nothing
interrupts you, and coming back is one edit and a restart. `ea pause` is the
better answer only if you want the polling to stop too — and then `ea resume` is
the thing you must remember, because nothing will do it for you.

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

A model session runs as a `claude -p` subprocess with `Write`, `Edit`, `Bash`
and `NotebookEdit` disallowed on the command line, `--strict-mcp-config` so the
CLI cannot add any of your own servers, and `--setting-sources ''` from a
dedicated empty working directory so no hooks, plugins, CLAUDE.md or auto-memory
are discovered. `--allowedTools` is a closed list chosen by the session's kind,
not shared between kinds:

| session | allowed tools | connectors it can see |
|---|---|---|
| tier-1 triage | none at all | none |
| the three briefings | `propose_action` | none |
| `ea chat` | `propose_action`, `remember` | none |

`remember` writes one row to the local `facts` table and reaches nothing, and
chat is the only kind given it: the prompt of a chat session is the owner
talking, which is the only provenance that makes a fact safe to splice back into
a later system prompt.

So the only tool in the system that can reach the outside world is
`propose_action`, served by the `ea-propose` binary — and it does no work. It
forwards the proposal to the daemon over the control socket, and the daemon puts
it through `Policy::decide`, which reads the merged `policy.toml` of every
discovered connector:

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

Two kinds of connector call have no `actions` row behind them, and both are
constrained the same three ways — the tool name comes from a constant in the
daemon's own source, the merged policy must already rate that tool `auto` for
that connector, and nothing reachable from the socket chooses any part of it:

1. **The scheduler's `watch_poll`**, once per connector per interval.
2. **The reads a briefing gathers** before its session starts —
   `unpaid_invoices`, `vat_summary`, `account_ledger` on `fortnox`. The daemon
   makes these itself, never the model; a connector whose `unpaid_invoices` were
   graded `approve` would simply not be read, and the refusal says so.

The material a briefing gathers then travels into the prompt as text, fenced and
labelled as data. A briefing session's tool scope is `propose_action` only —
deliberately **not** `remember`, because the material is text other people wrote
and a durable fact written from it would be spliced back into a later chat
prompt — and it is handed an empty connector list, so there is not even a server
for a tool call to land on. A briefing that wants to change something must
propose it like anything else.

## Development

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

No test in the default suite touches Telegram, Canvas, Google, Microsoft
Graph, Notion or Fortnox, and none spawns a real `claude`. Every connector's
tests run against a `wiremock` server on loopback. Exactly one test is
`#[ignore]`d — `session.rs`'s, which spawns the real CLI and spends
subscription usage; run it by name when you want to check the CLI's output
shape has not moved under you.

Be clear about what that buys and what it does not. **No connector in this
repository has ever spoken to its real service.** The tests show this code
handles the shapes each API's documentation describes; they cannot show those
are the shapes the API sends. Every connector's README ends with a section
saying so and listing the specific assumptions worth re-checking the first time
you point it at a live account — `connectors/kth/README.md` is the longest, and
its first unverified claim is whether KTH will let you connect at all.

So the first real run of any of these is an experiment, not a deployment. It
will fail loudly if it fails: a connector that gets an unexpected shape
propagates the error, the breaker trips after five of them, and `ea status`
carries the reason in `last_error`.
