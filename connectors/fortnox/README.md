# Fortnox connector

The company's accounting system — Fortnox, for **Devs Alike AB** — as a stdio
MCP server the daemon spawns. It answers *how are we doing*, *who owes us
money*, *what is due to Skatteverket and when*, and it can book a voucher.

This is the first connector in this workspace that can change something outside
the machine it runs on. A voucher posted to Fortnox is in the company's books:
the accountant reads it, Skatteverket eventually reads it, and unwinding it
takes a correcting voucher rather than a delete. Everything below is arranged
around that.

## Nothing is posted without a human tap

Restating Phase 1's gate, because this is the connector it exists for.

A model session runs as a `claude -p` subprocess with `Write`, `Edit` and `Bash`
removed and exactly one MCP tool allowed: `propose_action`. That tool does no
work — it forwards the proposal to the daemon, which puts it through
`Policy::decide` over the merged `policy.toml` of every discovered connector,
*before* the action row exists. `auto` runs now; `approve` is recorded as
`proposed` and pushed to Telegram, where nothing happens until a person taps;
`deny` is refused on the spot; **anything not listed is treated as `approve`**,
so a hallucinated tool name is queued for a human, never run.

`connectors/fortnox/policy.toml` rates all four writes — `record_voucher`,
`record_expense`, `reconcile_payment`, `attach_receipt` — as `approve`, with no
amount threshold and no exceptions. The file says why at length; the short
version is that what makes a voucher worth a glance is not the size of the
number on it, and a threshold is a thing that only ever gets tuned downward.

Two more properties hold this up, and neither should be traded away:

- **The control socket has no method that calls a connector.** The only
  connector call with no `actions` row behind it is the scheduler's
  `watch_poll`, whose tool name is a constant and which policy must rate
  `auto`.
- **There is no `confirm: true` parameter, deliberately.** The TypeScript this
  is ported from guarded every write with one: call the tool to see a preview,
  call it again with `confirm: true` to post. Keeping that here would put two
  confirmation mechanisms in series, and two means one of them is the one people
  stop reading. So the write tools take no `confirm`, post when called, and
  reaching one means the gate already approved it; the preview each write used
  to render is its own read-only tool (`preview_expense` and friends), which
  makes no HTTP call at all. `no_write_tool_takes_a_confirm_parameter` pins this
  against a future edit that reintroduces the flag.

## Setting up

### 1. Register the integration in the Fortnox Developer Portal

Open <https://developer.fortnox.se/>, sign in, and either open the existing
integration or create one.

Enable exactly these scopes:

| scope | what this connector uses it for |
|---|---|
| `bookkeeping` | `/vouchers`, `/accounts`, `/financialyears` |
| `invoice` | `/invoices` — kundfakturor, and `watch_poll`'s only live signal |
| `supplierinvoice` | `/supplierinvoices` — leverantörsfakturor |
| `customer` | `/customers`, for the names on an invoice |
| `archive` | `/inbox` and `/voucherfileconnections` — `attach_receipt` only |

If you are reusing the integration behind the old `fortnox-mcp` TypeScript
project, note that its granted set was `bookkeeping invoice supplierinvoice
customer supplier companyinformation` — so **`archive` has to be added** (it is
what lets a receipt file be attached to a voucher), and `supplier` and
`companyinformation` can go: nothing here reaches `/suppliers` or
`/companyinformation`.

Register the redirect URI as **exactly**:

```
http://localhost:8910/callback
```

**Exactly** is not a figure of speech. Fortnox compares the `redirect_uri` on
the consent request and on the code exchange against the registered string, as
strings. `http://127.0.0.1:8910/callback` is a different URI. So is the same
text with a trailing slash. So is `https`. A mismatch comes back as a flat `400`
from the token endpoint with nothing in it about URIs, which is why
`ea-fortnox-authorize` ends that particular failure with the URI it sent and the
sentence naming this as the usual cause.

That port is 8910 because that is what the old integration already has
registered — reuse it and you do not have to touch the portal entry at all. If
you change it, change `DEFAULT_REDIRECT_URI` in
`crates/ea-fortnox-mcp/src/config.rs` (or set `redirectUri` in `app.json`) to
the same string, and register the new one in the portal first.

### 2. Write `app.json`

The client id and secret live in the connector's config directory, not beside
this README:

```bash
mkdir -p ~/.config/exec-agent/fortnox
cat > ~/.config/exec-agent/fortnox/app.json <<'JSON'
{
  "clientId": "PASTE_THE_CLIENT_ID_HERE",
  "clientSecret": "PASTE_THE_CLIENT_SECRET_HERE",
  "redirectUri": "http://localhost:8910/callback"
}
JSON
chmod 600 ~/.config/exec-agent/fortnox/app.json
```

Where each value comes from:

