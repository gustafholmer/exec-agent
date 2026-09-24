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

### 1. The seven-day token, and why "just publish the app" is not free advice

A Google Cloud OAuth client left in **Testing** publishing status issues refresh
tokens to its test users that **expire seven days after consent**. The daemon
would work for a week and then fail every poll with `invalid_grant`, and the
message you get back (`Re-authorise with: ea-google-authorize work`) would be
correct and useless, because you would be doing it again next Tuesday.

The obvious fix — flip the app to **In production** — is not obviously
available here, because of *which* scopes this connector asks for:

| scope | Google's sensitivity tier |
|---|---|
| `calendar.readonly` | sensitive |
| `gmail.readonly` | **restricted** |
| `gmail.compose` | **restricted** |

Restricted is Google's strictest tier. Google's own documentation says an app
requesting restricted scopes must complete **app verification**, and that an app
which "has the ability to access data from or through a third-party server" must
additionally pass an independent **CASA security assessment** by a
Google-empanelled assessor, repeated **annually**. That is a paid, recurring
process aimed at products with users, not at a daemon on one laptop — and note
that this daemon *does* forward mail bodies to a model provider for triage, so
"it is only local, the assessment cannot apply to me" is not a conclusion to
reach casually.

So there is a real trade-off, and you have to pick:

**A. Stay in Testing and re-authorise weekly.** Certain, free, and annoying: one
`ea-google-authorize <account>` run per account per week, forever. If you only
want the calendar side, dropping the two Gmail scopes leaves you with one
*sensitive* scope and no restricted one, which takes the security assessment
out of the picture — verification for production still applies, but that is a
review, not an annual paid audit.

**B. Internal app — the clean way out, if you have a Workspace domain.** If the
account belongs to a Google Workspace or Cloud Identity organisation, and the
Cloud project lives in that organisation, set the app's audience to **Internal**.
An Internal app serves only users in the organisation and is not subject to the
External verification flow, and the seven-day test-user expiry does not apply
because an Internal app has no test users. Caveats: a consumer `@gmail.com`
account cannot do this; your Workspace admin can still block the app or the
scopes for the domain; and it covers only accounts in that domain, so a `work`
account on the domain and a `private` consumer account are two different
situations under one connector.

**C. Publish External without verification.** Google documents that an app *can*
be published unverified — while calling it strongly discouraged — with the app
name and logo hidden, an unverified-app warning at consent, and a hard cap of
100 users. What is documented, but which **I have not been able to confirm
against a live project**, is whether that is actually reachable for an app
requesting restricted scopes, and whether it removes the seven-day expiry:
Google ties that expiry to the *Testing* status, which implies publishing ends
it, and there are credible reports that the console refuses to publish an app
requesting restricted Gmail scopes until verification is submitted. Google can
also come back and require verification later. Treat this as "try it and see
what the console says", not as a promise.

**D. Submit for verification and CASA.** Correct if this ever becomes something
other people use. Disproportionate for a single-user tool: it is an annual,
paid assessment.

What to actually do: open **Google Auth Platform** → **Audience** in the Cloud
Console and read what your own project says — it states your audience, your
publishing status, and what changing it would require. Decide **before** the
first `authorize` run: the publishing status at consent time is what decides
whether the token you mint is a seven-day one.

