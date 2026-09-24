//! The Telegram update loop: how a tap on the owner's phone reaches the
//! executor.
//!
//! Without this, [`Notifier::push_action`](crate::notify::telegram::Notifier::push_action)
//! sends a message with two buttons that nothing is listening for. Pressing
//! one would do nothing at all, forever.
//!
//! # Long polling, not a webhook
//!
//! `getUpdates` over an outbound HTTPS connection, deliberately.
//!
//! Every decision this loop makes rests on `callback_query.from.id` being the
//! identity Telegram's servers stamped on the update. Over long polling that
//! holds: the bytes came from `api.telegram.org` over TLS in answer to a
//! request this process made. **Over a webhook it does not.** A webhook update
//! is a plain HTTP POST that anyone who can reach the URL can send, `from.id`
//! included, so the owner check would be worth nothing without also setting
//! `setWebhook`'s `secret_token` and comparing the
//! `X-Telegram-Bot-Api-Secret-Token` header on every request. A webhook also
//! wants an inbound port and a certificate on a laptop that moves between
//! networks. Long polling needs none of that.
//!
//! # Two identity mistakes this module is shaped to prevent
//!
//! 1. [`TelegramUserId`] is constructed in exactly one place in this crate's
//!    production code — [`dispatch`], from `callback_query.from.id`. The
//!    struct field is `pub`, so this is a convention rather than a wall, but
//!    the convention is one grep away from being checkable.
//! 2. `message.chat.id` is **not** an identity. It is checked separately,
//!    against the configured chat, so that a tap delivered in some other chat
//!    the bot was added to is refused even if it carries the owner's user id.
//!
//! Both refusals reply with
//! [`UNRECOGNISED_REPLY`](crate::notify::telegram::UNRECOGNISED_REPLY) — the
//! same sentence a malformed payload gets, so nobody can map the action space
//! by diffing replies.
//!
//! # At-least-once, and why that is safe
//!
//! The offset is persisted after each update is handled, so the same tap is
//! not replayed on the next poll or after a restart. It cannot be
//! exactly-once: the process can die between executing an action and writing
//! the offset. What makes that harmless is that a replay is not a second
//! effect — the `proposed -> approved` transition is a conditional UPDATE, so
//! a redelivered approval finds the row already decided and answers "already
//! handled".

use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use ea_core::store::kv::KvStore;
use serde::Deserialize;

use crate::executor::ToolCaller;
use crate::notify::telegram::{Notifier, TelegramUserId, Transport, UNRECOGNISED_REPLY};

/// `kv` key holding the next `getUpdates` offset.
pub const OFFSET_KEY: &str = "telegram.update_offset";

/// How long Telegram holds a `getUpdates` request open with nothing to say.
///
/// Must stay comfortably below the transport's own 30-second HTTP timeout, or
/// every idle poll would be reported as a network failure.
pub const POLL_TIMEOUT_SECS: u64 = 25;

/// Telegram truncates `answerCallbackQuery`'s `text` past 200 characters.
pub const CALLBACK_TEXT_LIMIT: usize = 200;

/// First backoff after a failed poll; it doubles up to [`MAX_BACKOFF`].
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling on the backoff. A laptop that closed its lid for an hour should
/// reconnect within a minute of waking, not keep doubling into the afternoon.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Pause after a poll that returned nothing.
///
/// A long poll normally *is* the pause: `getUpdates` holds the connection open
/// for [`POLL_TIMEOUT_SECS`] when there is nothing to say. But nothing
/// guarantees that — a Bot API server that answers an empty batch immediately
/// would otherwise turn this loop into a spin that hammers the API and starves
/// everything else on the runtime. One second costs nothing against a
/// twenty-five-second poll and makes the loop's worst case a slow tick rather
/// than a hot one.
pub const IDLE_PAUSE: Duration = Duration::from_secs(1);

// --------------------------------------------------------------------------
// The wire shapes
// --------------------------------------------------------------------------

