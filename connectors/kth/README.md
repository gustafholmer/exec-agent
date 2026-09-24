# KTH connector

The owner's KTH mailbox, read-only, as a stdio MCP server the daemon spawns. It
answers one question: *what is sitting unread in my KTH inbox.*

Nothing here writes. There is no draft tool, no send tool, no "mark as read" —
this connector reads unread mail and leaves it unread. That is enforced three
times over: no write tool is registered, the OAuth grant holds `offline_access`
and a read-only `Mail.Read` and nothing else, and `policy.toml` denies
`create_draft` and `send_mail` by name so the rules are already in force if
anybody ever writes one.

---

## Read this first: what is unverified

**Nothing in this connector has ever spoken to Microsoft.** Every test in
`crates/ea-kth` runs against a `wiremock` server on loopback. What the tests
show is that this code handles the shapes Microsoft's documentation describes;
they cannot show that those are the shapes Microsoft sends, and they cannot
show that KTH will let you connect at all.

Specifically unverified, in rough order of how likely each is to bite:

| claim | status |
|---|---|
| KTH's tenant permits a **user** to consent to a third-party app asking for delegated `Mail.Read` | **unknown — only you can find out; see below** |
| The Graph response shapes (`value[]`, `body.contentType`, `from.emailAddress`, `receivedDateTime`) | unverified, from documentation |
| `$filter=isRead eq false` on `me/mailFolders/inbox/messages` is accepted | unverified |
| Message ids are base64url text (`A–Z a–z 0–9 - _ =`) | unverified; a wider id is **refused with a clear error**, not sent |
| Entra rotates the refresh token on every refresh | unverified here; the code handles both rotation and its absence |
| The tenant id `3db27ecc-1791-4dda-9b51-798adfa4a3ca` is KTH's | **measured** — `login.microsoftonline.com/kth.se/.well-known/openid-configuration` resolves to it |

What *was* measured, before any of this was written:

- `dig MX kth.se` → `mailfilter-ng-{1,2,3,4}.sunet.se`. SUNET's filter is the
  inbound edge, not Microsoft.
- KTH's SPF record is
  `v=spf1 include:_spf.kth.se include:spf.protection.outlook.com ~all` — KTH
  *sends* through Exchange Online.
- There is **no DNS at all** for `imap.kth.se`, `mail.kth.se`, or
  `autodiscover.kth.se`. Only `webmail.kth.se` (130.237.28.91).

So the mailboxes are in Exchange Online, there is no IMAP host to connect to,
and Microsoft Graph is the only path. That part is settled.

---

## The one thing you have to try

Only you can find out whether KTH permits this, because it requires signing in
as you. It takes about ten minutes, most of it in a browser.

### 1. Register an application you own

You cannot register an application inside KTH's tenant — you are a student
there, not an administrator. You register it in a tenant of **your own** and
mark it multitenant, which is the ordinary shape for a desktop app that signs
users in from other organisations.

1. <https://entra.microsoft.com> → **App registrations** → **New
   registration**. (Sign in with a personal Microsoft account if you have no
   other tenant; Entra will create a directory for it.)
2. Name: anything — `exec-agent` will do.
3. Supported account types: **Accounts in any organizational directory
   (Any Microsoft Entra ID tenant — Multitenant)**. This is the setting that
   lets a KTH account sign in to an app registered elsewhere.
4. Redirect URI: platform **Public client/native (mobile & desktop)**, value
   `http://127.0.0.1:8473/callback`.
5. **API permissions** → **Add a permission** → **Microsoft Graph** →
   **Delegated permissions** → tick `Mail.Read` and `offline_access`. Nothing
   else. Do **not** add `Mail.ReadWrite` or `Mail.Send`.
6. **Authentication** → confirm "Allow public client flows" is on. Do **not**
   create a client secret; this connector has no use for one and refuses an
   `app.json` that contains one.
7. Copy the **Application (client) ID**.

### 2. Write `app.json`

```bash
mkdir -p ~/.config/exec-agent/kth
cat > ~/.config/exec-agent/kth/app.json <<'JSON'
{
  "clientId": "PASTE_THE_APPLICATION_CLIENT_ID_HERE"
}
JSON
chmod 600 ~/.config/exec-agent/kth/app.json
```

The tenant defaults to KTH's (`3db27ecc-1791-4dda-9b51-798adfa4a3ca`) and the
redirect URI to `http://127.0.0.1:8473/callback`; both can be overridden with
`"tenant"` and `"redirectUri"` keys, and neither normally needs to be.

The tenant is pinned deliberately. The obvious alternative, `/common`, would
let any Microsoft account — a personal outlook.com address, a different
university — complete the consent and be written to disk under the label `kth`,
and nothing downstream could tell. Pinned, that is a login error instead of a
silently wrong mailbox.