References: [OAuth app state
overview](https://developers.google.com/identity/protocols/oauth2/production-readiness/overview),
[Restricted scope
verification](https://developers.google.com/identity/protocols/oauth2/production-readiness/restricted-scope-verification),
[Manage app audience](https://support.google.com/cloud/answer/15549945).

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
3. **OAuth consent screen** → choose the audience point 1 pointed you at
   (*Internal* if you have a Workspace domain, otherwise *External*), fill in
   the app name and your own address, and add exactly these three scopes:

   ```
   https://www.googleapis.com/auth/calendar.readonly
   https://www.googleapis.com/auth/gmail.readonly
   https://www.googleapis.com/auth/gmail.compose
   ```

   `gmail.compose` is what lets `create_draft` write a draft. It does not grant
   sending — `gmail.send` is deliberately absent, and adding it would weaken the
   guarantee in the first paragraph of this file.
4. Set the publishing status you settled on in point 1 above — and know which
   one it is before you authorise, not after.
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

The account label is validated against `^[a-z0-9][a-z0-9_-]*$` before it
is ever turned into a path: the label reaches the token store from tool
arguments a language model writes, so `../../id_rsa` is a realistic input rather
than a thought experiment.

Lower-case only. The label becomes `<label>.json`, and macOS' APFS is
case-insensitive by default, so `work` and `Work` would be two accounts sharing
one token file: authorising the second would overwrite the first's grant.
`ea-google-authorize Work` is refused, and the error tells you to type `work`.

## Tools

Every tool but `watch_poll` takes a **required `account`**. There is no default
account, and there must not be one: a tool that defaulted its account would
quietly read the wrong mailbox, and nothing downstream — triage, the
notification, you reading it — could tell.

| tool | policy | what it does |
|---|---|---|
| `list_events` | `auto` | Events on one account's primary calendar, now to `days` ahead (default 7, max 90). |
| `find_conflicts` | `auto` | Overlapping timed events. Pass `other_accounts` to scan several accounts as one calendar — that is how a work meeting clashing with a private appointment is found. All-day events excluded; back-to-back is not a clash. |
| `list_mail` | `auto` | Messages matching a Gmail query, newest first. Defaults to the same query `watch_poll` runs: `is:unread -category:promotions -category:social newer_than:7d`. |
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
| `calendar_conflict` | `gconflict:<id>\|<id>` | Each overlapping pair, computed over the **merged** calendars of all accounts — except a meeting you accepted in two of them, which is one meeting, not a clash. |
| `mail` | `gmail:<account>:<id>` | Each of the 25 most recent messages matching `is:unread -category:promotions -category:social newer_than:7d`, per account. |

The account is part of every id because the same event or message id can
legitimately exist in two accounts, and those are two rows, not one.

A conflict's id **sorts** its two event ids before joining them. A conflict is an
unordered pair, and the order it arrives in depends on which account was polled
first; an id built in arrival order would flip between polls, and each flip
would register as a brand-new event — the same clash nagging you every two
minutes forever.

The unread query is not plain `is:unread`. That includes the Promotions and
Social tabs, where most mailboxes keep most of their unread messages; with a cap
of 25 per poll, a week of newsletters would fill every poll and starve out the
mail you actually needed to see. `newer_than:7d` bounds it the way the 7-day
lookahead bounds the calendar: mail older than that which is still unread is not
news. Change it in `watch::UNREAD_QUERY`, or per call with `list_mail`'s `query`.

One invitation accepted in **both** accounts is two calendar entries that
overlap perfectly, and reporting it as a cross-account clash would be a
permanent false alarm with a stable id — news the daemon can never retire, every
two minutes, until you stop reading conflicts at all. Both copies are still
reported as their own `calendar_event` rows; they are just not reported as
clashing, because Google's `iCalUID` (stable across calendars, unlike the event
id) says they are one meeting. Two events with **no** `iCalUID` are never
treated as the same meeting: a missing conflict is worse than a spurious one.

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
- Gmail's category operators (`-category:promotions`, `-category:social`) are
  inbox-tab filters; an account with tabs disabled simply matches nothing extra,
  which is the harmless direction.
- `iCalUID` is read off `events.list` and used only to suppress the
  same-meeting-twice conflict. Google documents it as stable across calendaring
  systems; with `singleEvents=true` the instances of a recurring series can share
  one, which only matters if two instances of one series overlap — and calling
  that "not a conflict" is the right answer anyway.
- Gmail reads `messages.list` then one `messages.get` per message at
  `format=full` — 1 + 25 requests per account per poll. `format=metadata` would
  be cheaper but omits the body, which is the part triage scores. The `get`s
  run eight at a time, so those 25 requests are four round trips rather than
  25; the daemon allows one poll 90 seconds against a 120-second interval, and
  the sequential version could not reliably fit.
- Bodies prefer `text/plain` and fall back to stripped `text/html`. Much of what
  a bank or a university sends has no plain-text part at all.
- No redirects are followed on any request: a same-host https→http downgrade
  would carry the bearer token in clear text.