/// One update, with only the parts this daemon acts on.
///
/// Everything but `update_id` is optional: Telegram adds update kinds over
/// time, and an update this build has never heard of must advance the offset
/// and be ignored, not fail the poll.
#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    /// The id `answerCallbackQuery` must be given, or the owner's phone shows
    /// a spinner on the button until it times out.
    pub id: String,
    /// Who pressed it. **The only identity in the payload.**
    pub from: User,
    /// Where it was pressed. Absent for a message too old for Telegram to
    /// still have; checked against the configured chat when present.
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub chat: Chat,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

// --------------------------------------------------------------------------
// Seams
// --------------------------------------------------------------------------

/// Where updates come from, and where callback answers go.
///
/// A trait so the loop can be driven from a scripted source: no test in this
/// crate may touch `api.telegram.org`. Spelled with an explicit
/// `impl Future + Send` for the same reason as
/// [`Transport`](crate::notify::telegram::Transport): a bare `async fn` in a
/// public trait trips `async_fn_in_trait` under `-D warnings`.
pub trait UpdateSource {
    /// `offset` is the first update id to receive; `None` means "whatever
    /// Telegram still has".
    fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> impl Future<Output = anyhow::Result<Vec<Update>>> + Send;

    /// Dismiss the spinner on the pressed button and show `text`.
    fn answer_callback(
        &self,
        callback_id: &str,
        text: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// What a button press is handed to. Implemented by
/// [`Notifier`](crate::notify::telegram::Notifier); a trait so the loop's own
/// tests do not need an executor and a database behind them.
pub trait CallbackHandler {
    fn handle(&self, from: TelegramUserId, data: &str) -> impl Future<Output = String> + Send;
}

/// The notifier is shared — the triage job pushes through it, the daemon
/// pushes new proposals through it, and this loop feeds taps back into it —
/// so the loop accepts one behind an `Arc` as readily as by value.
impl<H: CallbackHandler + Send + Sync + ?Sized> CallbackHandler for std::sync::Arc<H> {
    fn handle(&self, from: TelegramUserId, data: &str) -> impl Future<Output = String> + Send {
        H::handle(self, from, data)
    }
}

impl<T: Transport + Send + Sync, C: ToolCaller + Send + Sync> CallbackHandler for Notifier<T, C> {
    fn handle(&self, from: TelegramUserId, data: &str) -> impl Future<Output = String> + Send {
        self.handle_callback(from, data)
    }
}

/// The persisted `getUpdates` offset.
pub struct OffsetStore {
    kv: KvStore,
}

impl OffsetStore {
    pub fn new(kv: KvStore) -> Self {
        Self { kv }
    }

    pub fn get(&self) -> anyhow::Result<Option<i64>> {
        Ok(self
            .kv
            .get(OFFSET_KEY)?
            .and_then(|raw| raw.trim().parse::<i64>().ok()))
    }

    /// Record the next offset. Monotonic: a lower value is ignored, so a
    /// reordered or duplicated batch cannot rewind the cursor and replay taps
    /// that were already acted on.
    pub fn set(&self, offset: i64) -> anyhow::Result<()> {
        if self.get()?.is_some_and(|current| current >= offset) {
            return Ok(());
        }
        self.kv.set(OFFSET_KEY, &offset.to_string())
    }
}

// --------------------------------------------------------------------------
// The loop
// --------------------------------------------------------------------------

/// Long-polls Telegram and feeds callback queries to a handler.
pub struct UpdateLoop<S: UpdateSource, H: CallbackHandler> {
    source: S,
    handler: H,
    /// The chat the bot was configured to talk in. A callback from anywhere
    /// else is refused.
    chat_id: i64,
    offsets: OffsetStore,
    poll_timeout_secs: u64,
    initial_backoff: Duration,
    max_backoff: Duration,
    idle_pause: Duration,
}

/// What one poll did, so a caller (and a test) can see it without a log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PollSummary {
    /// Updates received, whatever their kind.
    pub received: usize,
    /// Callback queries actually passed to the handler.
    pub handled: usize,
    /// Callback queries refused before the handler: wrong chat, or no data.
    pub refused: usize,
}

impl<S: UpdateSource, H: CallbackHandler> UpdateLoop<S, H> {
    pub fn new(source: S, handler: H, chat_id: i64, offsets: OffsetStore) -> Self {
        Self {
            source,
            handler,
            chat_id,
            offsets,
            poll_timeout_secs: POLL_TIMEOUT_SECS,
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            idle_pause: IDLE_PAUSE,
        }
    }

