# Notion connector

Notion for the owner's several workspaces, as a stdio MCP server the daemon
spawns. It reads what has been shared with it, files new notes without asking,
and tells the daemon about anything with a date on it.

Nothing in this connector can remove anything from Notion. There is no tool
that deletes, archives, or edits existing content, and `policy.toml` denies
`delete_page` and `archive_page` although no such tools exist — the policy gate
matches rules by name, so the denial is already in force if one is ever added.
That asymmetry is not decoration: it is what makes `create_page` safe to run
unattended (see *[Why `create_page` is `auto`](#why-create_page-is-auto)*).

## One integration per workspace

A Notion integration is created **inside** a workspace and can never see
another one. So this connector holds one token per workspace, each under its
own label, and **every tool takes a required `workspace` argument**. There is
no default. A write that guessed would file the owner's note in the wrong
workspace and report success.

Labels must match `^[a-z0-9][a-z0-9_-]*$` — they become filenames, on a
case-insensitive disk, so `work` and `Work` would be one file holding two
workspaces' tokens. `personal` and `work` are the expected ones; any label that
passes the rule works.

## Getting a token

For **each** workspace you want the connector to see:

1. Go to <https://www.notion.so/my-integrations> and click **New
   integration**.
2. Pick the workspace from the dropdown — this is the choice that cannot be
   changed later, and it is why there is one token per workspace.
3. Type **Internal**. Name it something you will recognise in a page's
   Connections menu, e.g. `exec-agent`.
4. Capabilities: grant *Read content* and *Insert content*. **Leave *Update
   content* off** and see whether anything breaks — nothing in this connector
   edits existing content, so it should not need it, and a capability you never
   grant is one fewer thing to have to trust the policy gate about. (Which
   Notion capability covers appending blocks to an existing page is not
   something this repo has verified against a live workspace; if
   `append_to_page` starts failing with a permissions error, that is the
   setting to turn on.) Leave *Read user information* off.
5. Copy the **Internal Integration Secret** (`ntn_…`; tokens minted before
   Notion's 2024 rename begin `secret_…`, and both work).

This secret is a bearer credential for everything the integration has been
shared with. Treat it like a password.

## The step everybody misses: share the pages

**A Notion token on its own sees nothing.** Not your pages, not your databases,
not a single row. An integration starts with access to exactly zero content,
and the API answers `404` — the same status as "no such page" — for anything
that has not been shared with it. So a correct token, correctly installed,
produces an empty search result and a connector that looks broken.

For each page or database the connector should see:

> open it in Notion → the **⋯** menu at the top right → **Connections** →
> *Add connections* → pick your integration.

Sharing a page shares everything nested under it, so sharing one "Work" parent
page is usually enough. For `watch_poll` to report anything, the **databases**
with dates in them must be among what you shared: a poll discovers its tables
by asking Notion what the integration can see, precisely so that this list
cannot drift from the sharing you actually did.

## Configuring

Credentials live outside this directory, in the per-connector config directory —
one file per workspace, named for its label:

```bash
mkdir -p ~/.config/exec-agent/notion

printf '{"token":"ntn_PASTE_THE_WORK_SECRET"}'     > ~/.config/exec-agent/notion/work.json
printf '{"token":"ntn_PASTE_THE_PERSONAL_SECRET"}' > ~/.config/exec-agent/notion/personal.json

chmod 600 ~/.config/exec-agent/notion/*.json
```

`workspaceName` is an optional second field, purely for your own benefit when
reading an error message:

```json
{ "token": "ntn_...", "workspaceName": "Gustaf's workspace" }
```

Checked when a workspace is used, and refused with a message naming the path
and these steps if wrong:

- the file must exist;
- it must be mode `0600` — a token readable by every process on the machine is
  a token you have given away;
- it must parse, and `token` must not be empty.

None of those errors, and none of the HTTP errors, ever print the token. The
`Debug` impls of `Credentials` and `NotionClient` print `<redacted>`, the token
travels in the `Authorization` header rather than in a URL, and the base URL is
rejected outright if it carries userinfo — so a token cannot reach a log line
through a `reqwest` error's URL.

Files are read **fresh on every call**. Adding `work.json` while the daemon is
running needs no restart. A connector with no credentials at all still starts
and still completes the MCP handshake; it fails every call with the
instructions above. Exiting instead would reach the daemon as "handshake
failed", which tells nobody what to do.

## Tools

| tool | policy | what it does |
|---|---|---|
| `search` | `auto` | Everything in one workspace whose title matches `query` — omit `query` to list everything shared with the integration. Returns `{ pages, data_sources, unknown }`, **separated**: see below. |
| `get_page` | `auto` | One page, as `{ id, title, url, page }`, `page` being Notion's raw object. |
| `query_database` | `auto` | Every row of one database, as `{ id, title, url, properties }`. |
| `create_page` | `auto` | Creates a new page under a parent page or as a row in a database. The one unattended write; see below. |
| `append_to_page` | `approve` | Adds plain-text paragraphs to the end of an existing page. |
| `watch_poll` | `auto` | What the daemon calls every `watch_interval_secs` (an hour). See below. |
| `delete_page` | `deny` | Does not exist, and is denied so it cannot quietly come into existence. |
| `archive_page` | `deny` | Likewise. |

### `search` returns two different kinds of thing

Notion's search endpoint does not separate pages from databases: one list comes
back with both in it, told apart only by each result's `"object"` field. They
are not interchangeable — a data source is a table you pass to
`query_database`, a page is a document you pass to `get_page`, and calling
either tool with the other's id fails.

So this connector never hands that mixed list on. `search` sorts it into
`pages`, `data_sources`, and `unknown` — the third bucket holding any object
type this connector was not written against, so a future API version's new type
lands somewhere visible rather than being mistaken for a page.

### Why `create_page` is `auto`

This is the first write anywhere in this project that runs **without a human
tap**, and it is a deliberate line rather than an oversight.

Creating a page is additive. It changes nothing that already exists, it lands
in the owner's sidebar where they will see it, and undoing it is one click.
Against that, an assistant that must ask permission before filing a note is an
assistant that never files notes — and filing notes is the reason Notion is in
front of it at all. The tap would be paid dozens of times over to prevent a
harm whose worst case is "there is a page I did not ask for".

`append_to_page` is `approve` for the mirror reason. Same API, similar blast
radius, different owner: it edits a page the owner already maintains — their
meeting notes, their project doc, the thing they will read next week and trust.
What the gate is protecting is not the number of bytes written, it is whose
work is at stake.

Both arguments depend on the `deny` rules above. `create_page` is only safe
unattended because nothing in this connector can delete or overwrite. If a
future tool changes that, this rule has to be revisited, and
`creating_a_page_is_auto_and_appending_to_one_is_not` in `crates/ea-notion` is
the test that will make somebody look.

### `watch_poll`

One entry per database row whose date property falls within two weeks either
side of now, across every configured workspace, as
`{ external_id, kind, payload }` with `external_id` of
`notion:<workspace>:<page_id>`.

- **The databases polled are whatever has been shared with the integration.**
  A poll asks Notion what it can see rather than reading a list from a config
  file, because a config file would drift from the sharing the moment you added
  a database.
- **A row's deadline is found by property *type*, not name.** There is no
  `due_at` field in Notion; the column is called `Due`, `Deadline`,
  `Förfaller`, or `📅`, depending on who built the table. Any property of type
  `date` is a candidate; a name containing `due`/`deadline`/`förfall`/
  `slutdatum`/`inlämning` wins a tie, and otherwise the earliest date does.
  `Created time` and `Last edited time` are different property types and are
  never mistaken for deadlines.
- **Rows with no date property are skipped.** An undated row is not a deadline,
  and feeding it to triage buries the rows that are.
- **The window looks backwards too.** Two weeks of grace, so a deadline missed
  over a long weekend still surfaces — and so a workspace that has been in use
  for years does not dump every 2024 row into triage on the connector's first
  poll.
- **Moving a deadline re-opens the item.** The due date is in the payload and
  nothing time-varying is, so an unchanged row polled twice produces byte-identical
  output (no re-notification), while a moved date changes the payload and puts
  the item back in front of triage.
- **It fails loudly.** An error in *any* workspace fails the whole poll. `[]` on
  failure is indistinguishable from "nothing is due", the circuit breaker would
  never trip, and a connector whose token was revoked in October would show
  green until somebody noticed the silence.

### Why an append is never split across two requests

`append_to_page` refuses text that renders as more than 100 paragraph blocks,
which is exactly Notion's own per-request limit. The underlying client *can*
chunk a longer append, but chunking is not atomic: a failure on the second
request leaves the first already on the page, and retrying the call duplicates
it. Capping at one request's worth means every append this connector performs
either happened or did not, and the refusal comes before anything is written.

## Smoke test

> **Unverified against the live Notion API.** Every test in this crate runs
> against `wiremock`; no test contacts Notion. The first person with a real
> token should run the following and check the shapes — in particular that
> `create_page` into a database works, since it writes the title under the
> property id `"title"` rather than under your title column's name.

```bash
cargo build --release

# One workspace configured as above.
./target/release/ea-notion <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"workspace":"work"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"watch_poll","arguments":{}}}
JSON
```

If `search` returns empty lists, the integration has not been shared with
anything — go back to *[The step everybody
misses](#the-step-everybody-misses-share-the-pages)*. That is the expected
first-run experience, and it is not a bug in the connector.
