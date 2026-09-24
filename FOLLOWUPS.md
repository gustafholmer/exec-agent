# Known gaps

Things the final review of Phase 4 found and we deliberately did not fix, with
enough detail to act on later. None is a safety hole; the policy gate holds.

## 1. `ea pause` does not survive a restart — FIXED

`Daemon::pause`/`resume` now record the owner's intent in `kv` under
`scheduler.paused`, and `main` applies it before `scheduler.start()`. The
persistence deliberately does *not* live in `Scheduler::pause`: `main` calls
that primitive as the first step of the shutdown drain, so persisting there
would record every clean shutdown as a pause and, under the plist's
`KeepAlive`, bring the daemon back paused and never running again. The
regression test is `daemon::tests::a_clean_shutdown_does_not_persist_a_pause`.

The trade-off this entry used to name — a reboot no longer rescues a
forgotten pause — was settled by visibility, not by an expiry: nothing
un-pauses the daemon on its own. `ea status` reports `paused_since` and
`paused_for_days`, and after 30 days adds a `pause_warning` naming the Fortnox
refresh token's 45-day lapse — the one connector a long pause permanently
breaks.

## 2. `notion.create_page` is graded `auto`

`connectors/notion/policy.toml:47`. It is the one write that executes with no
human tap, and briefing sessions render untrusted Notion content while holding
`propose_action`. The content is now fenced and labelled as data, which is the
mitigation; the residual is that a Notion collaborator with workspace access
could try to steer a page into existence. Blast radius is one reversible page,
and `create_page` demands an explicit parent.

Regrade it to `approve` if you ever share those workspaces more widely. The
policy file's own rationale reasons about model *error*, not deliberate
steering — worth rewriting whichever way you decide.

## 3. `remember` has no second deterministic gate

`propose_action` is gated twice: the CLI allowlist, then `Policy::decide`.
`remember` has only the allowlist (`daemon.rs:552`, `session.rs:325`). Every
session shares one `ea-propose` server on one socket with no per-session
argument, so the allowlist is the only control. No defect today — triage
sessions get no `--allowedTools` at all and so cannot call it. The fix, if the
scoping ever loosens, is a scope flag at `session.rs:325`.

## 4. Telegram delivery is at-least-once with no idempotency key

A reboot between sending a reply and committing the update offset re-answers
the same message — and re-charges a session for it. The existing comment
justifies at-least-once for callbacks only; it does not cover replies.

## 5. `facts::matching` has no `LIMIT`

A large fact table would splice an unbounded block into a chat system prompt.
Bounded in practice by how many facts one person writes.

## 6. `OVERDUE_GRACE` is 365 days

Deliberate: Notion exposes no completion signal, so a shorter grace would
permanently drop genuinely overdue tasks. The cost is that a large first-poll
backlog is retained for a year and retention never prunes it.

## 7. A Canvas token change needs a daemon restart

`ea-canvas` calls `Credentials::load()` once before serving, unlike the other
four connectors, which re-read. Rare operation; one `launchctl kickstart` works
around it.

## 8. A long chat reply can truncate the over-budget note off the end

`notify/telegram.rs:908` truncates outgoing text at `MAX_MESSAGE_BYTES` (3800).
The over-budget note is appended *after* the reply, so a reply close to that
size would push the note off the end — the one case where the note matters
most is also the one where it can vanish.

Narrow: it needs a near-4 KB reply and an over-budget day at the same time.
Left alone at merge because the fix changes truncation semantics on the path
every notification shares, and that deserves its own review. The right shape
is to reserve the note's length before truncating the reply, not to truncate
the concatenation.