Nothing in this file is a credential (an application id is a public identifier
by design), so unlike the Google connector's `app.json` its mode is not
checked. `chmod 600` above is habit, not a requirement.

### 3. Attempt the consent — this is the experiment

```bash
cargo build --release
./target/release/ea-kth-authorize kth
```

It prints a consent URL, waits on `127.0.0.1:8473`, and writes
`~/.config/exec-agent/kth/kth.json` at mode `0600` if it succeeds. Open the
URL, **sign in with your KTH account** (not a personal one), and read what the
page says.

**Three outcomes.**

- **A consent page listing "Read your mail" and "Maintain access to data you
  have given it access to", and an Accept button.** It works. Approve it; the
  command writes the token file and prints the granted scopes.
- **"Need admin approval" / `AADSTS65001`.** KTH's tenant requires an
  administrator to consent on behalf of users. No change to this code can work
  around that. Read the next section.
- **`AADSTS50011`** (redirect URI mismatch) or **`AADSTS700016`** (application
  not found in the directory). These are the registration's fault, not KTH's,
  and they are fixable: check step 1's redirect URI is exactly
  `http://127.0.0.1:8473/callback` under the *public client* platform, and that
  the account types are multitenant.

Whatever happens, nothing secret is printed: not the authorization code, not
the PKCE verifier, not the tokens.

---

## If KTH refuses consent

If the consent page demands administrator approval, you have three options, and
the third is the one to take.

**A. Ask KTH IT to grant admin consent for the application.** Legitimate, and
occasionally granted for a read-only scope on one's own mailbox. It requires
giving them the application id and explaining what it does. Expect it to take
days, and expect "no" to be a common answer for a personally-registered app.

**B. Build an IMAP transport instead.** There is nothing to build it against.
The spike found no DNS for `imap.kth.se`, `mail.kth.se` or `autodiscover.kth.se`
— there is no host. If KTH documents an IMAP endpoint somewhere this spike did
not look, the code is ready for it: `mail::MailTransport` is the only place that
knows how mail is fetched, and a second implementation of that trait is the
whole change. But do not start there on a hunch.

**C. Forward KTH mail to the Gmail account that is already connected.** This is
the right answer, it takes two minutes, and it needs no code at all.

> KTH's webmail is Outlook on the web. Open <https://webmail.kth.se>, go to
> **Settings → Mail → Forwarding**, enable forwarding to the Gmail address the
> `google` connector already reads, and tick "keep a copy of forwarded
> messages" so your KTH mailbox stays intact. In Gmail, optionally add a filter
> on `to:<your>@kth.se` so the forwarded mail is labelled and easy to read back.

That gets KTH mail into triage today, through a connector that is already
working, with no application registration and no consent. Its costs, stated
plainly so the choice is an informed one: the mail arrives tagged as coming
from your KTH address rather than *in* a KTH mailbox; the `from` on a forwarded
message is preserved by Outlook forwarding but the envelope changes, so a Gmail
filter on `from:` may behave differently than you expect; and KTH IT could
disable automatic external forwarding at any time (some universities do,
specifically to stop mail leaving the institution). Check that forwarding is
actually enabled a week later.

If you take option C, this connector simply sits unconfigured. The daemon still
starts it, every call fails with a message naming `app.json`, and the
scheduler's breaker trips for it — which is visible in `ea status` and costs
nothing else. Deleting `connectors/kth/` removes it entirely.

---

## Tools

Every tool but `watch_poll` takes a **required `account`**. There is no default
account, and there must not be one: in practice you will authorise one mailbox
and always pass `kth`, which is exactly the situation in which a default looks
harmless and becomes wrong the day a second one exists.

| tool | policy | what it does |
|---|---|---|
| `list_mail` | `auto` | Unread messages in one mailbox's inbox, newest first (default 25, max 100). |
| `get_mail` | `auto` | One message in full, by the id `list_mail` returned. |
| `watch_poll` | `auto` | What the daemon calls every `watch_interval_secs` (5 minutes). Takes no arguments. |
| `create_draft` | `deny` | Does not exist. Would require `Mail.ReadWrite`, which is not requested. |
| `send_mail` | `deny` | Does not exist, and never will. |

### What `watch_poll` reports

A JSON array of `{ external_id, kind, payload }`, covering every authorised
account, plus one row per account that could not be read:

| kind | external id | what |
|---|---|---|
| `mail` | `kthmail:<account>:<id>` | Each of the 25 most recent unread messages in the inbox. |
| `connector_error` | `ktherr:<account>` | One row for an account this poll could not read. Names the account, the error, what is invisible while it lasts, and what to do. |

