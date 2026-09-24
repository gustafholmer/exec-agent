# Google connector

Google Calendar and Gmail for **several** Google identities at once — typically
a `work` account and a `private` one — as a stdio MCP server the daemon spawns.
It answers two questions: *what is on my calendar, and does any of it clash*,
and *what is sitting unread in my mail*.

Everything here is read-only except one tool. `create_draft` writes a draft into
the Drafts folder; nothing in this connector sends it. That is enforced three
times over: no send tool exists, the OAuth scopes exclude `gmail.send` so Google
itself would refuse, and `policy.toml` denies `send_mail` by name so the rule is
already in force if anybody ever writes one.

## Read this before you set it up

Three things discovered while building this that will otherwise cost you real
time.

### 1. Publish the OAuth app, or re-authorise every seven days

A Google Cloud OAuth client left in **Testing** publishing status issues refresh
tokens that **expire after 7 days**. The daemon would work for a week and then
fail every poll with `invalid_grant`, and the message you get back
(`Re-authorise with: ea-google-authorize work`) would be correct and useless,
because you would be doing it again next Tuesday.

In Google Cloud Console → **APIs & Services** → **OAuth consent screen**, set
the publishing status to **In production** ("Publish app"). For an *External*
app with only your own account as a user this needs no verification review as
long as you request only the three scopes below — Google shows an "unverified
app" interstitial during consent, which you click through once. Do this
**before** the first `authorize` run, or the token you mint will be a seven-day
one.

### 2. Do not re-run `authorize` casually

Every successful `ea-google-authorize <account>` run mints a **new** refresh
token. The old one is not replaced — it keeps working — and Google caps the
number of live refresh tokens per (user, OAuth client) pair at around 100,
silently revoking the **oldest** once you pass it. Nothing warns you. Authorise
each account once, and re-run it only when a token is genuinely dead.

### 3. Only the *primary* calendar is read — subscribed calendars are invisible

`calendar.rs` reads `calendars/primary/events`. That is the one calendar owned
by the account, and nothing else:

- a calendar **shared with you** by someone else — invisible;
- a calendar you **subscribed** to by URL (an `.ics` feed) — invisible;
- a **secondary** calendar you created yourself — invisible.

This is the assumption most likely to disappoint. KTH course schedules and exam
dates are commonly consumed as a subscribed iCal feed rather than as events on
the primary calendar, and if yours are, **this connector will not see a single
one of them** — no events, no conflicts, no reminders, and no error either: the
poll succeeds and reports nothing, which looks exactly like a quiet week.

Check before you rely on it: in Google Calendar's left-hand list, anything under
*Other calendars* is not primary. If your course calendars live there, either
copy the events you care about onto the primary calendar, or treat this
connector as covering meetings and appointments only. Supporting a list of
calendar ids is a small change to `CalendarClient::list_events` and is the right
fix; it is not in this phase.

## Setting up

### Create a Desktop OAuth client

1. Google Cloud Console → create (or pick) a project.
2. **APIs & Services** → **Enabled APIs & services** → enable the **Google
   Calendar API** and the **Gmail API**.
3. **OAuth consent screen** → *External*, fill in the app name and your own
   address, and add exactly these three scopes:

   ```
   https://www.googleapis.com/auth/calendar.readonly
   https://www.googleapis.com/auth/gmail.readonly
   https://www.googleapis.com/auth/gmail.compose
   ```

   `gmail.compose` is what lets `create_draft` write a draft. It does not grant
   sending — `gmail.send` is deliberately absent, and adding it would weaken the
   guarantee in the first paragraph of this file.
4. **Publish app** (see point 1 above).
5. **Credentials** → **Create credentials** → **OAuth client ID** → **Desktop
   app**. Register `http://127.0.0.1:8471/callback` as the redirect URI — that
   is the loopback address `ea-google-authorize` listens on.
6. Copy the client id and client secret.

### Write `app.json`

The OAuth *client* is shared by every account and lives in the connector's
config directory, not beside this README:

```bash
mkdir -p ~/.config/exec-agent/google
cat > ~/.config/exec-agent/google/app.json <<'JSON'
{
  "clientId": "PASTE_THE_CLIENT_ID_HERE",
  "clientSecret": "PASTE_THE_CLIENT_SECRET_HERE",
  "redirectUri": "http://127.0.0.1:8471/callback"
}
JSON
chmod 600 ~/.config/exec-agent/google/app.json
```

It must be mode `0600`: it is half of what a code exchange needs, and a
world-readable copy is a secret you have given away. A connector with no
`app.json` still starts and still completes the MCP handshake — it fails every
call with a message naming this path. Exiting instead would reach the daemon as
"handshake failed", which tells nobody what to do.

### Authorise each account

```bash
cargo build --release
./target/release/ea-google-authorize work
./target/release/ea-google-authorize private
```

Each run opens a consent page, catches the redirect on `127.0.0.1:8471`, and
writes `~/.config/exec-agent/google/<account>.json` at mode `0600`. Sign in as
the *right* identity each time — the label is yours to choose and Google will
not stop you authorising the same mailbox twice under two names.