| field | where to get it |
|---|---|
| `clientId` | The integration's **Client ID** in the Developer Portal. Public by design — it appears in the consent URL. |
| `clientSecret` | The integration's **Client Secret**, shown in the portal. Half of what a code exchange needs; treat it like a password. |
| `redirectUri` | Optional. Defaults to `http://localhost:8910/callback`; set it only if the portal entry says something else. |

**Yours already exist.** The old TypeScript project holds the same registered
integration's credentials in
`~/dev/tryffle/dev0/apps/fortnox-mcp/.env`, as `FORTNOX_CLIENT_ID` and
`FORTNOX_CLIENT_SECRET`. Those are the two values to paste above. Moving live
financial credentials between projects is your call, not a build step's, so
nothing in this repo copies them for you — and once `app.json` is in place, the
old `.env` copy is a second place a secret lives, which is worth a thought.

`app.json` must be mode `0600`; the connector refuses to load it otherwise,
naming the path and the `chmod`. A connector with no `app.json` still starts and
still completes the MCP handshake — it fails every Fortnox-touching call with a
message naming this path, and the `preview_*` tools keep working meanwhile,
because they post nothing and need no credentials. Exiting instead would reach
the daemon as "handshake failed", which tells nobody what to do.

### 3. Authorise

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build --release
./target/release/ea-fortnox-authorize
ls -l ~/.config/exec-agent/fortnox/tokens.json   # must be -rw-------
```

The command prints a consent URL, binds `localhost:8910` on both loopback
families (a listener on `::1` while the browser goes to `127.0.0.1` is a refused
connection *after* you have already consented), catches the redirect, exchanges
the code, and writes `tokens.json` at mode `0600`.

**Approve for Devs Alike AB.** Fortnox asks *which company* the integration may
act for. An account with access to more than one can approve for the wrong one,
and nothing afterwards will say so: the tokens work, the tools answer, and the
figures are somebody else's books. Check the company name on the consent screen
before you click.

If Fortnox returns an access token with no refresh token, the command writes
**nothing** and says so. An access token alone works for about an hour and
cannot be renewed; writing it would give you a connector that fails tomorrow,
inside the daemon, hours away from the command that caused it.

Tokens are re-read on every call, so authorising while the daemon is running
needs no restart.

## The refresh token rotates, and lapses after 45 days

This is the single most important operational fact about this connector, and it
has already cost this project a working integration once.

- **Every refresh returns a new refresh token and kills the one presented.** Not
  Google's model. A rotation that is not persisted is fatal silently and later:
  the access token that came back works for an hour, nothing fails today, and
  the next refresh presents a token Fortnox retired an hour ago. `TokenManager`
  therefore persists *before* it returns, and a failure to persist is a loud,
  specific error rather than a warning.
- **A refresh token unused for 45 days lapses.** Nothing in this code can
  prevent that. The daemon polls daily, which keeps the grant warm — but a
  laptop that is off, a daemon that is stopped, or a connector left unconfigured
  for six weeks all end the same way.

**This has already happened.** The old integration's tokens at
`~/.config/fortnox-mcp/tokens.json` were last written 98 days ago and its access
token expired 97 days ago; the refresh token lapsed long before anyone noticed,
because nothing was asking. They are dead and cannot be revived — only a person
at a browser can.

So: **if the assistant goes quiet about accounting — no invoice nags, no tax
deadlines, nothing when you ask how the company is doing — check this first.**

```bash
ls -l ~/.config/exec-agent/fortnox/tokens.json    # when was it last written?
ea status                                         # is the fortnox job tripped?
./target/release/ea-fortnox-authorize             # the fix, always
```

Re-running `authorize` is safe and is the only fix. Two things make the failure
visible rather than silent: `watch_poll` propagates every error instead of
returning `[]` (an empty array is indistinguishable from a quiet week, and the
breaker would never trip), and every message about a missing or refused grant
ends with the exact command above.

## Tools

Eighteen, in four groups. Every one of them is listed by name in
`policy.toml`, and two tests check that list against the registered tools in
both directions, so a tool cannot be added without a policy rule or a rule left
pointing at a tool that no longer exists.

### Read — GET only, nothing changes

| tool | policy | what it does |
|---|---|---|
| `financial_overview` | `auto` | Cash accounts (19xx), unpaid customer invoices, unpaid supplier invoices. The place to start when asked how things stand. |
| `profit_and_loss` | `auto` | Income-statement accounts (BAS classes 3–8) with balances, for the financial year covering a date. |
| `balance_sheet` | `auto` | Balance-sheet accounts (BAS classes 1–2) with balances. |
| `vat_summary` | `auto` | Output moms 261x/262x/263x and input moms 264x. 2650 (the settlement account) is excluded, being neither side. |
| `unpaid_invoices` | `auto` | Kundfakturor (`kind: "customer"`) or leverantörsfakturor (`kind: "supplier"`), every page. |
| `account_ledger` | `auto` | Vouchers in a financial year, optionally one series. Fortnox's list view omits the rows; use `query_fortnox` on a single voucher for line detail. |
| `query_fortnox` | `auto` | Escape hatch: GET any Fortnox v3 path. Read-only by construction — it issues a GET and has no way to write. |
| `vat_report` | `auto` | The VAT accounts netted into `output_vat`, `input_vat`, `net_vat_to_pay`. |
| `period_report` | `auto` | Income statement and balance sheet together — the bundle to hand an accountant. |
| `result_summary` | `auto` | Revenue, costs, financial items, the net result, Vinst or Förlust, and a per-class breakdown. |

### Preview — no HTTP call at all

These render the voucher a write *would* post, and post nothing. They are `auto`
on purpose: a preview that needed approval would just be the write with extra
steps, and the whole point is that a person can be shown what is about to be
booked before anyone decides anything. They need no credentials, so they work on
a machine that has never been authorised.

| tool | policy | previews |
|---|---|---|
| `preview_voucher` | `auto` | `record_voucher`. Unbalanced lines are an error naming the discrepancy, not a preview. |
| `preview_expense` | `auto` | `record_expense` — net to the expense account, input VAT to 2640, gross credited to the payment account. |
| `preview_reconciliation` | `auto` | `reconcile_payment`. |

The preview text is upstream's, near-verbatim, because that string is what a
person reads on their phone before approving a voucher and it has been read by a
real person approving real vouchers. Only its first line changed, and only
because the sentence it carried was an instruction to re-run with `confirm:
true`.

### Write — a human tap, every time

| tool | policy | what it posts |
|---|---|---|
| `record_voucher` | `approve` | A general manual voucher from explicit debit/credit lines. Unbalanced lines are refused before anything is sent. |
| `record_expense` | `approve` | A supplier expense from a VAT-inclusive gross amount. The rate must be 25, 12, 6 or 0. |
| `reconcile_payment` | `approve` | A bank payment against a receivable (debit 1930, credit 1510) or a payable (debit 2440, credit 1930). |
| `attach_receipt` | `approve` | Uploads a local PDF/TIF/JPG to the Fortnox inbox and connects it to an existing voucher. Needs the `archive` scope. |

### The daemon's own

| tool | policy | what it does |
|---|---|---|
| `watch_poll` | `auto` | Called on the connector's timer (daily — see `connector.toml`). Returns `[{ external_id, kind, payload }]`: unpaid customer invoices with an `overdue` flag, and upcoming Swedish tax deadlines (AGI, preliminärskatt, moms). Fails loudly; never `[]` on error. |

## The numbers are best-effort. Check them before you file.

`vat_report`, `vat_summary`, `result_summary`, `period_report`,
`profit_and_loss` and `balance_sheet` are **computed here**, by this code,
from the balances Fortnox returns for the accounts in the company's chart. They
are not Fortnox's own reports, and they are not Skatteverket's.

What that means in practice:

- The classification is the **BAS chart convention** — first digit of a
  four-digit account number is the class, 261x/262x/263x is output VAT at 25/12/6
  percent, 264x is input VAT, 2650 is the settlement account. A company that has
  customised its chart away from that convention will be summarised wrongly, and
  nothing will complain.
- An account this code cannot classify (not four ASCII digits, or class 9) is
  **silently excluded** from every class-based sum rather than failing the call.
  That matches the TypeScript it replaces, and the alternative — one odd account
  blanking the whole income statement — is worse. But it does mean a figure can
  be quietly short.
- `net_vat_to_pay` is arithmetic over account balances. It is not a
  momsdeklaration. Periodisation, reverse charge, EU trade, import VAT and
  anything your accountant does at close are not modelled here.

**Before filing anything, check these figures against the official
momsdeklaration in Fortnox or with the accountant.** Treat what this connector
says as a good enough answer to "roughly where are we", not as a filing.

## Known limitations

Recorded during this phase, all of them real and none of them fixed here.

1. **`watch_poll` polls customer invoices only.** Receivables — money owed to
   the company, which nobody else will chase. Supplier payables are *not*
   watched and will never produce a nag; they are visible any time a session
   calls `unpaid_invoices` with `kind: "supplier"`. The reason is the external
   id: `fortnox-invoice:<number>`, with nothing naming which side of the ledger
   it came from. Fortnox numbers kundfakturor and leverantörsfakturor in two
   independent sequences, so polling both would eventually have invoice 1042
   from one side collide with 1042 from the other — one event row flipping
   between two invoices' payloads on alternate polls, re-opening for triage
   every time. Widening it needs the side in the id
   (`fortnox-invoice:customer:1042`), which is a breaking change to every id
   already recorded.

2. **The `LON` keyword also matches `SALONG` and `LONDON`.** The bank-import
   coder classifies a transaction by substring-matching keywords against the
   bank's text, and `LON` (unaccented *lön*, salary → 7010) is a three-character
   substring key. `SALONG`, `LONDON`, `BALLONG` and `MELONI` all code to 7010
   with the motivering `Nyckelord "LON"`. This is inherited behaviour, kept
   deliberately so the port matches upstream's output; the one thing that *was*
   changed is the ordering, because upstream lists `LON` before its own `HALLON`
   entry, which made `HALLON` (the mobile operator, 6212) unreachable. Every
   suggestion is a proposal a person reviews, so a wrong account is caught at the
   review rather than in the books — but read the motivering, do not just accept
   the account.

3. **The bank-import writer refuses to overwrite a workbook containing sheets it
   did not write.** `calamine` parses cell values and discards the rest of the
   package; `rust_xlsxwriter` writes a new file from nothing; there is no
   read-modify-write path between them. So writing rebuilds the file from the
   three known sheets, and anything else in it would be lost silently. Rather
   than lose it, `write_workbook` errors and names the sheet. Cell comments,
   conditional formats and charts *inside* the three known sheets are still lost
   and cannot be detected — the guard covers whole sheets, which is the loss a
   person would actually notice. (Bank import is library code in `ea-fortnox`;
   it is not exposed as an MCP tool in this phase.)

## Smoke test

> **Unverified against the live Fortnox API.** Every test in these two crates
> runs against `wiremock` on loopback; no test contacts Fortnox, and none can.
> The figures below have not been compared with what Fortnox's own UI shows,
> because the only available grant lapsed 98 days ago. The first person with a
> live grant should do that comparison — if the numbers disagree, the
> reimplementation is wrong and every later figure rests on it.

With `app.json` and `tokens.json` in place, talk to the connector directly over
stdio — it is an ordinary MCP server, so two JSON-RPC lines are enough:

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build --release

printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"financial_overview","arguments":{}}}' \
  | ./target/release/ea-fortnox-mcp
```

