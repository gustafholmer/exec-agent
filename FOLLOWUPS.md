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
`remember` has only the allowlist (`daemon.rs:676`, `session.rs:325`). Every
session shares one `ea-propose` server on one socket with no per-session
argument, so the allowlist is the only control. No defect today — triage
sessions get no `--allowedTools` at all and so cannot call it. The fix, if the
scoping ever loosens, is a scope flag at `session.rs:325`.

## 4. Telegram delivery is at-least-once with no idempotency key — FIXED

`notify/updates.rs` now keeps a second durable mark beside the offset:
`HandledUpdates`, a high-water `update_id` under `telegram.handled_through`,
written *before* the handler runs. A redelivery below the mark is skipped,
counted in `PollSummary::duplicates` and logged. The offset is still written
after handling, so a batch interrupted halfway through is still redelivered —
the claim is what stops the redelivery being acted on a second time.

The trade-off, chosen deliberately: a crash *during* handling now costs the
outcome rather than producing a duplicate. A message loses its answer and the
owner re-sends; a tap loses its press and the owner taps again, with the
buttons still on the phone and the action still `proposed`. Both are a re-do
the owner can see and repeat, and a crash after the `proposed -> approved`
transition was never recoverable by replay anyway. A duplicate chat turn is a
`claude` session charged against the day's budget with nothing said about it.
So the claim is applied to every update kind rather than to messages alone:
one mechanism, one key, no branch on kind to get wrong.

The regression test for the *ordering* is
`updates::tests::a_crash_inside_the_handler_leaves_the_update_already_claimed`:
the handler panics mid-session and the poll dies with it, and the claim has to
already be in `kv` when the restarted loop is handed the same update back.
Moving `self.handled.mark(id)?` below the `dispatch` call fails that test and
only that test. Its neighbours,
`a_redelivered_message_is_answered_once_across_a_restart` and
`a_handled_update_leaves_a_high_water_mark`, cover the redelivery path and the
high-water semantics but both read `kv` only after a poll that ran to
completion, so neither can see which side of the handler the mark was written
on.

## 5. `facts::matching` has no `LIMIT` — FIXED

Capped at `MAX_MATCHING_FACTS` (20), which is far above what the topic-word
selectivity produces in practice and bounds the pathological case.

Which twenty was the load-bearing half. `all()` is oldest-first, and a cap on
that order would keep whatever the owner said first and drop every later
correction — exactly the rows that exist because the earlier ones were wrong.
So the cap is applied to a new recency order,
`COALESCE(updated_at, created_at) DESC, id DESC`: newest first, with the id
breaking a same-instant tie so the cut is the same set from run to run, and a
correction moving its fact back to the front because `remember` writes
`updated_at`.

The regression tests are
`facts::tests::matching_is_bounded_and_keeps_the_most_recently_touched_facts`
and `facts::tests::a_corrected_fact_is_kept_over_newer_but_untouched_ones`;
both fail against an unbounded `matching` and against a cap on the
oldest-first order.

## 6. `OVERDUE_GRACE` is 365 days

Deliberate: Notion exposes no completion signal, so a shorter grace would
permanently drop genuinely overdue tasks. The cost is that a large first-poll
backlog is retained for a year and retention never prunes it.

## 7. A Canvas token change needs a daemon restart — FIXED

`CanvasServer::new` now takes the config directory and builds its client from
whatever `credentials.json` holds at the time of each call, the way
`ea-notion` and `ea-kth` read their token stores. `main` latches nothing. The
cost is one file read and a `reqwest::Client` per tool call, against four
tools that each make a network round trip anyway.

`Credentials::load()` went with the latch it existed for: `load_from(dir)` is
the only entry point now, so a second one that hard-codes the directory cannot
quietly grow the latch back.

The regression tests are
`canvas::tools::tests::a_new_token_is_picked_up_without_a_restart` (a token
rewritten mid-process is carried by the very next request) and
`credentials_written_after_start_up_need_no_restart`.

## 8. A long chat reply can truncate the over-budget note off the end — FIXED

`rendered_turn` now reserves the note — and the ellipsis `truncate` adds —
before cutting the reply, so the concatenation already fits and the
transport's own cut has nothing left to take. Truncation still goes through
`truncate`, which never splits a character; the regression test's reply is
multi-byte throughout, so a cut landing mid-character would panic rather than
pass.

The truncation semantics on the shared notification path are unchanged: the
transport still cuts at `MAX_MESSAGE_BYTES`, and only the chat turn's own
rendering reserves anything. A note longer than the whole budget leaves the
reply nothing, which is the right way round — the notes are the daemon's own
short sentences and the note is the part that must not be lost.

The regression test is
`telegram::tests::a_long_reply_is_cut_to_make_room_for_the_over_budget_note`.