    /// Shorten the waits. Tests only — the defaults are what production uses.
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }

    /// Shorten the idle pause. Tests only.
    pub fn with_idle_pause(mut self, pause: Duration) -> Self {
        self.idle_pause = pause;
        self
    }

    pub fn with_poll_timeout_secs(mut self, secs: u64) -> Self {
        self.poll_timeout_secs = secs;
        self
    }

    /// One `getUpdates` round trip and everything it produced.
    ///
    /// The offset is advanced update by update, immediately after each one is
    /// answered, rather than once at the end of the batch. A failure halfway
    /// through a batch therefore costs the remaining updates a redelivery, not
    /// the whole batch.
    pub async fn poll_once(&self) -> anyhow::Result<PollSummary> {
        let offset = self.offsets.get()?;
        let updates = self
            .source
            .get_updates(offset, self.poll_timeout_secs)
            .await
            .context("polling Telegram for updates")?;

        let mut summary = PollSummary {
            received: updates.len(),
            ..PollSummary::default()
        };

        for update in updates {
            let id = update.update_id;
            match self.dispatch(update).await {
                Dispatched::Handled => summary.handled += 1,
                Dispatched::Refused => summary.refused += 1,
                Dispatched::Ignored => {}
            }
            // After the effect, never before: an update that is acted on and
            // then loses its offset write is replayed and refused by the
            // store's status transition. An update whose offset was written
            // first and then failed to act would be lost silently.
            self.offsets.set(id + 1)?;
        }

        Ok(summary)
    }

    /// Handle one update. The **only** place a [`TelegramUserId`] is built.
    async fn dispatch(&self, update: Update) -> Dispatched {
        let Some(query) = update.callback_query else {
            // A message, an edited message, a channel post: not this daemon's
            // business, but its offset still advances.
            return Dispatched::Ignored;
        };

        // `message.chat.id` is a place, not a person. It is checked as a
        // place, and it is never used as the identity.
        if let Some(message) = &query.message {
            if message.chat.id != self.chat_id {
                tracing::warn!(
                    chat = message.chat.id,
                    "refusing a telegram callback from an unconfigured chat"
                );
                self.answer(&query.id, UNRECOGNISED_REPLY).await;
                return Dispatched::Refused;
            }
        }

        let Some(data) = query.data.as_deref() else {
            self.answer(&query.id, UNRECOGNISED_REPLY).await;
            return Dispatched::Refused;
        };

        // `from.id`, and nothing else. `message.chat.id` is right there and
        // would compile; in a group it is not even the same number, and in a
        // DM it would authorise whoever the bot is talking to.
        let from = TelegramUserId(query.from.id);
        let reply = self.handler.handle(from, data).await;
        self.answer(&query.id, &reply).await;
        Dispatched::Handled
    }

    /// Answer the callback query, logging rather than propagating a failure:
    /// the decision has already been made and recorded, and losing the
    /// confirmation toast must not make the loop retry the tap.
    async fn answer(&self, callback_id: &str, text: &str) {
        let text = clip(text, CALLBACK_TEXT_LIMIT);
        if let Err(err) = self.source.answer_callback(callback_id, &text).await {
            tracing::warn!(error = %format!("{err:#}"), "answering a telegram callback failed");
        }
    }

    /// Poll until `shutdown` goes true, backing off after a failure.
    ///
    /// A network failure is expected, not exceptional: a laptop sleeps, a
    /// train goes into a tunnel, Telegram has a bad minute. The loop logs,
    /// waits, and carries on — and because the offset is durable, whatever was
    /// missed arrives on the next successful poll.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut backoff = self.initial_backoff;
        loop {
            if *shutdown.borrow() {
                return;
            }

            let poll = tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                result = self.poll_once() => result,
            };

