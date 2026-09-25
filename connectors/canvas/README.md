# Canvas connector

Read-only access to Canvas LMS (KTH's instance at `https://canvas.kth.se`), as a
stdio MCP server the daemon spawns. It answers one question well: **what is due,
and when.**

Nothing in this connector can change anything in Canvas. There is no tool that
submits, comments, or enrols, and `policy.toml` denies `submit_assignment`
although no such tool exists — the policy gate matches rules by name, so the
denial is already in force if one is ever added. A session must never be able to
hand in your coursework.

## Getting a token

1. Open Canvas → **Account** → **Settings**.
2. Under *Approved Integrations*, click **+ New Access Token**.
3. Purpose: `exec-agent`. Leave the expiry blank, or set one and diarise it —
   an expired token makes the connector fail loudly (see below), not go quiet.
4. Copy the token. Canvas shows it exactly once.

This token is a bearer credential for your **whole Canvas account**: anyone
holding it can read everything you can, and act as you. Treat it like a
password.

## Configuring

Credentials live outside this directory, in the per-connector config directory:

```bash
mkdir -p ~/.config/exec-agent/canvas
cat > ~/.config/exec-agent/canvas/credentials.json <<'JSON'
{
  "baseUrl": "https://canvas.kth.se",
  "token": "PASTE_THE_TOKEN_HERE"
}
JSON
chmod 600 ~/.config/exec-agent/canvas/credentials.json
```

Checked at start-up, and refused with a message naming the path if wrong:

- the file must exist (a missing one prints exactly the commands above);
- it must be mode `0600` — a token readable by every process on the machine is
  a token you have given away;
- `baseUrl` must be `https` (loopback is exempt, for pointing the connector at
  a local fake);
- `token` must not be empty.

None of those errors, and none of the HTTP errors, ever print the token. The
`Debug` impls of `Credentials` and `CanvasClient` print `<redacted>`, and the
token travels in the `Authorization` header rather than in a URL, so it cannot
reach a log through a `reqwest` error's URL.

A connector with no credentials still starts and still completes the MCP
handshake: it fails every call with the instructions above. Exiting instead
would reach the daemon as "handshake failed", which tells nobody what to do.

## Tools

| tool | policy | what it does |
|---|---|---|
| `list_courses` | `auto` | Active enrolments, as `{ id, name, course_code }`. |
| `list_assignments` | `auto` | Every assignment in one course (`course_id`), dated or not, as `{ id, course_id, name, due_at, html_url }`. `due_at` is `null` when there is no deadline. |
| `list_upcoming` | `auto` | Every future-dated assignment across all active courses, earliest first. |
| `watch_poll` | `auto` | What the daemon calls every `watch_interval_secs` (30 minutes). A JSON array of `{ external_id, kind, payload }`, one entry per **dated** assignment. |
| `submit_assignment` | `deny` | Does not exist, and is denied so it cannot quietly come into existence. |

`watch_poll` skips assignments with no due date: an undated assignment is not a
deadline, and feeding it to triage would bury the things that are.

`watch_poll` **fails loudly**. A Canvas error becomes a tool error, never an
empty array — `[]` on failure is indistinguishable from "nothing is due", and
the daemon's circuit breaker would never trip, so an expired token would leave
the connector silently green for the rest of term.

## Smoke test

> **Verified against the live Canvas API on 2026-09-25** (`canvas.kth.se`).
> `list_courses` returned ten courses and `watch_poll` five assignment events,
> both with the shapes this crate expects. Every *test* here still runs against
> `wiremock` and none contacts Canvas, so a shape change at the vendor would
> not fail the suite — it would fail the next real poll.
>
> That run also found the only defect real traffic has exposed so far: Canvas
> answers **403** to any request without a `User-Agent`, and `reqwest` sends
> none by default. Every client in this workspace now sets one; see the failure
> guide below.

With credentials in place, talk to the connector directly over stdio — it is an
ordinary MCP server, so two JSON-RPC lines are enough:

```bash
cargo build --release          # or: cargo build -p ea-canvas

printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_courses","arguments":{}}}' \
  | ./target/release/ea-canvas
```

Expect an `initialize` result, then a `tools/call` result whose text content is
a JSON array of your courses. Then the same with `"name":"list_upcoming"` for
deadlines, and `"name":"watch_poll"` for what the daemon will see.

Every one of these is a `GET`. Running the smoke test cannot change anything in
Canvas.

Failure reading guide:

- `HTTP 401 Unauthorized` — the token is expired, deleted, or from another
  Canvas instance. Make a new one and rewrite `credentials.json`.
- `content-type "text/html" … which is not JSON` — Canvas served a login
  redirect or a maintenance page. Check `baseUrl` and the token.
- `no Canvas credentials at …` — the file is missing; the message carries the
  commands to create it.
- `HTTP 403 Forbidden` whose body is HTML saying *"You are not authorized to
  access this site because you have not provided a valid user agent"* — the
  request reached Canvas without a `User-Agent` header. `canvas.kth.se`
  rejects those outright, and the message says nothing about your token, so
  the 403 reads like a permissions problem when it is not. Every client in
  this workspace now sends `exec-agent/<version>`
  (`ea_core::http::USER_AGENT`), so this should not recur from the connector
  — but a `curl` you write by hand will hit it, and `curl -A exec-agent/0.1`
  is the fix.

## Notes on the Canvas API

Assumptions this connector makes, worth re-checking against the live instance:

- Courses come from `GET /api/v1/courses?enrollment_state=active&per_page=100`.
  Entries carrying `access_restricted_by_date: true` are stubs for enrolments
  whose dates have passed and are dropped.
- Assignments come from `GET /api/v1/courses/{id}/assignments?per_page=100`.
- `list_upcoming` is composed from those two rather than from Canvas's own
  `/users/self/upcoming_events`, which answers a different question (calendar
  events within a fixed window) in a different shape.
- Pagination follows `Link: rel="next"`, capped at 50 hops, and refuses a link
  that leaves the configured host — the header is server-controlled and the
  request carries your token.