Both ids are stable, so an account that stays dead is one event in your log, not
one every five minutes. If **every** authorised account fails, the poll returns
an error instead — that is a connector that cannot do its job, and the
scheduler's breaker should see it. A poll with *no* authorised accounts is an
error for the same reason, never an empty list: silence must not look like
success.

Only the **inbox** is polled. `me/messages` would span Junk and Deleted Items,
and unread junk is exactly what a digest must not be filled with. Mail bodies
are truncated at 2000 characters, visibly: these payloads are read back into a
triage prompt in batches, and one newsletter with a 200 KB body would cost more
than the rest of the batch and say nothing the first two thousand characters
did not.

---

## Notes on the Graph API

Assumptions worth re-checking against a live mailbox, all of them unverified:

- One request per account per poll. Graph's `$select` returns the message
  **body** in the list response, so unlike Gmail there is no list-then-get
  fan-out — no concurrency to tune and no request budget to bound.
- **Bodies prefer `text`, fall back to stripped `html`, and fall back again to
  Graph's `bodyPreview`.** This is the single most important behaviour in the
  connector. Most university mail has no plain-text alternative at all, and a
  client that read only the plain-text case would hand triage an empty body —
  which is scored as noise and thrown away, so the mail that mattered most
  would be exactly the mail that vanished. That failure was found once already
  in this project, in the Gmail client.
- `<style>` and `<script>` contents are dropped along with their tags. Exchange
  generates mail with stylesheets running to hundreds of rules, and with a
  2000-character body cap those would push the actual message out of the
  prompt.
- Numeric HTML entities are decoded (`&#246;` → `ö`). Swedish mail is full of
  them.
- No `$orderby`. Graph documents restrictions on combining `$filter` and
  `$orderby` on message collections, and tripping one returns
  `InefficientFilter` — a poll that would fail permanently for a reason nobody
  could guess. Messages are sorted newest-first in-process instead.
- No pagination. `$top` bounds the result and `@odata.nextLink` is ignored: a
  mailbox with more than 25 unread messages has a problem this connector cannot
  solve.
- No redirects are followed on any request: a same-host https→http downgrade
  would carry the bearer token in clear text.
- A 429 is reported, not retried. The next scheduled poll is the retry;
  hammering a throttled endpoint lengthens the penalty.

## The transport seam

`mail::MailTransport` is the only place in the crate that knows how mail is
fetched. `watch.rs` and `tools.rs` hold an `Arc<dyn MailTransport>` and never
mention Microsoft, HTTP, or OAuth. If the consent question above comes back
"no" and some other transport turns out to be available, what has to be
replaced is `auth.rs` (an OAuth token store becomes whatever credential that
transport needs) and `graph.rs` (one implementation becomes another). What
survives untouched: the `Mail` model, the body extraction and its stripper,
`watch.rs`, `tools.rs`, `connector.toml`, `policy.toml`, and every test of any
of them.

`watch::tests::swapping_the_transport_needs_no_change_above_the_seam` drives the
whole poll through a transport that has never heard of Microsoft, which is the
compiling version of that claim.

## Account labels

The label becomes `~/.config/exec-agent/kth/<label>.json` and is validated
against `^[a-z0-9][a-z0-9_-]*$` before it is ever turned into a path — labels
reach the token store from tool arguments a language model writes, so
`../../id_rsa` is a realistic input rather than a thought experiment.

Lower-case only: macOS' APFS is case-insensitive by default, so `kth` and `KTH`
would be two accounts sharing one token file, and authorising the second would
overwrite the first's grant. `ea-kth-authorize KTH` is refused, and the error
tells you to type `kth`.

Accounts are re-read on every poll, so authorising one while the daemon is
running needs no restart.

## Smoke test

> **Unverified against live Microsoft Graph.** No test in this crate contacts
> Microsoft. The first person with a working grant should run this and check
> the shapes.

With `app.json` and an authorised account in place:

```bash
cargo build --release

printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_mail","arguments":{"account":"kth"}}}' \
  | ./target/release/ea-kth
```

Then the same with `"name":"watch_poll","arguments":{}` for what the daemon will
see. Both are reads; there is nothing here that is not.

Failure reading guide:

- `invalid_grant` — the refresh token is revoked or expired, the password
  changed, or a Conditional Access policy now refuses the app. Re-run
  `ea-kth-authorize kth`.
- `HTTP 403` — either the grant lacks `Mail.Read` (a grant does not gain scopes
  retroactively; re-authorise), or KTH has withdrawn permission for the
  application. Re-read "If KTH refuses consent".
- `HTTP 429` — Graph is throttling. Nothing is retried inside a poll; the next
  one is the retry.
- `no KTH app registration at …` — `app.json` is missing; the message carries
  the path.
- `answered … which is not JSON` — Microsoft returned a sign-in or consent page
  where JSON was expected. The token is not being accepted.