            match poll {
                Ok(summary) => {
                    backoff = self.initial_backoff;
                    if summary.handled > 0 || summary.refused > 0 {
                        tracing::info!(
                            handled = summary.handled,
                            refused = summary.refused,
                            "telegram callbacks processed"
                        );
                    }
                    if summary.received == 0 {
                        // See IDLE_PAUSE: never poll in a tight loop, whatever
                        // the far end does.
                        tokio::select! {
                            biased;
                            _ = shutdown.changed() => return,
                            _ = tokio::time::sleep(self.idle_pause) => {}
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %format!("{err:#}"),
                        backoff_ms = backoff.as_millis() as u64,
                        "telegram update poll failed; retrying"
                    );
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.max_backoff);
                }
            }
        }
    }
}

enum Dispatched {
    Handled,
    Refused,
    Ignored,
}

/// Cut to `max` characters without splitting one.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// Test doubles shared by this module's two test modules: the scripted
/// `getUpdates` source, a transport that records instead of sending, and a
/// connector spy. Kept in one place so the end-to-end tests drive exactly the
/// source the unit tests do.
#[cfg(test)]
mod tests_support {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::notify::telegram::Transport;

    /// A scripted `getUpdates`: each call pops the next reply, then idles.
    #[derive(Default)]
    pub(super) struct ScriptedSource {
        pub(super) replies: Mutex<Vec<anyhow::Result<Vec<Update>>>>,
        /// `(offset, timeout)` per call, in order.
        polls: Mutex<Vec<(Option<i64>, u64)>>,
        answers: Mutex<Vec<(String, String)>>,
    }

