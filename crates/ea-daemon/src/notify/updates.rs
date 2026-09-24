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
//! 1. [`TelegramUserId`] is constructed in exactly two places in this crate's
//!    production code, both in `dispatch`: from `callback_query.from.id` for a
//!    button press, and from `message.from.id` for a free-text message. The
//!    struct field is `pub`, so this is a convention rather than a wall, but
//!    the convention is one grep away from being checkable.
//! 2. `message.chat.id` is **not** an identity. It is checked separately,
//!    against the configured chat, so that a tap or a message delivered in
//!    some other chat the bot was added to is refused even if it carries the
//!    owner's user id. The two fields sit side by side in the same JSON object
//!    and have the same type; using the chat as the person would authenticate
//!    everyone in the chat, and in a DM it would authenticate whoever is
//!    talking to the bot.
//!
//! Both refusals reply with
//! [`UNRECOGNISED_REPLY`](crate::notify::telegram::UNRECOGNISED_REPLY) — the
//! same sentence a malformed payload gets, so nobody can map the action space
//! by diffing replies.
//!
//! # Delivery: at most once, because a duplicate costs money
//!
//! Two durable marks, written at opposite ends of handling one update, and
//! the difference between them is the whole design.
//!
//! [`OffsetStore`] is the cursor Telegram is polled with, and it is written
//! **after** an update is handled. Telegram redelivers an update until its
//! offset is confirmed, so a process that dies mid-batch gets the rest of the
//! batch again rather than losing it.
//!
//! [`HandledUpdates`] is the idempotency key, and it is written **before** the
//! handler runs. It is what makes a redelivery a no-op. Without it, a crash
//! between answering a message and writing the offset would hand the same
//! message back after the restart and answer it a second time — and a
//! free-text message is a `claude` session charged against the day's budget,
//! so "answer it again" means "bill the owner again", silently.
//!
//! Claiming before the handler chooses the cheaper failure deliberately. A
//! crash *during* handling now costs the outcome rather than producing a
//! duplicate:
//!
//! * A message loses its answer. The owner sees no reply and re-sends — one
//!   session, on purpose, and they know they are spending it.
//! * A tap loses its press. The buttons are still on the phone and the action
//!   is still `proposed`, so tapping again does exactly what the first tap
//!   would have. (A crash *after* the `proposed -> approved` transition was
//!   never recoverable by replay anyway: the conditional UPDATE means a
//!   redelivered approval already only answered "already handled".)
//!
//! Both of those are a re-do the owner can see and repeat. A duplicate
//! charge is neither.

use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use ea_core::store::kv::KvStore;
use serde::Deserialize;

use crate::executor::ToolCaller;
use crate::notify::telegram::{Notifier, TelegramUserId, Transport, UNRECOGNISED_REPLY};

/// `kv` key holding the next `getUpdates` offset.
pub const OFFSET_KEY: &str = "telegram.update_offset";