Then `"name":"result_summary","arguments":{"date":"2026-09-24"}` and compare it
with Fortnox's own resultaträkning for the same financial year, and
`"name":"watch_poll","arguments":{}` for what the daemon will see. All three are
`GET`s: running the smoke test cannot change anything in Fortnox.

### The gate check

The one check this whole phase is for. With the daemon running and the
connector authorised, ask a chat session to book a trivial expense:

```bash
ea queue            # the action must be listed as `proposed`
                    # then check Fortnox: NOTHING may have been posted
ea reject <id>
```

The session must call `preview_expense` (which posts nothing) and then queue a
`record_expense` action. If anything reached Fortnox before your tap, the gate is
broken and nothing else in this connector matters.

### Failure reading guide

- `no Fortnox integration at …/app.json` — `app.json` is missing. The message
  carries the commands to create it.
- `… is mode 0644; it holds the Fortnox client secret` — `chmod 600` it.
- `No stored Fortnox tokens. Re-run ea-fortnox-authorize` — never authorised, or
  `tokens.json` was removed or is unparseable (a corrupt token file is treated as
  absent and logged, never quoted — it may hold a token).
- A `400` from the token endpoint at **authorize** time — almost always the
  redirect URI not matching the portal entry exactly. See step 1.
- A `400` from the token endpoint at **refresh** time — the refresh token
  lapsed or was retired. Re-run `ea-fortnox-authorize`.
- `HTTP 403` on one tool while others work — the grant is missing that tool's
  scope. Enable it on the integration and re-authorise; a grant does not gain
  scopes retroactively.

## Notes on the Fortnox API

Assumptions worth re-checking against the live account:

- OAuth is `https://apps.fortnox.se/oauth-v1/auth` and `/oauth-v1/token`; the
  code exchange and the refresh both POST a form with HTTP Basic client
  authentication. The client secret and the refresh token travel in the **body**,
  never in a URL, and the token client follows **no redirects** — `reqwest`'s
  default policy strips `Authorization` across origins but knows nothing about a
  request body.
- Token-endpoint *error* bodies are quoted, truncated; *success* bodies never
  are, because a success body is exactly the document holding both tokens. A
  success body that will not parse is reported by its length and the serde
  error's classification.
- Access tokens last an hour and are refreshed 60 seconds before their stated
  expiry, because a request begun at expiry-minus-nothing can arrive after it.
- Financial years come from `/financialyears`, and every dated report resolves
  the year covering its date rather than assuming the current one.
- `tokens.json` is written atomically: temp file in the same directory, `fsync`,
  `rename`. With a rotating refresh token, a truncated file is a dead grant.