    impl ScriptedSource {
        pub(super) fn with(replies: Vec<anyhow::Result<Vec<Update>>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().rev().collect()),
                ..Default::default()
            })
        }

        pub(super) fn polls(&self) -> Vec<(Option<i64>, u64)> {
            self.polls.lock().unwrap().clone()
        }

        pub(super) fn answers(&self) -> Vec<(String, String)> {
            self.answers.lock().unwrap().clone()
        }
    }

    impl UpdateSource for Arc<ScriptedSource> {
        async fn get_updates(
            &self,
            offset: Option<i64>,
            timeout_secs: u64,
        ) -> anyhow::Result<Vec<Update>> {
            self.polls.lock().unwrap().push((offset, timeout_secs));
            match self.replies.lock().unwrap().pop() {
                Some(reply) => reply,
                // The script ran out: behave like an idle long poll.
                None => Ok(Vec::new()),
            }
        }

        async fn answer_callback(&self, callback_id: &str, text: &str) -> anyhow::Result<()> {
            self.answers
                .lock()
                .unwrap()
                .push((callback_id.to_string(), text.to_string()));
            Ok(())
        }
    }

    /// A [`Transport`] that records what it was asked to send. The notifier's
    /// outgoing messages are not what these tests are about, but a notifier
    /// needs one.
    #[derive(Default)]
    pub(super) struct RecordingTransport {
        pub(super) sent: Mutex<Vec<String>>,
    }

    impl Transport for RecordingTransport {
        async fn send(&self, text: &str, _buttons: &[(String, String)]) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    /// Records every connector call, so a test can assert the strong property:
    /// that the connector was never reached at all.
    #[derive(Default, Clone)]
    pub(super) struct SpyCaller {
        pub(super) calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl crate::executor::ToolCaller for SpyCaller {
        async fn call(
            &self,
            connector: &str,
            tool: &str,
            _args: serde_json::Value,
        ) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string()));
            Ok("recorded".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::tests_support::ScriptedSource;
    use super::*;

    const OWNER_CHAT: i64 = 4242;
    const OWNER_USER: i64 = 555;

    /// Records exactly who the loop said pressed the button.
    #[derive(Default)]
    struct SpyHandler {
        seen: Mutex<Vec<(TelegramUserId, String)>>,
    }

    impl SpyHandler {
        fn seen(&self) -> Vec<(TelegramUserId, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl CallbackHandler for Arc<SpyHandler> {
        async fn handle(&self, from: TelegramUserId, data: &str) -> String {
            self.seen.lock().unwrap().push((from, data.to_string()));
            format!("handled {data}")
        }
    }

    /// A handler whose reply is longer than Telegram will accept.
    struct LongWinded;

    impl CallbackHandler for LongWinded {
        async fn handle(&self, _from: TelegramUserId, _data: &str) -> String {
            "x".repeat(500)
        }
    }

    fn callback(update_id: i64, from: i64, chat: Option<i64>, data: Option<&str>) -> Update {
        Update {
            update_id,
            callback_query: Some(CallbackQuery {
                id: format!("cb-{update_id}"),
                from: User { id: from },
                message: chat.map(|id| Message { chat: Chat { id } }),
                data: data.map(str::to_string),
            }),
        }
    }

    fn tap(update_id: i64, data: &str) -> Update {
        callback(update_id, OWNER_USER, Some(OWNER_CHAT), Some(data))
    }

    struct Fixture {
        _dir: TempDir,
        conn: Arc<Mutex<Connection>>,
        source: Arc<ScriptedSource>,
        handler: Arc<SpyHandler>,
    }

    impl Fixture {
        fn new(replies: Vec<anyhow::Result<Vec<Update>>>) -> Self {
            let dir = TempDir::new().unwrap();
            let conn = Arc::new(Mutex::new(
                ea_core::db::open(&dir.path().join("state.db")).unwrap(),
            ));
            Self {
                _dir: dir,
                conn,
                source: ScriptedSource::with(replies),
                handler: Arc::new(SpyHandler::default()),
            }
        }

        fn offsets(&self) -> OffsetStore {
            OffsetStore::new(KvStore::new(Arc::clone(&self.conn)))
        }

        /// A fresh loop over the same source, handler and database — which is
        /// also what a restart looks like from the offset's point of view.
        fn build(&self) -> UpdateLoop<Arc<ScriptedSource>, Arc<SpyHandler>> {
            UpdateLoop::new(
                Arc::clone(&self.source),
                Arc::clone(&self.handler),
                OWNER_CHAT,
                self.offsets(),
            )
            .with_backoff(Duration::from_millis(5), Duration::from_millis(20))
            .with_idle_pause(Duration::from_millis(1))
        }
    }

    #[tokio::test]
    async fn a_callback_reaches_the_handler_with_the_sender_id() {
        let f = Fixture::new(vec![Ok(vec![tap(7, "approve:3")])]);
        let summary = f.build().poll_once().await.unwrap();

        assert_eq!(summary.received, 1);
        assert_eq!(summary.handled, 1);
        assert_eq!(
            f.handler.seen(),
            vec![(TelegramUserId(OWNER_USER), "approve:3".to_string())]
        );
        assert_eq!(
            f.source.answers(),
            vec![("cb-7".to_string(), "handled approve:3".to_string())]
        );
    }

    /// The review focus of the whole module. `message.chat.id` is in the same
    /// payload and would compile in place of `from.id`; getting it wrong
    /// authorises whoever the bot happens to be talking to.
    #[tokio::test]
    async fn the_identity_is_from_id_and_never_chat_id() {
        let f = Fixture::new(vec![Ok(vec![callback(
            1,
            OWNER_USER,
            Some(OWNER_CHAT),
            Some("approve:9"),
        )])]);
        f.build().poll_once().await.unwrap();

        let seen = f.handler.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].0,
            TelegramUserId(OWNER_USER),
            "the user id must come from callback_query.from.id"
        );
        assert_ne!(
            seen[0].0,
            TelegramUserId(OWNER_CHAT),
            "the chat id must never be used as an identity"
        );
    }

    #[tokio::test]
    async fn a_callback_from_another_chat_never_reaches_the_handler() {
        let f = Fixture::new(vec![Ok(vec![callback(
            2,
            OWNER_USER,
            Some(999_999),
            Some("approve:4"),
        )])]);
        let summary = f.build().poll_once().await.unwrap();

        assert_eq!(summary.refused, 1);
        assert_eq!(summary.handled, 0);
        assert!(
            f.handler.seen().is_empty(),
            "a tap in an unconfigured chat must not reach the decision code"
        );
        assert_eq!(f.source.answers()[0].1, UNRECOGNISED_REPLY);
    }

    #[tokio::test]
    async fn a_callback_with_no_data_is_refused_politely() {
        let f = Fixture::new(vec![Ok(vec![callback(
            3,
            OWNER_USER,
            Some(OWNER_CHAT),
            None,
        )])]);
        let summary = f.build().poll_once().await.unwrap();
        assert_eq!(summary.refused, 1);
        assert!(f.handler.seen().is_empty());
        assert_eq!(f.source.answers()[0].1, UNRECOGNISED_REPLY);
    }

    /// A callback whose message Telegram no longer has still carries `from`,
    /// which is the only field the decision needs.
    #[tokio::test]
    async fn a_callback_with_no_message_is_still_handled() {
        let f = Fixture::new(vec![Ok(vec![callback(
            4,
            OWNER_USER,
            None,
            Some("reject:1"),
        )])]);
        assert_eq!(f.build().poll_once().await.unwrap().handled, 1);
    }

    /// An update kind this build has never heard of must advance the offset
    /// rather than wedging the loop on it forever.
    #[tokio::test]
    async fn a_non_callback_update_is_ignored_but_still_advances_the_offset() {
        let f = Fixture::new(vec![Ok(vec![Update {
            update_id: 11,
            callback_query: None,
        }])]);
        let summary = f.build().poll_once().await.unwrap();
        assert_eq!(
            (summary.received, summary.handled, summary.refused),
            (1, 0, 0)
        );
        assert_eq!(f.offsets().get().unwrap(), Some(12));
    }

    #[tokio::test]
    async fn the_offset_advances_so_the_same_tap_is_not_processed_twice() {
        let f = Fixture::new(vec![
            Ok(vec![tap(7, "approve:3")]),
            // Telegram's answer once the offset has confirmed update 7.
            Ok(vec![]),
        ]);
        let lp = f.build();
        lp.poll_once().await.unwrap();
        lp.poll_once().await.unwrap();

        assert_eq!(
            f.source.polls(),
            vec![(None, POLL_TIMEOUT_SECS), (Some(8), POLL_TIMEOUT_SECS)],
            "the second poll must confirm the first update"
        );
        assert_eq!(f.handler.seen().len(), 1, "the tap must be handled once");
    }

    /// The offset is in the database, not in memory: a restart must not
    /// re-ask Telegram for taps that were already acted on.
    #[tokio::test]
    async fn the_offset_survives_a_restart() {
        let f = Fixture::new(vec![Ok(vec![tap(20, "approve:1")])]);
        f.build().poll_once().await.unwrap();

        // A brand-new loop over the same database.
        f.build().poll_once().await.unwrap();

        assert_eq!(
            f.source.polls()[1].0,
            Some(21),
            "a restarted loop must resume from the persisted offset"
        );
        assert_eq!(f.handler.seen().len(), 1);
    }

    /// A reordered or duplicated batch must not rewind the cursor and replay
    /// taps that have already been acted on.
    #[test]
    fn the_offset_never_moves_backwards() {
        let dir = TempDir::new().unwrap();
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        let offsets = OffsetStore::new(KvStore::new(conn));
        offsets.set(50).unwrap();
        offsets.set(20).unwrap();
        assert_eq!(offsets.get().unwrap(), Some(50));
        offsets.set(51).unwrap();
        assert_eq!(offsets.get().unwrap(), Some(51));
    }

    #[tokio::test]
    async fn a_long_reply_is_cut_to_telegrams_limit() {
        let dir = TempDir::new().unwrap();
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        let source = ScriptedSource::with(vec![Ok(vec![tap(1, "approve:1")])]);
        let lp = UpdateLoop::new(
            Arc::clone(&source),
            LongWinded,
            OWNER_CHAT,
            OffsetStore::new(KvStore::new(conn)),
        );
        lp.poll_once().await.unwrap();
        assert_eq!(source.answers()[0].1.chars().count(), CALLBACK_TEXT_LIMIT);
    }

    #[tokio::test]
    async fn a_failed_poll_is_an_error_the_caller_sees() {
        let f = Fixture::new(vec![Err(anyhow::anyhow!("connection reset"))]);
        let err = f.build().poll_once().await.unwrap_err();
        assert!(format!("{err:#}").contains("connection reset"));
    }

    /// Addition 1's durability requirement: the loop must survive a network
    /// failure and resume, rather than exiting and leaving the owner's buttons
    /// dead until the next restart.
    #[tokio::test]
    async fn the_loop_survives_a_network_failure_and_resumes() {
        let f = Fixture::new(vec![
            Err(anyhow::anyhow!("dns failure")),
            Err(anyhow::anyhow!("connection reset by peer")),
            Ok(vec![tap(31, "approve:5")]),
        ]);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handler = Arc::clone(&f.handler);
        let task = tokio::spawn(f.build().run(rx));

        // Wait for the tap that only arrives after two failed polls.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handler.seen().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the loop stopped polling after a network failure"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop must stop when told to")
            .unwrap();

        assert_eq!(
            f.handler.seen(),
            vec![(TelegramUserId(OWNER_USER), "approve:5".to_string())]
        );
        assert_eq!(
            f.offsets().get().unwrap(),
            Some(32),
            "the recovered poll must still confirm its update"
        );
    }

    #[tokio::test]
    async fn the_loop_stops_when_shutdown_is_signalled_before_it_starts() {
        let f = Fixture::new(vec![]);
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), f.build().run(rx))
            .await
            .expect("an already-signalled shutdown must return immediately");
        assert!(f.source.polls().is_empty());
    }
}