/// `kv` key holding the highest `update_id` this daemon has taken
/// responsibility for. See [`HandledUpdates`].
pub const HANDLED_KEY: &str = "telegram.handled_through";

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
    /// A free-text message. Requested since the chat surface exists; before
    /// that `allowed_updates` asked for callback queries alone.
    #[serde(default)]
    pub message: Option<Message>,
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
    /// Telegram stamps this. A bot's own messages are not delivered back to
    /// it by `getUpdates`, but another bot in the same group is, and a loop
    /// between two bots answering each other is a cost bug with no ceiling.
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    /// Where it was sent. Optional only so that a message shaped in some way
    /// this build has never seen cannot fail the *whole* poll batch: a missing
    /// chat is treated as "not the configured chat" and ignored, never as
    /// "check skipped".
    #[serde(default)]
    pub chat: Option<Chat>,
    /// Who sent it. **The only identity in a message update.** Optional
    /// because Telegram omits it for channel posts, which have no author.
    #[serde(default)]
    pub from: Option<User>,
    /// Absent on a sticker, a photo, a location, or a service message.
    #[serde(default)]
    pub text: Option<String>,
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

    /// Send a reply into `chat_id`. Used for free-text messages, which have no
    /// callback query to answer.
    fn send_message(
        &self,
        chat_id: i64,
        text: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// What a button press is handed to. Implemented by
/// [`Notifier`](crate::notify::telegram::Notifier); a trait so the loop's own
/// tests do not need an executor and a database behind them.
pub trait CallbackHandler {
    fn handle(&self, from: TelegramUserId, data: &str) -> impl Future<Output = String> + Send;
}

/// What a free-text message is handed to. Implemented by
/// [`Notifier`](crate::notify::telegram::Notifier), which checks the owner
/// before it does anything at all.
pub trait MessageHandler {
    fn handle_message(
        &self,
        from: TelegramUserId,
        text: &str,
    ) -> impl Future<Output = String> + Send;
}

impl<H: MessageHandler + Send + Sync + ?Sized> MessageHandler for std::sync::Arc<H> {
    fn handle_message(
        &self,
        from: TelegramUserId,
        text: &str,
    ) -> impl Future<Output = String> + Send {
        H::handle_message(self, from, text)
    }
}

impl<T: Transport + Send + Sync, C: ToolCaller + Send + Sync> MessageHandler for Notifier<T, C> {
    fn handle_message(
        &self,
        from: TelegramUserId,
        text: &str,
    ) -> impl Future<Output = String> + Send {
        Notifier::handle_message(self, from, text)
    }
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

/// Which updates this daemon has already taken responsibility for.
///
/// A high-water mark rather than a set: Telegram issues `update_id`s in
/// increasing order per bot, so "everything up to and including *n*" is the
/// whole truth about what has been claimed, and it is one small `kv` row
/// instead of a list that grows forever.
///
/// Durable for the same reason [`crate::notify::log::NotificationLog`] is: the
/// daemon runs under `launchd` with `KeepAlive`, so "the process came back" is
/// routine, and a claim held in memory would be forgotten by exactly the crash
/// it exists to survive.
///
/// See the module docs for why this is written *before* the handler runs and
/// the offset after it.
pub struct HandledUpdates {
    kv: KvStore,
}

impl HandledUpdates {
    pub fn new(kv: KvStore) -> Self {
        Self { kv }
    }

    /// The highest claimed `update_id`, or `None` on a fresh database.
    pub fn high_water(&self) -> anyhow::Result<Option<i64>> {
        Ok(self
            .kv
            .get(HANDLED_KEY)?
            .and_then(|raw| raw.trim().parse::<i64>().ok()))
    }

    /// Whether `update_id` has already been claimed — and so must not be
    /// acted on a second time.
    pub fn contains(&self, update_id: i64) -> anyhow::Result<bool> {
        Ok(self
            .high_water()?
            .is_some_and(|claimed| claimed >= update_id))
    }

    /// Claim `update_id` before anything is done about it.
    ///
    /// Monotonic, like [`OffsetStore::set`] and for the same reason: a
    /// redelivered or reordered batch must never lower the mark and make an
    /// update claimable again.
    pub fn mark(&self, update_id: i64) -> anyhow::Result<()> {
        if self.contains(update_id)? {
            return Ok(());
        }
        self.kv.set(HANDLED_KEY, &update_id.to_string())
    }
}

// --------------------------------------------------------------------------
// The loop
// --------------------------------------------------------------------------

/// Long-polls Telegram and feeds callback queries to a handler.
pub struct UpdateLoop<S: UpdateSource, H: CallbackHandler + MessageHandler> {
    source: S,
    handler: H,
    /// The chat the bot was configured to talk in. A callback from anywhere
    /// else is refused.
    chat_id: i64,
    offsets: OffsetStore,
    /// Written before each update is handled; see the module docs.
    handled: HandledUpdates,
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
    /// Callback queries and messages actually passed to a handler.
    pub handled: usize,
    /// Updates refused before the handler: wrong chat, or no callback data.
    pub refused: usize,
    /// Updates Telegram delivered again that this daemon had already claimed.
    /// Expected after a crash mid-handling; a steady stream of them means the
    /// offset is not being written and wants looking at.
    pub duplicates: usize,
}

impl<S: UpdateSource, H: CallbackHandler + MessageHandler> UpdateLoop<S, H> {
    pub fn new(
        source: S,
        handler: H,
        chat_id: i64,
        offsets: OffsetStore,
        handled: HandledUpdates,
    ) -> Self {
        Self {
            source,
            handler,
            chat_id,
            offsets,
            handled,
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
    ///
    /// Each update is claimed in [`HandledUpdates`] before it is handled, and
    /// one Telegram has already been answered for is skipped however often it
    /// is redelivered. See the module docs for why the two marks are written
    /// at opposite ends.
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

            // Telegram redelivers until the offset is confirmed, so this is
            // the ordinary shape of a crash recovery, not an anomaly.
            if self.handled.contains(id)? {
                tracing::info!(
                    update = id,
                    "skipping a telegram update this daemon has already answered"
                );
                summary.duplicates += 1;
                self.offsets.set(id + 1)?;
                continue;
            }

            // Before the effect, never after: a chat turn is a paid session,
            // and an update answered but not marked is answered again after a
            // restart. Marked-but-not-answered costs the owner a re-send or a
            // second tap, which they can see and redo; charged twice for one
            // sentence is silent. See the module docs.
            self.handled.mark(id)?;

            match self.dispatch(update).await {
                Dispatched::Handled => summary.handled += 1,
                Dispatched::Refused => summary.refused += 1,
                Dispatched::Ignored => {}
            }
            // After the effect, so that a batch interrupted halfway through is
            // redelivered rather than lost; the claim above is what keeps the
            // redelivery from being acted on twice.
            self.offsets.set(id + 1)?;
        }

        Ok(summary)
    }

    /// Handle one update. The **only** place a [`TelegramUserId`] is built.
    async fn dispatch(&self, update: Update) -> Dispatched {
        let Some(query) = update.callback_query else {
            return match update.message {
                Some(message) => self.dispatch_message(message).await,
                // An edited message, a channel post, an update kind this build
                // has never heard of: not this daemon's business, but its
                // offset still advances.
                None => Dispatched::Ignored,
            };
        };

        // `message.chat.id` is a place, not a person. It is checked as a
        // place, and it is never used as the identity.
        if let Some(chat) = query.message.as_ref().and_then(|m| m.chat.as_ref()) {
            if chat.id != self.chat_id {
                tracing::warn!(
                    chat = chat.id,
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

    /// Handle one free-text message.
    ///
    /// Three checks, in this order and for three different reasons:
    ///
    /// 1. **The chat**, because `chat.id` is a place: a message in some other
    ///    chat the bot was added to is not this daemon's business, and
    ///    answering it would leak the assistant into that room.
    /// 2. **The sender is not a bot**, because two bots in one group will
    ///    otherwise answer each other forever at a model's price per turn.
    ///    The bot's own outgoing messages are not delivered back to it by
    ///    `getUpdates`, so this is about the other ones.
    /// 3. **The sender**, `message.from.id` and never `chat.id` — done by the
    ///    handler, which owns the configured owner id and answers a stranger
    ///    with the same sentence a malformed callback gets.
    async fn dispatch_message(&self, message: Message) -> Dispatched {
        let Some(chat) = message.chat.as_ref().map(|chat| chat.id) else {
            tracing::debug!("ignoring a telegram message with no chat");
            return Dispatched::Ignored;
        };
        if chat != self.chat_id {
            tracing::warn!(
                chat,
                "ignoring a telegram message from an unconfigured chat"
            );
            // No reply at all: this is a room the owner did not configure, and
            // anything sent there is noise to people who did not ask for it.
            return Dispatched::Refused;
        }

        let Some(from) = message.from.as_ref() else {
            // A channel post has no author, so there is nobody to authorise.
            return Dispatched::Ignored;
        };
        if from.is_bot {
            tracing::debug!(from = from.id, "ignoring a telegram message from a bot");
            return Dispatched::Ignored;
        }

        // `from.id`, and nothing else. `message.chat.id` is the field directly
        // above it in the same object and would compile.
        let sender = TelegramUserId(from.id);
        let text = message.text.as_deref().unwrap_or_default();
        let reply = self.handler.handle_message(sender, text).await;
        self.reply(chat, &reply).await;
        Dispatched::Handled
    }

    /// Send a reply to a message, logging rather than propagating a failure:
    /// the turn is already recorded, and a failed send must not make the loop
    /// replay the message and run a second session.
    async fn reply(&self, chat_id: i64, text: &str) {
        if let Err(err) = self.source.send_message(chat_id, text).await {
            tracing::warn!(error = %format!("{err:#}"), "replying to a telegram message failed");
        }
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
                    if summary.handled > 0 || summary.refused > 0 || summary.duplicates > 0 {
                        tracing::info!(
                            handled = summary.handled,
                            refused = summary.refused,
                            duplicates = summary.duplicates,
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
        /// `(chat_id, text)` per reply the loop sent to a message.
        sent: Mutex<Vec<(i64, String)>>,
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

        pub(super) fn sent(&self) -> Vec<(i64, String)> {
            self.sent.lock().unwrap().clone()
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

        async fn send_message(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push((chat_id, text.to_string()));
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

    /// Records exactly who the loop said pressed the button, and who it said
    /// sent a message.
    #[derive(Default)]
    struct SpyHandler {
        seen: Mutex<Vec<(TelegramUserId, String)>>,
        messages: Mutex<Vec<(TelegramUserId, String)>>,
    }

    impl SpyHandler {
        fn seen(&self) -> Vec<(TelegramUserId, String)> {
            self.seen.lock().unwrap().clone()
        }

        fn messages(&self) -> Vec<(TelegramUserId, String)> {
            self.messages.lock().unwrap().clone()
        }
    }

    impl CallbackHandler for Arc<SpyHandler> {
        async fn handle(&self, from: TelegramUserId, data: &str) -> String {
            self.seen.lock().unwrap().push((from, data.to_string()));
            format!("handled {data}")
        }
    }

    impl MessageHandler for Arc<SpyHandler> {
        async fn handle_message(&self, from: TelegramUserId, text: &str) -> String {
            self.messages.lock().unwrap().push((from, text.to_string()));
            format!("answered {text}")
        }
    }

    /// A handler whose reply is longer than Telegram will accept.
    struct LongWinded;

    impl CallbackHandler for LongWinded {
        async fn handle(&self, _from: TelegramUserId, _data: &str) -> String {
            "x".repeat(500)
        }
    }

    impl MessageHandler for LongWinded {
        async fn handle_message(&self, _from: TelegramUserId, _text: &str) -> String {
            "x".repeat(500)
        }
    }

    fn callback(update_id: i64, from: i64, chat: Option<i64>, data: Option<&str>) -> Update {
        Update {
            update_id,
            message: None,
            callback_query: Some(CallbackQuery {
                id: format!("cb-{update_id}"),
                from: User {
                    id: from,
                    is_bot: false,
                },
                message: chat.map(|id| Message {
                    chat: Some(Chat { id }),
                    from: None,
                    text: None,
                }),
                data: data.map(str::to_string),
            }),
        }
    }

    fn tap(update_id: i64, data: &str) -> Update {
        callback(update_id, OWNER_USER, Some(OWNER_CHAT), Some(data))
    }

    /// A message update, built the way Telegram delivers one: the sender in
    /// `message.from`, the place in `message.chat`.
    fn message(update_id: i64, from: i64, chat: i64, text: Option<&str>) -> Update {
        Update {
            update_id,
            callback_query: None,
            message: Some(Message {
                chat: Some(Chat { id: chat }),
                from: Some(User {
                    id: from,
                    is_bot: false,
                }),
                text: text.map(str::to_string),
            }),
        }
    }

    fn says(update_id: i64, text: &str) -> Update {
        message(update_id, OWNER_USER, OWNER_CHAT, Some(text))
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

        fn handled(&self) -> HandledUpdates {
            HandledUpdates::new(KvStore::new(Arc::clone(&self.conn)))
        }

        /// A fresh loop over the same source, handler and database — which is
        /// also what a restart looks like from the offset's point of view.
        fn build(&self) -> UpdateLoop<Arc<ScriptedSource>, Arc<SpyHandler>> {
            UpdateLoop::new(
                Arc::clone(&self.source),
                Arc::clone(&self.handler),
                OWNER_CHAT,
                self.offsets(),
                self.handled(),
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

    // -- free-text messages -------------------------------------------------

    /// The identity in a message update is `message.from.id`. `message.chat.id`
    /// is in the same object, is also an `i64`, and would compile — and in the
    /// owner's own DM it is *nearly* the right number, which is what makes the
    /// mistake survivable long enough to ship.
    #[tokio::test]
    async fn a_message_reaches_the_handler_with_from_id_and_the_reply_goes_to_the_chat() {
        let f = Fixture::new(vec![Ok(vec![says(12, "when is the tenta?")])]);
        let summary = f.build().poll_once().await.unwrap();

        assert_eq!(
            (summary.received, summary.handled, summary.refused),
            (1, 1, 0)
        );
        assert_eq!(
            f.handler.messages(),
            vec![(TelegramUserId(OWNER_USER), "when is the tenta?".to_string())],
            "the sender is message.from.id"
        );
        assert_ne!(
            OWNER_USER, OWNER_CHAT,
            "the fixture's chat and user ids must differ, or this proves nothing"
        );
        assert_eq!(
            f.source.sent(),
            vec![(OWNER_CHAT, "answered when is the tenta?".to_string())],
            "the answer goes back to the chat it came from"
        );
        assert!(f.handler.seen().is_empty(), "no callback was involved");
        assert_eq!(f.offsets().get().unwrap(), Some(13));
    }

    /// A message in some other chat the bot was added to is not this daemon's
    /// business — and it gets no reply at all, so the assistant does not
    /// announce itself in a room nobody configured.
    #[tokio::test]
    async fn a_message_from_an_unconfigured_chat_is_refused_silently() {
        let f = Fixture::new(vec![Ok(vec![message(
            13,
            OWNER_USER,
            OWNER_CHAT + 1,
            Some("hello"),
        )])]);
        let summary = f.build().poll_once().await.unwrap();

        assert_eq!((summary.handled, summary.refused), (0, 1));
        assert!(f.handler.messages().is_empty());
        assert!(f.source.sent().is_empty());
        assert_eq!(
            f.offsets().get().unwrap(),
            Some(14),
            "the offset still advances"
        );
    }

    /// Two bots in one group answering each other is a cost bug with no
    /// ceiling.
    #[tokio::test]
    async fn a_message_from_a_bot_is_ignored() {
        let mut update = says(14, "beep");
        if let Some(message) = update.message.as_mut() {
            if let Some(from) = message.from.as_mut() {
                from.is_bot = true;
            }
        }
        let f = Fixture::new(vec![Ok(vec![update])]);
        let summary = f.build().poll_once().await.unwrap();

        assert_eq!((summary.handled, summary.refused), (0, 0));
        assert!(f.handler.messages().is_empty());
        assert!(f.source.sent().is_empty());
    }

    /// A message with no text — a sticker, a photo — still belongs to the
    /// owner, so the handler decides what to say about it rather than the loop
    /// dropping it.
    #[tokio::test]
    async fn a_message_with_no_text_still_reaches_the_handler() {
        let f = Fixture::new(vec![Ok(vec![message(15, OWNER_USER, OWNER_CHAT, None)])]);
        f.build().poll_once().await.unwrap();
        assert_eq!(
            f.handler.messages(),
            vec![(TelegramUserId(OWNER_USER), String::new())]
        );
    }

    /// A channel post has no author, so there is nobody to authorise.
    #[tokio::test]
    async fn a_message_with_no_sender_is_ignored() {
        let mut update = says(16, "posted");
        if let Some(message) = update.message.as_mut() {
            message.from = None;
        }
        let f = Fixture::new(vec![Ok(vec![update])]);
        let summary = f.build().poll_once().await.unwrap();
        assert_eq!((summary.handled, summary.refused), (0, 0));
        assert!(f.handler.messages().is_empty());
    }

    /// An update kind this build has never heard of must advance the offset
    /// rather than wedging the loop on it forever.
    #[tokio::test]
    async fn a_non_callback_update_is_ignored_but_still_advances_the_offset() {
        let f = Fixture::new(vec![Ok(vec![Update {
            update_id: 11,
            callback_query: None,
            message: None,
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

    /// Telegram redelivers an update until its offset is confirmed, so a
    /// crash between answering a message and committing the offset hands the
    /// same message back after the restart. A free-text message costs a
    /// `claude` session, charged against the daily budget, so answering it
    /// twice charges the owner twice for the same sentence.
    #[tokio::test]
    async fn a_redelivered_message_is_answered_once_across_a_restart() {
        let f = Fixture::new(vec![
            Ok(vec![says(7, "when is the tenta?")]),
            Ok(vec![says(7, "when is the tenta?")]),
        ]);

        f.build().poll_once().await.unwrap();
        assert_eq!(f.handler.messages().len(), 1, "the first delivery answers");

        // The crash: the reply went out, the offset write never landed, so
        // the restarted daemon asks Telegram for update 7 again.
        KvStore::new(Arc::clone(&f.conn))
            .set(OFFSET_KEY, "7")
            .unwrap();

        let summary = f.build().poll_once().await.unwrap();

        assert_eq!(
            f.handler.messages().len(),
            1,
            "one session, not two: the update was claimed before it was answered"
        );
        assert_eq!(summary.received, 1);
        assert_eq!(summary.duplicates, 1);
        assert_eq!(summary.handled, 0);
        assert_eq!(
            f.source.sent().len(),
            1,
            "and the owner is not answered twice either"
        );
        // The cursor still moves past it, or the loop would stick on it.
        assert_eq!(f.offsets().get().unwrap(), Some(8));
    }

    /// The claim is written before the handler runs, so a crash *during* the
    /// session costs the answer rather than charging for a second one.
    #[tokio::test]
    async fn an_update_is_claimed_before_it_is_handled() {
        let f = Fixture::new(vec![Ok(vec![says(11, "hello")])]);
        let handled = HandledUpdates::new(KvStore::new(Arc::clone(&f.conn)));
        assert!(!handled.contains(11).unwrap());

        f.build().poll_once().await.unwrap();

        assert!(handled.contains(11).unwrap());
        assert!(
            handled.contains(10).unwrap(),
            "the mark is a high-water mark: everything below it is spent too"
        );
        assert!(!handled.contains(12).unwrap());
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
            OffsetStore::new(KvStore::new(Arc::clone(&conn))),
            HandledUpdates::new(KvStore::new(conn)),
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
        conversations: ea_core::store::conversations::ConversationStore,
    }

    /// A session runner that answers without spawning anything. No test in the
    /// default suite may start a real `claude`.
    struct CannedSessions;

    impl crate::triage::SessionBoundary for CannedSessions {
        fn run_session(
            &self,
            _req: crate::session::SessionRequest,
        ) -> crate::triage::BoxedSession<'_> {
            Box::pin(async {
                Ok(crate::session::SessionOutcome {
                    text: "the tenta is on the 14th".to_string(),
                    structured: None,
                    session_id: Some("sess-1".to_string()),
                    cost_usd: Some(0.001),
                })
            })
        }
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
        let conversations =
            ea_core::store::conversations::ConversationStore::new(Arc::clone(&conn));
        let chat = Arc::new(crate::chat::ChatService::new(
            conversations.clone(),
            ea_core::store::facts::FactStore::new(Arc::clone(&conn)),
            Some(Arc::new(CannedSessions) as Arc<dyn crate::triage::SessionBoundary>),
            crate::budget::Budget::new(
                RunStore::new(Arc::clone(&conn)),
                60,
                crate::notify::policy::DEFAULT_TIME_ZONE,
            ),
            crate::config::DEFAULT_CHAT_MODEL,
            crate::notify::policy::DEFAULT_TIME_ZONE,
        ));
        let notifier = Arc::new(
            Notifier::new(
                RecordingTransport::default(),
                ActionStore::new(Arc::clone(&conn)),
                executor,
                OWNER,
            )
            .with_chat(chat as Arc<dyn crate::chat::ChatResponder>),
        );
        Harness {
            _dir: dir,
            actions: ActionStore::new(Arc::clone(&conn)),
            calls,
            notifier,
            conn,
            conversations,
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
                HandledUpdates::new(KvStore::new(Arc::clone(&self.conn))),
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
            message: None,
            callback_query: Some(CallbackQuery {
                id: format!("cb-{update_id}"),
                from: User {
                    id: from.0,
                    is_bot: false,
                },
                message: Some(Message {
                    chat: Some(Chat { id: CHAT }),
                    from: None,
                    text: None,
                }),
                data: Some(data.to_string()),
            }),
        }
    }

    /// A free-text message as Telegram delivers it: the sender in
    /// `message.from`, never in `message.chat`.
    fn writes(update_id: i64, from: TelegramUserId, text: &str) -> Update {
        Update {
            update_id,
            callback_query: None,
            message: Some(Message {
                chat: Some(Chat { id: CHAT }),
                from: Some(User {
                    id: from.0,
                    is_bot: false,
                }),
                text: Some(text.to_string()),
            }),
        }
    }

    /// End to end over the real notifier and a real conversation store: a
    /// message on the phone becomes two rows in the thread and an answer sent
    /// back to the chat it came from.
    #[tokio::test]
    async fn the_owners_message_becomes_a_turn_in_the_shared_conversation() {
        let h = harness();
        let (source, lp) = h.drive(vec![writes(1, OWNER, "when is the tenta?")]);

        lp.poll_once().await.unwrap();

        assert_eq!(
            source.sent(),
            vec![(CHAT, "the tenta is on the 14th".to_string())]
        );
        let id = h.conversations.current().unwrap();
        let messages = h.conversations.recent(id, 10).unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].surface, "telegram");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].body, "the tenta is on the 14th");
        assert!(
            h.calls.lock().unwrap().is_empty(),
            "a chat reaches no connector"
        );
    }

    /// The whole point of the owner check, end to end: a stranger's message
    /// must not start a session, must not touch the thread, and must get the
    /// same sentence a stranger's button press gets.
    #[tokio::test]
    async fn a_strangers_message_starts_no_session_and_leaves_no_trace() {
        let h = harness();
        let (source, lp) = h.drive(vec![writes(1, STRANGER, "when is the tenta?")]);

        lp.poll_once().await.unwrap();

        assert_eq!(
            source.sent(),
            vec![(
                CHAT,
                crate::notify::telegram::UNRECOGNISED_REPLY.to_string()
            )]
        );
        let id = h.conversations.current().unwrap();
        assert!(
            h.conversations.recent(id, 10).unwrap().is_empty(),
            "a stranger must not write to the owner's conversation"
        );
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

    /// A redelivered update is now stopped by the claim, before the handler
    /// and before the store — one `update_id`, one effect, whatever Telegram
    /// sends. The store's `proposed -> approved` transition is still the
    /// second line of defence for the genuinely double *tap* (two presses,
    /// two `update_id`s); see
    /// `telegram::tests::a_double_tapped_approve_executes_exactly_once`.
    #[tokio::test]
    async fn a_redelivered_tap_does_not_execute_twice() {
        let h = harness();
        let id = h.propose();
        let data = format!("approve:{id}");
        let (source, lp) = h.drive(vec![press(1, OWNER, &data), press(1, OWNER, &data)]);

        let summary = lp.poll_once().await.unwrap();

        assert_eq!(
            h.calls.lock().unwrap().len(),
            1,
            "the second delivery must not produce a second voucher"
        );
        assert_eq!(summary.duplicates, 1, "and it is counted as what it was");
        assert_eq!(
            source.answers().len(),
            1,
            "the copy is dropped before the handler, so there is nothing new \
             to answer: {:?}",
            source.answers()
        );
        assert_eq!(h.status(id), "executed");
    }
}