Accounts are re-read on every poll, so authorising a second account while the
daemon is running needs no restart.

The account label is validated against `^[A-Za-z0-9][A-Za-z0-9_-]*$` before it
is ever turned into a path: the label reaches the token store from tool
arguments a language model writes, so `../../id_rsa` is a realistic input rather
than a thought experiment.

## Tools

Every tool but `watch_poll` takes a **required `account`**. There is no default
account, and there must not be one: a tool that defaulted its account would
quietly read the wrong mailbox, and nothing downstream — triage, the
notification, you reading it — could tell.

| tool | policy | what it does |
|---|---|---|
| `list_events` | `auto` | Events on one account's primary calendar, now to `days` ahead (default 7, max 90). |
| `find_conflicts` | `auto` | Overlapping timed events. Pass `other_accounts` to scan several accounts as one calendar — that is how a work meeting clashing with a private appointment is found. All-day events excluded; back-to-back is not a clash. |
| `list_mail` | `auto` | Messages matching a Gmail query (default `is:unread`), newest first. |
| `get_mail` | `auto` | One message in full, by the id `list_mail` returned. |
| `create_draft` | `approve` | Writes a plain-text draft. **The only write in this connector.** A human approves each one: a draft is cheap, but it appears in your mailbox with your name on it. |
| `watch_poll` | `auto` | What the daemon calls every `watch_interval_secs` (2 minutes). Takes no arguments. |
| `send_mail` | `deny` | Does not exist, and is denied so it cannot quietly come into existence. |

### What `watch_poll` reports

A JSON array of `{ external_id, kind, payload }`, covering three signals across
**every** authorised account:

| kind | external id | what |
|---|---|---|
| `calendar_event` | `gcal:<account>:<id>` | Each event in the next 7 days. |
| `calendar_conflict` | `gconflict:<id>\|<id>` | Each overlapping pair, computed over the **merged** calendars of all accounts. |
| `mail` | `gmail:<account>:<id>` | Each of the 25 most recent unread messages, per account. |

The account is part of every id because the same event or message id can
legitimately exist in two accounts, and those are two rows, not one.

A conflict's id **sorts** its two event ids before joining them. A conflict is an
unordered pair, and the order it arrives in depends on which account was polled
first; an id built in arrival order would flip between polls, and each flip
would register as a brand-new event — the same clash nagging you every two
minutes forever.

Mail bodies are truncated at 2000 characters, visibly. These payloads are read
back into a tier-1 triage prompt in batches, and one newsletter with a 200 KB
body would cost more than the rest of the batch and say nothing the first two
thousand characters did not.

`watch_poll` **fails loudly**, and that matters more here than it does for a
single-account connector. If one account's token has lapsed, the whole poll
fails — it does **not** return the healthy account's rows. A half-poll that
looks complete is worse than a visible failure: the daemon would record it as a
success, the breaker would never trip, and `ea status` would stay green while
half your mail went unread. A poll with *no* authorised accounts is an error for
the same reason, not an empty list.

## Smoke test

> **Unverified against the live Google APIs.** Every test in this crate runs
> against `wiremock` on loopback; no test contacts Google. The first person with
> real credentials should run this and check the shapes.

With `app.json` and at least one authorised account in place:

```bash
cargo build --release

printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_events","arguments":{"account":"work"}}}' \
  | ./target/release/ea-google
```

Then the same with `"name":"watch_poll","arguments":{}` for what the daemon will
see. Both are reads. `create_draft` is the only call that changes anything, and
the policy makes you approve it.

Failure reading guide:

- `invalid_grant` — the refresh token is revoked, expired, or was issued to a
  different OAuth client. If this started exactly a week after you set it up,
  read point 1 at the top of this file. Otherwise re-run
  `ea-google-authorize <account>`.
- `HTTP 403` — the token is missing a scope. Check the three scopes on the
  consent screen, then re-authorise; a grant does not gain scopes retroactively.
- `no Google OAuth client at …` — `app.json` is missing; the message carries the
  path.
- A poll that succeeds and reports no calendar events at all — read point 3.

## Notes on the Google APIs

Assumptions worth re-checking against live accounts:

- Calendar reads `GET calendars/primary/events` with `singleEvents=true` (so
  recurring events arrive as individual instances), paginating on
  `nextPageToken` up to 50 hops.
- A `cancelled` event is dropped; an event with no `summary` becomes
  `(no title)` rather than being dropped.
- All-day events are parsed (a bare `date` boundary, midnight UTC) but excluded
  from conflict detection, because an all-day event overlaps everything that day
  and would drown the signal.
- Gmail reads `messages.list` then one `messages.get` per message at
  `format=full` — 1 + 25 requests per account per poll. `format=metadata` would
  be cheaper but omits the body, which is the part triage scores.
- Bodies prefer `text/plain` and fall back to stripped `text/html`. Much of what
  a bank or a university sends has no plain-text part at all.
- No redirects are followed on any request: a same-host https→http downgrade
  would carry the bearer token in clear text.