/// The loop wired to the real [`Notifier`], so the owner check is exercised
/// where it actually sits — between a tap arriving and an action executing —
/// rather than only in the notifier's own unit tests.
///
/// Nothing here touches the network: the transport is a recorder and the
/// connector is a spy.
#[cfg(test)]
mod end_to_end {
    use std::sync::{Arc, Mutex};

    use chrono::Duration as ChronoDuration;
    use ea_core::policy::Policy;
    use ea_core::store::actions::{ActionStore, ProposeInput};
    use ea_core::store::runs::RunStore;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::tests_support::*;
    use super::*;
    use crate::executor::Executor;
    use crate::notify::telegram::Notifier;

    const OWNER: TelegramUserId = TelegramUserId(10_000_001);
    const STRANGER: TelegramUserId = TelegramUserId(10_000_002);
    const CHAT: i64 = 4242;

    /// The loop as production builds it: a scripted source in place of the
    /// network, and the real notifier behind it.
    type RealLoop = UpdateLoop<Arc<ScriptedSource>, Arc<Notifier<RecordingTransport, SpyCaller>>>;

    struct Harness {
        _dir: TempDir,
        actions: ActionStore,
        calls: Arc<Mutex<Vec<(String, String)>>>,
        notifier: Arc<Notifier<RecordingTransport, SpyCaller>>,
        conn: Arc<Mutex<Connection>>,
    }

    fn harness() -> Harness {
        let dir = TempDir::new().unwrap();
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        let caller = SpyCaller::default();
        let calls = Arc::clone(&caller.calls);
        let executor = Arc::new(Executor::new(
            ActionStore::new(Arc::clone(&conn)),
            RunStore::new(Arc::clone(&conn)),
            Policy::parse("[fortnox]\nrecord_voucher = \"approve\"\n").unwrap(),
            caller,
        ));
        let notifier = Arc::new(Notifier::new(
            RecordingTransport::default(),
            ActionStore::new(Arc::clone(&conn)),
            executor,
            OWNER,
        ));
        Harness {
            _dir: dir,
            actions: ActionStore::new(Arc::clone(&conn)),
            calls,
            notifier,
            conn,
        }
    }

    impl Harness {
        fn propose(&self) -> i64 {
            self.actions
                .propose(ProposeInput {
                    connector: "fortnox".into(),
                    tool: "record_voucher".into(),
                    args: serde_json::json!({ "amount": 1200 }),
                    preview: "Record a voucher for 1200 SEK".into(),
                    rationale: "the invoice arrived".into(),
                    ttl: ChronoDuration::hours(24),
                })
                .unwrap()
                .id
        }

        fn drive(&self, updates: Vec<Update>) -> (Arc<ScriptedSource>, RealLoop) {
            let source = ScriptedSource::with(vec![Ok(updates)]);
            let lp = UpdateLoop::new(
                Arc::clone(&source),
                Arc::clone(&self.notifier),
                CHAT,
                OffsetStore::new(KvStore::new(Arc::clone(&self.conn))),
            );
            (source, lp)
        }

        fn status(&self, id: i64) -> String {
            self.actions
                .get(id)
                .unwrap()
                .unwrap()
                .status
                .as_str()
                .to_string()
        }
    }

    fn press(update_id: i64, from: TelegramUserId, data: &str) -> Update {
        Update {
            update_id,
            callback_query: Some(CallbackQuery {
                id: format!("cb-{update_id}"),
                from: User { id: from.0 },
                message: Some(Message {
                    chat: Chat { id: CHAT },
                }),
                data: Some(data.to_string()),
            }),
        }
    }

    #[tokio::test]
    async fn the_owners_tap_approves_and_executes() {
        let h = harness();
        let id = h.propose();
        let (source, lp) = h.drive(vec![press(1, OWNER, &format!("approve:{id}"))]);

        lp.poll_once().await.unwrap();

        assert_eq!(h.status(id), "executed");
        assert_eq!(
            *h.calls.lock().unwrap(),
            vec![("fortnox".to_string(), "record_voucher".to_string())]
        );
        let answer = &source.answers()[0];
        assert_eq!(answer.0, "cb-1");
        assert!(
            answer.1.contains(&format!("Executed #{id}")),
            "{}",
            answer.1
        );
    }

    #[tokio::test]
    async fn a_strangers_tap_changes_nothing_and_says_nothing() {
        let h = harness();
        let id = h.propose();
        let (source, lp) = h.drive(vec![press(1, STRANGER, &format!("approve:{id}"))]);

        lp.poll_once().await.unwrap();

        assert_eq!(
            h.status(id),
            "proposed",
            "a stranger must not decide anything"
        );
        assert!(
            h.calls.lock().unwrap().is_empty(),
            "the connector must not be reached"
        );
        assert_eq!(
            source.answers()[0].1,
            UNRECOGNISED_REPLY,
            "the reply must not reveal that this action exists"
        );
    }

    #[tokio::test]
    async fn the_owners_reject_never_calls_the_connector() {
        let h = harness();
        let id = h.propose();
        let (_source, lp) = h.drive(vec![press(1, OWNER, &format!("reject:{id}"))]);

        lp.poll_once().await.unwrap();

        assert_eq!(h.status(id), "rejected");
        assert!(h.calls.lock().unwrap().is_empty());
    }

    /// At-least-once delivery is safe because the store's transition is the
    /// claim: a redelivered tap finds the row already decided.
    #[tokio::test]
    async fn a_redelivered_tap_does_not_execute_twice() {
        let h = harness();
        let id = h.propose();
        let data = format!("approve:{id}");
        let (source, lp) = h.drive(vec![press(1, OWNER, &data), press(1, OWNER, &data)]);

        lp.poll_once().await.unwrap();

        assert_eq!(
            h.calls.lock().unwrap().len(),
            1,
            "the second delivery must not produce a second voucher"
        );
        assert!(
            source.answers()[1].1.contains("already handled"),
            "{:?}",
            source.answers()
        );
    }
}
