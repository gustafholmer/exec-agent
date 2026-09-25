//! Telegram: the daemon's one channel to the human, and the human's one way
//! back.
//!
//! Two things happen here and they are deliberately separated.
//!
//! * **The decision logic** — [`Notifier`] — turns an action id into a message
//!   with two buttons, and turns a button press back into a store transition,
//!   possibly an execution, and a sentence a person can read. It never touches
//!   the network: it talks to a [`Transport`].
//! * **The wire** — [`TelegramTransport`] — is the only thing in this file that
//!   opens a socket, and it is the only thing that holds the bot token.
//!
//! That split is the point. Everything worth getting wrong is in the first
//! part, and the first part is testable against a fake with no network.
//!
//! # Why a direct Bot API call and not `teloxide`
//!
//! The workspace originally pinned `teloxide = "0.13"`. That version exists,
//! but it is four minor releases stale (0.17.0 is current), and 0.17 does not
//! build on this toolchain at all: it pulls `takecell 0.1.2`, which requires
//! rustc 1.96, and ours is 1.93. Making it build needs a `--precise` downgrade
//! of a transitive dependency recorded in `Cargo.lock` — a pin that the next
//! `cargo update` quietly undoes.
//!
//! The weight is the other half of it. `teloxide` with default features locks
//! 210 packages and brings `native-tls`/`openssl-sys` with it. This workspace
//! deliberately uses `reqwest` with `default-features = false` and `rustls-tls`
//! so that a daemon holding OAuth tokens for accounting and mail links no C
//! TLS stack; Cargo's feature unification would have applied `teloxide`'s
//! choice to our `reqwest` too.
//!
//! And none of what `teloxide` is actually *for* is used here. Its value is the
//! dispatcher, `dptree`, the update listener and the command macros. This
//! module sends one message with two inline buttons and is handed callback data
//! as a `&str` by whatever owns the update loop. That is one HTTP POST, and it
//! is written below in about forty lines against `reqwest`, which is already a
//! workspace dependency.
//!
//! # Exactly-once, when the human taps twice
//!
//! A Telegram inline button is trivially double-tappable, and the actions
//! behind it post real vouchers. Two concurrent `approve:<id>` callbacks must
//! produce exactly one connector call.
//!
//! This module adds **no** new lock for that, on purpose. Two layers already
//! stand behind it, and a third in front would only be a fourth thing to get
//! out of step:
//!
//! 1. [`ActionStore::approve`] is a single conditional `UPDATE ... WHERE
//!    status = 'proposed'`. Exactly one of two concurrent callbacks changes a
//!    row; the other gets an error. That is the claim, and it survives a
//!    restart between the two taps in a way an in-memory set cannot.
//! 2. [`Executor`] then takes its own in-flight claim and, immediately before
//!    the connector call, the durable [`ActionStore::claim_for_execution`].
//!
//! What this module owes is the *reply*. The loser of the race must not be
//! shown a raw `anyhow` string — "action 7 is approved, not proposed; cannot
//! move to approved" is a sentence for a log, not for a person holding a phone.
//! So a failed transition is never reported as a failure directly: the row is
//! read back and the reply describes the state it is actually in. See
//! [`Notifier::already_handled`].
//!
//! # Who may press the buttons, and who may talk
//!
//! Exactly one person: the [`TelegramUserId`] handed to [`Notifier::new`],
//! which is required rather than optional so that there is no "unconfigured,
//! allow everything" state to fall into. A callback from any other id is
//! refused before its payload is even parsed, and the refusal is
//! indistinguishable from the reply to a button the daemon does not recognise.
//! See [`Notifier::handle_callback`] for what a bot token does and does not
//! let an attacker forge — required reading for whoever writes the update
//! loop.
//!
//! [`Notifier::handle_message`] — free text rather than a button — is held to
//! the identical standard, and deliberately shares
//! [`UNRECOGNISED_REPLY`] with the callback path. The identity in a message
//! update is **`message.from.id`**, which is a different field from
//! `message.chat.id`: the chat is a place, the `from` is a person, and in a
//! group they are not even the same number. A message routed to the chat
//! surface without that check would let anyone who found the bot hold a
//! conversation with the owner's assistant — and that assistant can propose
//! actions.
//!
//! # The token, and data arriving from the network
//!
//! The bot token is a bearer credential for the entire bot: anyone holding it
//! can read every message sent to it and send as it. It is read from
//! `~/.config/exec-agent/telegram.token`, which must be mode `0600`; it is
//! never in the repo, never logged, and never in an error message. The last one
//! is not free — the token sits in the Bot API *URL path*, and `reqwest`'s
//! errors carry the URL, so every error path here goes through
//! [`reqwest::Error::without_url`].
//!
//! Callback data arrives from Telegram, which means it arrives from the
//! network. [`parse_callback`] contains no `unwrap`, no slicing, and no
//! `parse` whose failure is not handled; anything unrecognised is a no-op with
//! a polite reply.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use ea_core::store::actions::{ActionStatus, ActionStore};

use crate::chat::{ChatResponder, ChatTurn, SURFACE_TELEGRAM};
use crate::executor::{Executor, ToolCaller};
use crate::notify::updates::{Update, UpdateSource};

/// Callback-data verb for the approve button.
pub const APPROVE: &str = "approve";

/// Callback-data verb for the reject button.
pub const REJECT: &str = "reject";

/// Telegram rejects a `sendMessage` whose text exceeds 4096 UTF-16 code units.
/// Budgeted conservatively in bytes, which is never an over-estimate.
const MAX_MESSAGE_BYTES: usize = 3800;

/// The filename under the config directory holding the bot token.
pub const TOKEN_FILE: &str = "telegram.token";

/// The filename under the config directory holding the owner's chat id.
pub const CHAT_ID_FILE: &str = "telegram.chat_id";

/// The filename under the config directory holding the Telegram *user* id of
/// the one person allowed to press the buttons.
///
/// Deliberately a separate file from [`CHAT_ID_FILE`], and not a reuse of it.
/// A chat id and a user id are equal only in a 1:1 DM with the bot; in a group
/// the chat id is the group's and belongs to no user at all, so reusing it
/// would refuse the owner's own taps — a failure that reads as a bug rather
/// than as a policy. A user id is not a credential (anyone the owner has
/// messaged can see it), so this file needs no special mode; it lives beside
/// the token because it is part of the same configuration.
pub const OWNER_ID_FILE: &str = "telegram.owner_id";

/// The single reply for every callback this daemon declines to act on.
///
/// Three situations share it — a malformed payload, an unknown verb, and a
/// callback from someone who is not the owner — and that sharing is the point.
/// If a stranger's reply differed from the malformed-data reply, anyone who
/// found the bot could walk the action ids by diffing responses and learn
/// which vouchers are pending. One constant, so the two texts cannot drift
/// apart in a later edit.
pub const UNRECOGNISED_REPLY: &str = "That button is not one I recognise. Nothing was done.";

/// How long one Bot API call may take before it is abandoned.
///
/// `reqwest::Client::new()` has **no** timeout, so a stalled connection to a
/// black-holed `api.telegram.org` blocks its caller until the process dies —
/// and the caller is a scheduler job.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The public Bot API.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Somewhere a message with buttons can be sent.
///
/// `buttons` is `(label, callback_data)` in display order; an empty slice is a
/// plain message. The lifetime-free `-> impl Future + Send` spelling rather
/// than `async fn` is deliberate: a bare `async fn` in a public trait trips
/// `async_fn_in_trait`, which this workspace builds with `-D warnings`, and the
/// explicit `Send` bound is what lets a `Notifier` be driven from a spawned
/// task. Same shape as [`crate::executor::ToolCaller`].
pub trait Transport {
    fn send(
        &self,
        text: &str,
        buttons: &[(String, String)],
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// A Telegram **user** id: `callback_query.from.id` for a button press,
/// `message.from.id` for a free-text message — the person, either way.
///
/// A newtype rather than a bare `i64` because the wrong `i64` is right there
/// in the same update. `message.chat.id` would compile just as happily in the
/// place of `from.id` and authenticate nothing, and in a group the two are not
/// even the same number. Construct it only where a `from.id` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TelegramUserId(pub i64);

impl fmt::Display for TelegramUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Turns actions into messages and button presses back into decisions.
pub struct Notifier<T: Transport, C: ToolCaller> {
    transport: T,
    actions: ActionStore,
    executor: Arc<Executor<C>>,
    /// The only user whose button presses and messages are acted on. Not an
    /// `Option`: an "unconfigured" variant is a branch that allows everyone,
    /// and this is the gate in front of posting real vouchers.
    owner: TelegramUserId,
    /// Where a free-text message goes. `None` on a daemon built without a
    /// chat service, which answers messages by saying so rather than by
    /// silently ignoring them.
    chat: Option<Arc<dyn ChatResponder>>,
}

/// What the owner is told when a message arrives at a daemon with no chat
/// service wired to it. Never shown to anyone else — a non-owner gets
/// [`UNRECOGNISED_REPLY`] and learns nothing.
pub const NO_CHAT_REPLY: &str =
    "I cannot hold a conversation right now: this daemon has no chat service.";

/// The reply to a message from the owner that carries no text to answer — a
/// sticker, a photo, a location.
pub const NO_TEXT_REPLY: &str = "I can only read text messages.";

/// One finished turn as the single message the phone gets.
///
/// A [`ChatTurn`] can carry both an answer and a note, and the note is the
/// only place the owner is told that the day's session budget is spent — that
/// triage has stopped scoring and the next briefing will not be written. Taking
/// one field and discarding the other (`reply.or(note)`) silently drops that,
/// on the surface the owner is most likely to be reading from and least likely
/// to run `ea status` from. So both are shown: the answer first, then the note
/// under it, separated by a blank line and marked with a dash so it reads as
/// the daemon's aside rather than part of the answer.
///
/// # Why the note's length is reserved rather than trusted to survive
///
/// Every outgoing message is cut to [`MAX_MESSAGE_BYTES`] by the transport,
/// and the note is at the *end*. Cutting the concatenation therefore takes
/// the note first: a model that answers at length on an over-budget day would
/// push off exactly the sentence that day exists to deliver, and the owner
/// crosses their spending ceiling without being told. So the reply is cut to
/// what is left after the note — and after the ellipsis [`truncate`] adds,
/// which is why the note is not simply subtracted — leaving the transport's
/// own cut nothing to do.
///
/// A note longer than the whole budget would leave the reply nothing at all;
/// that is the right way round. The notes are this daemon's own short
/// sentences, and the note is the part that must not be lost.
fn rendered_turn(turn: ChatTurn) -> String {
    match (turn.reply, turn.note) {
        (Some(reply), Some(note)) => {
            let tail = format!("\n\n— {note}");
            let room = MAX_MESSAGE_BYTES.saturating_sub(tail.len() + ELLIPSIS.len());
            format!("{}{tail}", truncate(&reply, room))
        }
        (Some(reply), None) => reply,
        (None, Some(note)) => note,
        (None, None) => "I had nothing to say.".to_string(),
    }
}

/// A parsed button press. Constructed only by [`parse_callback`], so an
/// unparseable payload cannot reach the store at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Callback {
    Approve(i64),
    Reject(i64),
}

/// Parse `approve:<id>` / `reject:<id>`, or `None`.
///
/// Everything here is total. `split_once` returns an `Option` rather than
/// indexing; the id goes through `i64::from_str`, whose overflow and
/// non-numeric cases are `Err` and not a panic; and an id is required to be
/// positive because SQLite rowids are, so `0` and negatives are refused before
/// they reach a query. Anything else — an unknown verb, no colon, an empty
/// payload, a second colon — is `None`, which the caller answers politely.
fn parse_callback(data: &str) -> Option<Callback> {
    let (verb, rest) = data.split_once(':')?;
    let id: i64 = rest.parse().ok()?;
    if id <= 0 {
        return None;
    }
    match verb {
        APPROVE => Some(Callback::Approve(id)),
        REJECT => Some(Callback::Reject(id)),
        _ => None,
    }
}

/// What [`truncate`] marks a cut with. Its own length is part of the budget
/// anything reserving room ahead of a cut has to allow for.
const ELLIPSIS: &str = "…";

/// Cut `s` to at most `max` bytes without splitting a character, plus
/// [`ELLIPSIS`] when anything was cut.
///
/// `&s[..max]` panics when `max` lands inside a multi-byte character, and a
/// preview containing a name with an accent in the wrong column is exactly how
/// that ships.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| *i <= max)
        .last()
        .unwrap_or(0);
    format!("{}{ELLIPSIS}", &s[..end])
}

impl<T: Transport, C: ToolCaller> Notifier<T, C> {
    /// `owner` is the one Telegram user whose taps count. It is required, by
    /// design: there is no constructor that leaves the button unguarded.
    pub fn new(
        transport: T,
        actions: ActionStore,
        executor: Arc<Executor<C>>,
        owner: TelegramUserId,
    ) -> Self {
        Self {
            transport,
            actions,
            executor,
            owner,
            chat: None,
        }
    }

    /// Route the owner's free-text messages to the shared conversation.
    ///
    /// Separate from [`Notifier::new`] because the chat service is optional
    /// and because the owner check must not depend on it: a daemon with no
    /// chat service still refuses a stranger with exactly the same sentence.
    pub fn with_chat(mut self, chat: Arc<dyn ChatResponder>) -> Self {
        self.chat = Some(chat);
        self
    }

    /// Send a plain message with no buttons.
    pub async fn notify(&self, text: &str) -> anyhow::Result<()> {
        self.transport
            .send(&truncate(text, MAX_MESSAGE_BYTES), &[])
            .await
    }

    /// Put a proposed action in front of the human with Approve and Reject.
    ///
    /// Refuses anything not currently `proposed`: buttons on an action that has
    /// already been decided are an invitation to a confusing second decision,
    /// and the only honest thing to show is nothing.
    pub async fn push_action(&self, id: i64) -> anyhow::Result<()> {
        let action = self
            .actions
            .get(id)?
            .ok_or_else(|| anyhow!("action {id} does not exist"))?;
        if action.status != ActionStatus::Proposed {
            bail!(
                "action {id} is {}, not proposed; it has already been decided",
                action.status.as_str()
            );
        }

        let text = truncate(
            &format!(
                "Approve this action?\n\n#{id} · {}.{}\n\n{}\n\nWhy: {}\n\nExpires: {}",
                action.connector, action.tool, action.preview, action.rationale, action.expires_at,
            ),
            MAX_MESSAGE_BYTES,
        );
        let buttons = vec![
            ("Approve".to_string(), format!("{APPROVE}:{id}")),
            ("Reject".to_string(), format!("{REJECT}:{id}")),
        ];

        self.transport.send(&text, &buttons).await
    }

    /// Handle one button press and return what to show the human.
    ///
    /// `from` is `callback_query.from.id` exactly as Telegram delivered it.
    /// Anything but the configured owner is refused *before* the payload is
    /// parsed and before the store is touched, and is answered with
    /// [`UNRECOGNISED_REPLY`] — the same sentence a malformed payload gets, so
    /// a stranger cannot learn that action #7 exists by diffing replies.
    ///
    /// Infallible by design: this answers a callback query, and every outcome —
    /// including a malformed payload or a store fault — has to come back as
    /// something displayable. Failures are logged with their detail and
    /// reported to the human in general terms.
    ///
    /// # How much `from` is worth — read this before writing an update loop
    ///
    /// `from.id` is stamped by Telegram's servers, not by the sender, and
    /// **the bot token does not let a third party forge it**. A stolen token
    /// is bad in other ways: the holder can read everything sent to the bot,
    /// send messages as the bot, and steal the update stream outright
    /// (`deleteWebhook`, then `getUpdates`). What it does not buy is approval
    /// injection, because the thief does not author the `from` field — Telegram
    /// does.
    ///
    /// **That reasoning holds for long polling only. A webhook is unsigned.**
    /// Telegram delivers webhook updates as a plain HTTP POST of JSON, and so
    /// can anyone else who can reach the URL: the entire body is then
    /// attacker-authored, `from.id` included, and this check is worth nothing.
    /// An update loop built on a webhook **must** set a secret via
    /// `setWebhook`'s `secret_token` and compare the
    /// `X-Telegram-Bot-Api-Secret-Token` header on every request before the
    /// body is believed. `from.id` alone is not sufficient there.
    ///
    /// The loop should also check `callback_query.message.chat.id` against the
    /// configured [`TelegramConfig::chat_id`]. That field is not available at
    /// this seam — this function is handed only the sender and the payload —
    /// so it is the loop's to enforce.
    pub async fn handle_callback(&self, from: TelegramUserId, data: &str) -> String {
        if from != self.owner {
            // `warn`, not `debug`: a tap from anyone else is either a
            // misconfigured owner id or a stranger who found the bot, and both
            // are worth seeing in a log. The sender id is recorded; the reply
            // reveals nothing at all.
            tracing::warn!(
                from = from.0,
                "refusing a telegram callback from a non-owner"
            );
            return UNRECOGNISED_REPLY.to_string();
        }

        match parse_callback(data) {
            Some(Callback::Approve(id)) => self.approve(id).await,
            Some(Callback::Reject(id)) => self.reject(id),
            None => {
                // Not an error: old buttons from a previous build, or someone
                // poking the bot. Deliberately does not echo `data` back.
                tracing::debug!("ignoring unrecognised telegram callback data");
                UNRECOGNISED_REPLY.to_string()
            }
        }
    }

    /// Handle one free-text message and return what to say back.
    ///
    /// `from` is **`message.from.id`** — the sender — and never
    /// `message.chat.id`, which is the place the message was sent in. The
    /// update loop checks the chat separately, as a place; this checks the
    /// person. Both refusals are [`UNRECOGNISED_REPLY`], byte-identical to the
    /// one a malformed callback gets, so nobody can learn anything by diffing
    /// replies — not that the bot has an owner, not that a given id is the
    /// owner, and not that a conversation exists at all.
    ///
    /// Infallible for the same reason as [`Notifier::handle_callback`]: every
    /// outcome has to be something displayable, including a session that
    /// failed.
    pub async fn handle_message(&self, from: TelegramUserId, text: &str) -> String {
        if from != self.owner {
            tracing::warn!(
                from = from.0,
                "refusing a telegram message from a non-owner"
            );
            return UNRECOGNISED_REPLY.to_string();
        }

        let text = text.trim();
        if text.is_empty() {
            return NO_TEXT_REPLY.to_string();
        }

        let Some(chat) = self.chat.as_ref() else {
            return NO_CHAT_REPLY.to_string();
        };

        match chat.respond(SURFACE_TELEGRAM, text).await {
            Ok(turn) => rendered_turn(turn),
            Err(err) => {
                // The detail goes to the log; the phone gets a sentence. A
                // failed turn stored no reply, so saying "it failed" is the
                // truth about the transcript as well as about the session.
                tracing::warn!(error = %format!("{err:#}"), "a telegram chat turn failed");
                "Something went wrong answering that. It was not recorded as an answer — \
                 try again, or check `ea log`."
                    .to_string()
            }
        }
    }

    /// Approve and execute.
    ///
    /// The `proposed -> approved` transition *is* the claim: one conditional
    /// UPDATE, so of two concurrent taps exactly one proceeds to the executor.
    /// No extra lock here — see the module docs.
    async fn approve(&self, id: i64) -> String {
        if let Err(err) = self.actions.approve(id) {
            // Almost always the losing half of a double tap, occasionally an
            // expired proposal. Either way the row itself is the authority on
            // what to say, so read it rather than surfacing this error.
            tracing::debug!(action = id, error = %err, "approve transition did not apply");
            return self.already_handled(id);
        }

        match self.executor.execute_approved(id).await {
            Ok(action) => match action.status {
                ActionStatus::Executed => format!("Executed #{id}."),
                ActionStatus::Failed => format!(
                    "#{id} was approved, but the connector refused it. Nothing changed on their side."
                ),
                other => format!("#{id} is now {}.", other.as_str()),
            },
            Err(err) => {
                tracing::warn!(action = id, error = %err, "approved action failed to execute");
                Self::could_not_run(id)
            }
        }
    }

    /// Reject. Nothing is executed and the connector is never reached — there
    /// is no call to the executor on this path at all.
    fn reject(&self, id: i64) -> String {
        match self.actions.reject(id, "rejected from Telegram") {
            Ok(_) => format!("Rejected #{id}. Nothing was called."),
            Err(err) => {
                tracing::debug!(action = id, error = %err, "reject transition did not apply");
                self.already_handled(id)
            }
        }
    }

    /// What the owner is told when an approved action could not be run at all.
    ///
    /// It names commands that exist. This used to end "Check `ea actions`",
    /// which is not a subcommand: a person following that instruction — at the
    /// moment they have just been told something failed — gets a clap usage
    /// error instead of an answer. The two that answer the question are
    /// `ea queue` (what is still waiting) and `ea log` (what happened).
    fn could_not_run(id: i64) -> String {
        format!(
            "#{id} was approved but could not be run. \
             `ea queue` for what is still waiting, `ea log` for what happened."
        )
    }

    /// The sentence shown when a transition did not apply.
    ///
    /// This is the double-tap loser's reply, and the reason it reads the row
    /// back instead of formatting the transition error: a person holding a
    /// phone needs "already handled", not a status-machine diagnostic.
    fn already_handled(&self, id: i64) -> String {
        match self.actions.get(id) {
            Ok(Some(action)) => match action.status {
                // The transition failed but the row still looks decidable.
                // Rare — a store fault rather than a race.
                ActionStatus::Proposed => {
                    format!("#{id} could not be updated just now. Try again.")
                }
                ActionStatus::Approved => {
                    format!("#{id} was already approved — it is being handled now.")
                }
                ActionStatus::Executed => format!("#{id} was already handled: it ran."),
                ActionStatus::Rejected => format!("#{id} was already handled: rejected."),
                ActionStatus::Failed => {
                    format!("#{id} was already handled: it ran and failed.")
                }
                ActionStatus::Expired => {
                    format!("#{id} expired before anyone decided. Nothing was done.")
                }
            },
            Ok(None) => format!("#{id} is not an action I know about."),
            Err(err) => {
                tracing::warn!(action = id, error = %err, "could not read action back");
                format!("#{id} could not be looked up just now.")
            }
        }
    }
}

/// Where the bot's credentials live, once read.
///
/// Deliberately has no `Debug` derive and no `Display`: the whole point of the
/// type is that the token inside it never reaches an output stream by accident.
pub struct TelegramConfig {
    token: String,
    /// Where messages are sent. In a group this is the group's id and is not
    /// any user's id — which is why it is not also the allow-list.
    pub chat_id: i64,
    /// Who may press the buttons. Read from its own file; see
    /// [`OWNER_ID_FILE`].
    pub owner_id: TelegramUserId,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("owner_id", &self.owner_id)
            .finish()
    }
}

/// Refuse `path` unless it is readable by its owner alone.
///
/// The mode check is not decoration. A bot token in a world-readable file in a
/// shared home directory is a credential handed to every process on the box,
/// and the failure is silent. Note what the error message contains: the path,
/// never the contents. Public because `config.toml` is held to the same
/// standard once it carries a token.
pub fn require_owner_only(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is mode {mode:04o}; it holds a credential and must be 0600 \
             (run: chmod 600 {})",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

/// Read a secret file, insisting it is not readable by anyone else.
///
/// The mode check is not decoration. A bot token in a world-readable file in a
/// shared home directory is a credential handed to every process on the box,
/// and the failure is silent. Note what the error messages contain: the path,
/// never the contents.
pub fn read_secret(path: &Path) -> anyhow::Result<String> {
    require_owner_only(path)?;

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value = raw.trim().to_string();
    if value.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(value)
}

/// The token goes into the Bot API *URL path*. A stray `/` or `?` in it would
/// not be a bad credential, it would be a request to a different endpoint, so
/// the charset is checked rather than trusted. The error says where the token
/// came from, and never quotes it.
fn check_token_charset(token: &str, source: &str) -> anyhow::Result<()> {
    if token.is_empty() {
        bail!("{source} holds an empty Telegram bot token");
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
    {
        bail!("{source} does not look like a Telegram bot token (unexpected characters)");
    }
    Ok(())
}

impl TelegramConfig {
    /// Load from `~/.config/exec-agent` (or `$EA_CONFIG_DIR`).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&ea_core::paths::config_dir())
    }

    /// Build from values that came from somewhere other than the three files —
    /// today, the `[telegram]` block of `config.toml`. The token still goes
    /// through the same charset check.
    pub fn from_parts(token: String, chat_id: i64, owner_id: i64) -> anyhow::Result<Self> {
        let token = token.trim().to_string();
        check_token_charset(&token, "the configured Telegram bot token")?;
        Ok(Self {
            token,
            chat_id,
            owner_id: TelegramUserId(owner_id),
        })
    }

    /// Load from an explicit directory. Used by tests, and by anyone running
    /// more than one bot out of one checkout.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        let token_path: PathBuf = dir.join(TOKEN_FILE);
        let token = read_secret(&token_path)?;

        check_token_charset(&token, &token_path.display().to_string())?;

        let chat_path = dir.join(CHAT_ID_FILE);
        let chat_raw = std::fs::read_to_string(&chat_path)
            .with_context(|| format!("reading {}", chat_path.display()))?;
        let chat_id: i64 = chat_raw
            .trim()
            .parse()
            .with_context(|| format!("{} must contain a numeric chat id", chat_path.display()))?;

        // The owner's *user* id, from its own file. Not a credential, so no
        // mode check — but required, because the alternative to knowing who
        // the owner is, is letting anyone approve a voucher.
        let owner_path = dir.join(OWNER_ID_FILE);
        let owner_raw = std::fs::read_to_string(&owner_path)
            .with_context(|| format!("reading {}", owner_path.display()))?;
        let owner_id: i64 = owner_raw.trim().parse().with_context(|| {
            format!(
                "{} must contain the numeric Telegram user id of the one person \
                 allowed to approve actions (this is a user id, not the chat id)",
                owner_path.display()
            )
        })?;

        Ok(Self {
            token,
            chat_id,
            owner_id: TelegramUserId(owner_id),
        })
    }
}

/// The wire. The only thing in this module that opens a socket, and the only
/// thing that holds the token.
pub struct TelegramTransport {
    http: reqwest::Client,
    base: String,
    token: String,
    chat_id: i64,
}

impl fmt::Debug for TelegramTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramTransport")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .finish()
    }
}

impl TelegramTransport {
    /// Fallible, and not because of the network: `reqwest::Client::new()`
    /// *panics* if TLS initialisation fails, and this constructor runs at
    /// daemon start. A daemon that cannot build an HTTPS client should say so
    /// and exit, not abort inside a library call.
    pub fn new(token: impl Into<String>, chat_id: i64) -> anyhow::Result<Self> {
        Self::with_base_url(DEFAULT_API_BASE, token, chat_id)
    }

    pub fn from_config(config: &TelegramConfig) -> anyhow::Result<Self> {
        Self::new(config.token.clone(), config.chat_id)
    }

    /// Point at a different Bot API origin. Telegram publishes a self-hostable
    /// Bot API server, and this is also how the transport is exercised against
    /// a local stub without ever reaching Telegram.
    pub fn with_base_url(
        base: impl Into<String>,
        token: impl Into<String>,
        chat_id: i64,
    ) -> anyhow::Result<Self> {
        Self::build(base, token, chat_id, HTTP_TIMEOUT)
    }

    /// The one place the client is constructed. Split out so the timeout is a
    /// parameter a test can shorten instead of a 30-second wait.
    fn build(
        base: impl Into<String>,
        token: impl Into<String>,
        chat_id: i64,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            // Not optional; see `ea_core::http` for the 403 that proved it.
            .user_agent(ea_core::http::USER_AGENT)
            .timeout(timeout)
            .build()
            .context("building the HTTPS client for the Telegram Bot API")?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            chat_id,
        })
    }
}

/// The envelope every Bot API method answers with.
///
/// Only the two fields that decide success are modelled. `ok` is the one that
/// matters: Telegram reports application-level failures — "chat not found",
/// "bot was blocked by the user" — with **HTTP 200** and `ok: false`, so a
/// status-code-only check calls those a delivered message.
#[derive(serde::Deserialize)]
struct ApiReply {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
}

impl Transport for TelegramTransport {
    fn send(
        &self,
        text: &str,
        buttons: &[(String, String)],
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        let url = format!("{}/bot{}/sendMessage", self.base, self.token);

        let mut body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": truncate(text, MAX_MESSAGE_BYTES),
        });
        if !buttons.is_empty() {
            let row: Vec<_> = buttons
                .iter()
                .map(|(label, data)| serde_json::json!({ "text": label, "callback_data": data }))
                .collect();
            body["reply_markup"] = serde_json::json!({ "inline_keyboard": [row] });
        }

        let request = self.http.post(url).json(&body);

        async move {
            // `without_url` on every error path: the URL is
            // `https://api.telegram.org/bot<TOKEN>/sendMessage`, and reqwest
            // puts it in `Display`. Without this the token lands in the first
            // log line written after the network hiccups.
            let response = request
                .send()
                .await
                .map_err(|err| err.without_url())
                .context("sending a Telegram message")?;

            let status = response.status();

            // The body is read on every path, success included: a 200 is not
            // proof of delivery. Telegram's error bodies do not echo the token,
            // but they are truncated anyway rather than pasted whole into a log.
            let body = response
                .text()
                .await
                .map_err(|err| err.without_url())
                .context("reading Telegram's reply")?;
            let envelope: Option<ApiReply> = serde_json::from_str(&body).ok();
            let detail = envelope
                .as_ref()
                .and_then(|reply| reply.description.as_deref())
                .map_or_else(|| truncate(&body, 300), |d| truncate(d, 300));

            if !status.is_success() {
                bail!("Telegram refused the message: HTTP {status}: {detail}");
            }
            match envelope {
                Some(reply) if reply.ok => Ok(()),
                // HTTP 200, `ok: false`. The message was not delivered.
                Some(_) => bail!("Telegram rejected the message: {detail}"),
                None => bail!(
                    "Telegram's reply was not the JSON envelope the Bot API \
                     documents: {detail}"
                ),
            }
        }
    }
}

/// The long-poll end of the Bot API, for [`crate::notify::updates::UpdateLoop`].
///
/// Lives here rather than in `updates` because the token is a private field of
/// this struct and must stay one: the update loop never sees it, and cannot
/// accidentally log it.
impl UpdateSource for TelegramTransport {
    fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> impl Future<Output = anyhow::Result<Vec<Update>>> + Send {
        let url = format!("{}/bot{}/getUpdates", self.base, self.token);
        let mut body = serde_json::json!({
            "timeout": timeout_secs,
            // Exactly the two kinds this daemon acts on: a button press and a
            // message. Nothing else is asked for, so Telegram's reply cannot
            // carry edited messages, channel posts or reactions — and the
            // update loop cannot start acting on a kind nobody reviewed.
            "allowed_updates": ["callback_query", "message"],
        });
        if let Some(offset) = offset {
            body["offset"] = serde_json::json!(offset);
        }

        // The client's own 30-second timeout is for a `sendMessage`; a long
        // poll is *designed* to hang for `timeout_secs`, so it gets its own
        // deadline with room for the round trip on top. Without this override
        // every idle poll would be reported as a network failure.
        let request = self
            .http
            .post(url)
            .json(&body)
            .timeout(Duration::from_secs(timeout_secs + 15));

        async move {
            let response = request
                .send()
                .await
                .map_err(|err| err.without_url())
                .context("polling Telegram for updates")?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| err.without_url())
                .context("reading Telegram's update batch")?;

            let envelope: UpdatesReply = match serde_json::from_str(&body) {
                Ok(envelope) => envelope,
                Err(err) => bail!(
                    "Telegram's getUpdates reply was not the documented envelope \
                     (HTTP {status}): {err}: {}",
                    truncate(&body, 300)
                ),
            };
            if !envelope.ok {
                bail!(
                    "Telegram refused getUpdates (HTTP {status}): {}",
                    truncate(
                        envelope.description.as_deref().unwrap_or("no reason given"),
                        300
                    )
                );
            }
            Ok(envelope.result)
        }
    }

    /// Reply into a specific chat.
    ///
    /// Takes `chat_id` rather than using [`Transport::send`]'s configured one
    /// because it answers a message that arrived somewhere, and the caller —
    /// which has already refused every chat but the configured one — is the
    /// authority on where that was.
    fn send_message(
        &self,
        chat_id: i64,
        text: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        let url = format!("{}/bot{}/sendMessage", self.base, self.token);
        let request = self.http.post(url).json(&serde_json::json!({
            "chat_id": chat_id,
            "text": truncate(text, MAX_MESSAGE_BYTES),
        }));

        async move {
            let response = request
                .send()
                .await
                .map_err(|err| err.without_url())
                .context("sending a Telegram reply")?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| err.without_url())
                .context("reading Telegram's reply")?;
            let envelope: Option<ApiReply> = serde_json::from_str(&body).ok();
            let detail = envelope
                .as_ref()
                .and_then(|reply| reply.description.as_deref())
                .map_or_else(|| truncate(&body, 300), |d| truncate(d, 300));
            match envelope {
                Some(reply) if reply.ok => Ok(()),
                Some(_) => bail!("Telegram rejected the reply (HTTP {status}): {detail}"),
                None => bail!(
                    "Telegram's reply was not the JSON envelope the Bot API \
                     documents (HTTP {status}): {detail}"
                ),
            }
        }
    }

    fn answer_callback(
        &self,
        callback_id: &str,
        text: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        let url = format!("{}/bot{}/answerCallbackQuery", self.base, self.token);
        let request = self.http.post(url).json(&serde_json::json!({
            "callback_query_id": callback_id,
            "text": text,
        }));

        async move {
            let response = request
                .send()
                .await
                .map_err(|err| err.without_url())
                .context("answering a Telegram callback query")?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| err.without_url())
                .context("reading Telegram's reply")?;
            let envelope: Option<ApiReply> = serde_json::from_str(&body).ok();
            match envelope {
                Some(reply) if reply.ok => Ok(()),
                other => bail!(
                    "Telegram rejected answerCallbackQuery (HTTP {status}): {}",
                    truncate(
                        other
                            .as_ref()
                            .and_then(|r| r.description.as_deref())
                            .unwrap_or(&body),
                        300
                    )
                ),
            }
        }
    }
}

/// `getUpdates`' envelope. Separate from [`ApiReply`] only because it carries
/// a `result` this daemon actually reads.
#[derive(serde::Deserialize)]
struct UpdatesReply {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    result: Vec<Update>,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use ea_core::policy::Policy;
    use ea_core::store::actions::ProposeInput;
    use ea_core::store::runs::RunStore;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    /// The one user allowed to press the buttons in these tests.
    const OWNER: TelegramUserId = TelegramUserId(10_000_001);

    /// Anyone else. A plausible neighbour of the owner's id, because an
    /// off-by-one in a comparison is exactly the bug this guards.
    const STRANGER: TelegramUserId = TelegramUserId(10_000_002);

    /// What a transport was asked to send: the body, and the buttons as
    /// `(label, callback_data)`.
    type Sent = (String, Vec<(String, String)>);

    #[derive(Clone, Default)]
    struct FakeTransport {
        sent: Arc<Mutex<Vec<Sent>>>,
    }

    impl Transport for FakeTransport {
        fn send(
            &self,
            text: &str,
            buttons: &[(String, String)],
        ) -> impl Future<Output = anyhow::Result<()>> + Send {
            let record = (text.to_string(), buttons.to_vec());
            let sent = self.sent.clone();
            async move {
                sent.lock().unwrap().push(record);
                Ok(())
            }
        }
    }

    /// Records every connector call. The assertion that matters throughout this
    /// module is on the *length* of this vector: "the connector was reached
    /// exactly once" is not provable from a status column alone.
    #[derive(Clone, Default)]
    struct FakeCaller {
        calls: Arc<Mutex<Vec<(String, String, Value)>>>,
        /// Held across the await so that two concurrent approvals genuinely
        /// overlap rather than completing one after the other in a single poll.
        delay: Duration,
    }

    impl ToolCaller for FakeCaller {
        fn call(
            &self,
            connector: &str,
            tool: &str,
            args: Value,
        ) -> impl Future<Output = anyhow::Result<String>> + Send {
            let calls = self.calls.clone();
            let delay = self.delay;
            let record = (connector.to_string(), tool.to_string(), args);
            async move {
                calls.lock().unwrap().push(record);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                Ok("done".to_string())
            }
        }
    }

    fn policy() -> Policy {
        Policy::parse(
            r#"
[fortnox]
record_voucher = "approve"
"#,
        )
        .unwrap()
    }

    struct Fixture {
        _dir: TempDir,
        notifier: Notifier<FakeTransport, FakeCaller>,
        sent: Arc<Mutex<Vec<Sent>>>,
        calls: Arc<Mutex<Vec<(String, String, Value)>>>,
        actions: ActionStore,
    }

    fn fixture_with_delay(delay: Duration) -> Fixture {
        let dir = TempDir::new().unwrap();
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));

        let transport = FakeTransport::default();
        let sent = transport.sent.clone();
        let caller = FakeCaller {
            delay,
            ..Default::default()
        };
        let calls = caller.calls.clone();

        let executor = Arc::new(Executor::new(
            ActionStore::new(conn.clone()),
            RunStore::new(conn.clone()),
            policy(),
            caller,
        ));

        Fixture {
            _dir: dir,
            notifier: Notifier::new(transport, ActionStore::new(conn.clone()), executor, OWNER),
            sent,
            calls,
            actions: ActionStore::new(conn),
        }
    }

    fn fixture() -> Fixture {
        fixture_with_delay(Duration::ZERO)
    }

    /// A chat service that records what it was asked and answers a canned
    /// line. No database, no session runner: what these tests are about is who
    /// is allowed to reach it at all.
    #[derive(Default)]
    struct FakeChat {
        seen: Mutex<Vec<(String, String)>>,
        reply: Option<String>,
        note: Option<String>,
        fail: bool,
    }

    impl FakeChat {
        fn answering(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: Some(reply.to_string()),
                ..Default::default()
            })
        }

        /// A turn that answered *and* has something the owner needs told —
        /// the over-budget turn.
        fn answering_with_note(reply: &str, note: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: Some(reply.to_string()),
                note: Some(note.to_string()),
                ..Default::default()
            })
        }

        fn noting(note: &str) -> Arc<Self> {
            Arc::new(Self {
                note: Some(note.to_string()),
                ..Default::default()
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                fail: true,
                ..Default::default()
            })
        }

        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl crate::chat::ChatResponder for FakeChat {
        fn respond<'a>(
            &'a self,
            surface: &'a str,
            message: &'a str,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = anyhow::Result<crate::chat::ChatTurn>> + Send + 'a>,
        > {
            self.seen
                .lock()
                .unwrap()
                .push((surface.to_string(), message.to_string()));
            Box::pin(async move {
                if self.fail {
                    anyhow::bail!("claude exited with status 1");
                }
                Ok(crate::chat::ChatTurn {
                    conversation_id: 1,
                    message_id: 1,
                    reply: self.reply.clone(),
                    note: self.note.clone(),
                })
            })
        }
    }

    /// A fixture whose notifier also answers free text.
    fn chat_fixture(chat: Arc<FakeChat>) -> (Fixture, Arc<FakeChat>) {
        let mut f = fixture();
        f.notifier = f
            .notifier
            .with_chat(Arc::clone(&chat) as Arc<dyn crate::chat::ChatResponder>);
        (f, chat)
    }

    fn propose(actions: &ActionStore) -> i64 {
        actions
            .propose(ProposeInput {
                connector: "fortnox".into(),
                tool: "record_voucher".into(),
                args: serde_json::json!({ "amount": 4200 }),
                preview: "Record voucher 4200 SEK to 5410 Consumables".into(),
                rationale: "the receipt arrived by mail".into(),
                ttl: chrono::Duration::hours(24),
            })
            .unwrap()
            .id
    }

    /// Every command this module tells the owner to run has to be a command
    /// that exists. `ea actions` never did; the two that answer the question
    /// do.
    #[test]
    fn the_failure_reply_names_commands_that_exist() {
        let text = Notifier::<FakeTransport, FakeCaller>::could_not_run(7);
        assert!(text.contains("#7"), "{text}");
        assert!(text.contains("`ea queue`"), "{text}");
        assert!(text.contains("`ea log`"), "{text}");
        assert!(
            !text.contains("ea actions"),
            "`ea actions` is not a subcommand: {text}"
        );
    }

    #[tokio::test]
    async fn pushing_an_action_carries_the_preview_and_both_callback_ids() {
        let f = fixture();
        let id = propose(&f.actions);

        f.notifier.push_action(id).await.unwrap();

        let sent = f.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "one message per proposal");
        let (text, buttons) = &sent[0];

        assert!(
            text.contains("Record voucher 4200 SEK to 5410 Consumables"),
            "the human decides on the preview, so it must be in the body: {text}"
        );
        assert!(text.contains("the receipt arrived by mail"), "{text}");

        assert_eq!(
            buttons,
            &vec![
                ("Approve".to_string(), format!("approve:{id}")),
                ("Reject".to_string(), format!("reject:{id}")),
            ],
            "callback data is the whole protocol between the button and the daemon"
        );
    }

    #[tokio::test]
    async fn an_approve_callback_approves_and_executes() {
        let f = fixture();
        let id = propose(&f.actions);

        let reply = f
            .notifier
            .handle_callback(OWNER, &format!("approve:{id}"))
            .await;

        assert!(reply.contains("Executed"), "{reply}");
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Executed
        );

        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fortnox");
        assert_eq!(calls[0].1, "record_voucher");
        assert_eq!(calls[0].2["amount"], 4200);
    }

    #[tokio::test]
    async fn a_reject_callback_rejects_and_never_reaches_the_connector() {
        let f = fixture();
        let id = propose(&f.actions);

        let reply = f
            .notifier
            .handle_callback(OWNER, &format!("reject:{id}"))
            .await;

        assert!(reply.contains("Rejected"), "{reply}");
        let action = f.actions.get(id).unwrap().unwrap();
        assert_eq!(action.status, ActionStatus::Rejected);
        assert!(action.reason.is_some(), "a rejection must record why");
        assert!(
            f.calls.lock().unwrap().is_empty(),
            "a rejected action must make zero connector calls"
        );
    }

    #[tokio::test]
    async fn an_unknown_action_id_answers_gracefully() {
        let f = fixture();

        let reply = f.notifier.handle_callback(OWNER, "approve:999999").await;

        assert!(
            reply.contains("not an action I know about"),
            "expected a human sentence, got: {reply}"
        );
        assert!(f.calls.lock().unwrap().is_empty());
    }

    /// Callback data is attacker-reachable input. Nothing in this list may
    /// panic, and none of it may reach a connector.
    #[tokio::test]
    async fn malformed_callback_data_answers_gracefully() {
        let f = fixture();
        // A real action exists, so a payload that *nearly* parses cannot be
        // dismissed just because the store is empty.
        let id = propose(&f.actions);

        let malformed = [
            "",
            ":",
            "approve",
            "approve:",
            "approve:abc",
            "approve:0",
            "approve:-1",
            // i64::MAX + 1, and a value with no chance of fitting anything
            "approve:9223372036854775808",
            "approve:99999999999999999999999999999999",
            "approve:1.0",
            "approve: 1",
            "approve:1:2",
            "reject:ö",
            "APPROVE:1",
            "delete:1",
            "approve:1; DROP TABLE actions",
            "🙂",
            "approve:\u{0}1",
        ];

        for data in malformed {
            let reply = f.notifier.handle_callback(OWNER, data).await;
            assert!(
                reply.contains("not one I recognise"),
                "{data:?} should be ignored politely, got: {reply}"
            );
        }

        assert!(
            f.calls.lock().unwrap().is_empty(),
            "malformed callback data must never reach a connector"
        );
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Proposed,
            "and must never move a real action"
        );
    }

    /// Review Focus #5. The human's thumb is faster than the network; two
    /// `approve:<id>` callbacks for the same action arrive at once. Exactly one
    /// voucher may be posted.
    ///
    /// The delay in the fake caller holds the first execution open across an
    /// await point, so the second callback is genuinely evaluated while the
    /// first is mid-flight. The loser must get a sentence with "already" in it,
    /// not a transition diagnostic.
    #[tokio::test]
    async fn a_double_tapped_approve_executes_exactly_once() {
        let f = fixture_with_delay(Duration::from_millis(50));
        let id = propose(&f.actions);

        // Bound to locals rather than inlined: `tokio::join!` holds both
        // futures past the end of the statement, so a `&format!(..)` temporary
        // in the argument would not live long enough.
        let tap = format!("approve:{id}");
        let (a, b) = tokio::join!(
            f.notifier.handle_callback(OWNER, &tap),
            f.notifier.handle_callback(OWNER, &tap)
        );

        assert_eq!(
            f.calls.lock().unwrap().len(),
            1,
            "the connector must be called once: {a} / {b}"
        );
        let replies = format!("{a} {b}");
        assert!(
            replies.contains("already") || replies.contains("Executed"),
            "{replies}"
        );
        assert!(
            replies.contains("already"),
            "the losing tap must be told it was already handled, not shown an \
             error: {replies}"
        );
        assert!(
            !replies.contains("cannot move to"),
            "a raw store diagnostic reached the human: {replies}"
        );
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Executed
        );
    }

    /// The same race with the two taps spread across tokio worker threads,
    /// so they can be running in the store at literally the same moment rather
    /// than interleaving at await points on one thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_double_tapped_approve_executes_exactly_once_across_threads() {
        let f = Arc::new(fixture_with_delay(Duration::from_millis(50)));
        let id = propose(&f.actions);

        let one = {
            let f = f.clone();
            tokio::spawn(async move {
                f.notifier
                    .handle_callback(OWNER, &format!("approve:{id}"))
                    .await
            })
        };
        let two = {
            let f = f.clone();
            tokio::spawn(async move {
                f.notifier
                    .handle_callback(OWNER, &format!("approve:{id}"))
                    .await
            })
        };
        let (a, b) = (one.await.unwrap(), two.await.unwrap());

        assert_eq!(
            f.calls.lock().unwrap().len(),
            1,
            "the connector must be called once: {a} / {b}"
        );
        assert!(format!("{a} {b}").contains("already"));
    }

    /// Approving something a human already rejected must not run it.
    #[tokio::test]
    async fn approving_an_already_rejected_action_changes_nothing() {
        let f = fixture();
        let id = propose(&f.actions);

        f.notifier
            .handle_callback(OWNER, &format!("reject:{id}"))
            .await;
        let reply = f
            .notifier
            .handle_callback(OWNER, &format!("approve:{id}"))
            .await;

        assert!(reply.contains("already handled: rejected"), "{reply}");
        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Rejected
        );
    }

    #[tokio::test]
    async fn pushing_an_already_decided_action_is_refused() {
        let f = fixture();
        let id = propose(&f.actions);
        f.actions.reject(id, "no").unwrap();

        assert!(f.notifier.push_action(id).await.is_err());
        assert!(
            f.sent.lock().unwrap().is_empty(),
            "no buttons on an action that is no longer decidable"
        );
    }

    #[tokio::test]
    async fn notify_sends_a_plain_message_with_no_buttons() {
        let f = fixture();

        f.notifier
            .notify("digest: 3 things happened")
            .await
            .unwrap();

        let sent = f.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "digest: 3 things happened");
        assert!(sent[0].1.is_empty());
    }

    #[test]
    fn truncate_never_splits_a_character() {
        // Every prefix length of a string of multi-byte characters: the naive
        // `&s[..max]` panics on most of these.
        let s = "åäöéüñ🙂🙂🙂";
        for max in 0..s.len() + 4 {
            let out = truncate(s, max);
            assert!(out.len() <= max + "…".len().max(s.len()));
        }
        assert_eq!(truncate("abc", 10), "abc");
    }

    /// The button posts a real voucher to company accounting. A tap from
    /// anyone but the owner must change nothing at all: no transition, no
    /// connector call, and a reply that does not admit the action exists.
    #[tokio::test]
    async fn a_callback_from_anyone_but_the_owner_does_nothing() {
        let f = fixture();
        let id = propose(&f.actions);

        for data in [format!("approve:{id}"), format!("reject:{id}")] {
            let reply = f.notifier.handle_callback(STRANGER, &data).await;

            assert_eq!(
                reply, UNRECOGNISED_REPLY,
                "a stranger gets the generic reply and nothing else: {reply}"
            );
            assert!(
                !reply.contains(&id.to_string()),
                "the reply named the action: {reply}"
            );
            let lower = reply.to_lowercase();
            for leak in [
                "approve",
                "reject",
                "action",
                "owner",
                "allowed",
                "permission",
            ] {
                assert!(
                    !lower.contains(leak),
                    "the reply leaked {leak:?} to a stranger: {reply}"
                );
            }
        }

        assert!(
            f.calls.lock().unwrap().is_empty(),
            "a non-owner tap must make zero connector calls"
        );
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Proposed,
            "a non-owner tap must not move the action"
        );
    }

    /// The generic reply is the whole defence against probing, so it is
    /// asserted rather than assumed: if a stranger's reply for a *valid* id
    /// differed from the reply to garbage by even one byte, anyone who found
    /// the bot could walk the id space and learn which vouchers are pending.
    #[tokio::test]
    async fn a_wrong_sender_and_malformed_data_get_byte_identical_replies() {
        let f = fixture();
        let id = propose(&f.actions);

        let malformed_from_owner = f.notifier.handle_callback(OWNER, "nonsense").await;
        let valid_from_stranger = f
            .notifier
            .handle_callback(STRANGER, &format!("approve:{id}"))
            .await;
        let unknown_from_stranger = f.notifier.handle_callback(STRANGER, "approve:999999").await;
        let malformed_from_stranger = f.notifier.handle_callback(STRANGER, "nonsense").await;

        assert_eq!(
            valid_from_stranger.as_bytes(),
            malformed_from_owner.as_bytes(),
            "a valid id from a stranger must be indistinguishable from garbage"
        );
        assert_eq!(unknown_from_stranger, malformed_from_owner);
        assert_eq!(malformed_from_stranger, malformed_from_owner);
        assert_eq!(
            valid_from_stranger, "That button is not one I recognise. Nothing was done.",
            "the exact text a stranger sees"
        );

        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(
            f.actions.get(id).unwrap().unwrap().status,
            ActionStatus::Proposed
        );
    }

    /// And the owner is unaffected by any of it.
    #[tokio::test]
    async fn the_owner_still_approves_exactly_as_before() {
        let f = fixture();
        let id = propose(&f.actions);

        let reply = f
            .notifier
            .handle_callback(OWNER, &format!("approve:{id}"))
            .await;

        assert_eq!(reply, format!("Executed #{id}."));
        assert_eq!(f.calls.lock().unwrap().len(), 1);
    }

    // --- the token, which must never leave the config directory ---

    fn write_secret(dir: &Path, name: &str, contents: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_token_is_read_from_a_private_file() {
        let dir = TempDir::new().unwrap();
        write_secret(dir.path(), TOKEN_FILE, "123456:AAEtOkenValue-_x\n", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "-1001234567890\n", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321\n", 0o644);

        let config = TelegramConfig::load_from(dir.path()).unwrap();

        assert_eq!(config.token, "123456:AAEtOkenValue-_x");
        assert_eq!(config.chat_id, -1_001_234_567_890);
    }

    /// The owner id is its own field and its own file. A group chat id is
    /// negative and belongs to no user; taking it for an allow-list would lock
    /// the owner out of their own bot.
    #[test]
    fn the_owner_id_is_a_separate_field_from_the_chat_id() {
        let dir = TempDir::new().unwrap();
        write_secret(dir.path(), TOKEN_FILE, "123456:AAEtOkenValue-_x", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "-1001234567890", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321", 0o644);

        let config = TelegramConfig::load_from(dir.path()).unwrap();

        assert_eq!(config.owner_id, TelegramUserId(7_654_321));
        assert_ne!(
            config.owner_id.0, config.chat_id,
            "a group chat id is not a user id; they must not be conflated"
        );
    }

    /// No owner id, no bot. There is no "allow everyone" default to fall into.
    #[test]
    fn a_missing_owner_id_is_an_error_not_an_open_door() {
        let dir = TempDir::new().unwrap();
        write_secret(dir.path(), TOKEN_FILE, "123456:AAEtOkenValue-_x", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "42", 0o600);

        let err = format!("{:#}", TelegramConfig::load_from(dir.path()).unwrap_err());
        assert!(err.contains(OWNER_ID_FILE), "{err}");

        write_secret(dir.path(), OWNER_ID_FILE, "not-a-number", 0o644);
        let err = format!("{:#}", TelegramConfig::load_from(dir.path()).unwrap_err());
        assert!(err.contains("user id"), "{err}");
    }

    #[test]
    fn a_world_readable_token_is_refused_and_not_echoed() {
        let dir = TempDir::new().unwrap();
        write_secret(dir.path(), TOKEN_FILE, "123456:AAEtOkenValue", 0o644);
        write_secret(dir.path(), CHAT_ID_FILE, "42", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321", 0o644);

        let err = format!("{:#}", TelegramConfig::load_from(dir.path()).unwrap_err());

        assert!(err.contains("0644"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
        assert!(
            !err.contains("AAEtOkenValue"),
            "the error leaked the token: {err}"
        );
    }

    #[test]
    fn a_token_with_url_metacharacters_is_refused_and_not_echoed() {
        let dir = TempDir::new().unwrap();
        // The token goes into the URL path; a `/` would silently retarget the
        // request at a different endpoint.
        write_secret(dir.path(), TOKEN_FILE, "123456:AA/../getUpdates", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "42", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321", 0o644);

        let err = format!("{:#}", TelegramConfig::load_from(dir.path()).unwrap_err());

        assert!(
            err.contains("does not look like a Telegram bot token"),
            "{err}"
        );
        assert!(
            !err.contains("getUpdates"),
            "the error leaked the token: {err}"
        );
    }

    #[test]
    fn an_empty_or_missing_token_is_an_error_not_a_blank_bot() {
        let dir = TempDir::new().unwrap();
        assert!(TelegramConfig::load_from(dir.path()).is_err());

        write_secret(dir.path(), TOKEN_FILE, "   \n", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "42", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321", 0o644);
        let err = format!("{:#}", TelegramConfig::load_from(dir.path()).unwrap_err());
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn the_config_never_prints_the_token() {
        let dir = TempDir::new().unwrap();
        write_secret(dir.path(), TOKEN_FILE, "123456:SeCrEtValue", 0o600);
        write_secret(dir.path(), CHAT_ID_FILE, "42", 0o600);
        write_secret(dir.path(), OWNER_ID_FILE, "7654321", 0o644);

        let config = TelegramConfig::load_from(dir.path()).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("SeCrEtValue"), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");

        let transport = TelegramTransport::from_config(&config).unwrap();
        let debug = format!("{transport:?}");
        assert!(!debug.contains("SeCrEtValue"), "{debug}");
    }

    // --- the wire ---
    //
    // The transport is the one part of this module the fake cannot check, and
    // the Bot API's JSON shape is the one thing about it that can be silently
    // wrong: a message with `reply_markup` spelled anything else arrives with
    // no buttons at all, and nothing in the daemon would ever notice. These two
    // tests point it at a local stub. Nothing here reaches Telegram.

    /// `reqwest` sends no `User-Agent` at all by default, which is what got
    /// the Canvas connector a 403 from the live API. The Bot API tolerates an
    /// anonymous client today; it need not keep doing so.
    #[tokio::test]
    async fn the_transport_identifies_the_client_by_user_agent() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:TOKEN/sendMessage"))
            .and(header("user-agent", ea_core::http::USER_AGENT))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .expect(1)
            .mount(&server)
            .await;

        TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42)
            .unwrap()
            .send("Approve this action?", &[])
            .await
            .expect("the User-Agent header must be present");
    }

    #[tokio::test]
    async fn the_transport_posts_the_bot_api_shape_telegram_expects() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:TOKEN/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        transport
            .send(
                "Approve this action?",
                &[
                    ("Approve".into(), "approve:7".into()),
                    ("Reject".into(), "reject:7".into()),
                ],
            )
            .await
            .unwrap();

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();

        assert_eq!(body["chat_id"], -42);
        assert_eq!(body["text"], "Approve this action?");
        let row = &body["reply_markup"]["inline_keyboard"][0];
        assert_eq!(row[0]["text"], "Approve");
        assert_eq!(row[0]["callback_data"], "approve:7");
        assert_eq!(row[1]["text"], "Reject");
        assert_eq!(row[1]["callback_data"], "reject:7");
    }

    /// The token sits in the URL path, and `reqwest` puts the URL in its error
    /// `Display`. A refused send must not be the thing that writes the bot's
    /// bearer credential into the log.
    #[tokio::test]
    async fn a_refused_send_errors_without_leaking_the_token() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"ok":false,"description":"Unauthorized"}"#),
            )
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:SeCrEtValue", 1).unwrap();
        let err = format!("{:#}", transport.send("hello", &[]).await.unwrap_err());

        assert!(err.contains("401"), "{err}");
        assert!(
            !err.contains("SeCrEtValue"),
            "the error leaked the token: {err}"
        );
    }

    /// The same, for a transport error rather than an HTTP status: the request
    /// dies on the connection, which is the variant whose `Display` carries the
    /// URL — and the URL is `.../bot<TOKEN>/sendMessage`.
    ///
    /// The failure is *manufactured*, not hoped for. This test used to start a
    /// `wiremock::MockServer`, take its URI and drop it, assuming nothing would
    /// rebind the port before the request went out. A concurrent test's server
    /// can, and did: a reviewer hit `unwrap_err()` on an `Ok` on a full-suite
    /// run, and it passed in isolation and on the re-run — the worst shape a
    /// failure can have, because it teaches everyone to re-run rather than
    /// look. Here the listener is *held* for the whole test, so the port cannot
    /// be taken by anybody else, and it answers every connection by dropping it
    /// unanswered. The transport error is then produced by this test, every
    /// time, on every machine.
    #[tokio::test]
    async fn a_connection_failure_errors_without_leaking_the_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hanging_up = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // Closed without a byte of response: reqwest fails the send,
                // which is the error path under test.
                drop(stream);
            }
        });

        let transport =
            TelegramTransport::with_base_url(format!("http://{addr}"), "123456:SeCrEtValue", 1)
                .unwrap();
        let err = format!("{:#}", transport.send("hello", &[]).await.unwrap_err());

        hanging_up.abort();
        assert!(
            !err.contains("SeCrEtValue"),
            "the error leaked the token: {err}"
        );
    }

    /// Telegram reports application-level failures — "chat not found", "bot
    /// was blocked by the user" — with **HTTP 200** and `ok: false`. Checking
    /// only the status code calls those a delivered message.
    #[tokio::test]
    async fn a_200_with_ok_false_is_a_failure_and_carries_the_description() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
            ))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:SeCrEtValue", 1).unwrap();
        let err = format!("{:#}", transport.send("hello", &[]).await.unwrap_err());

        assert!(
            err.contains("chat not found"),
            "the description is the only thing that says what went wrong: {err}"
        );
        assert!(
            !err.contains("SeCrEtValue"),
            "the error leaked the token: {err}"
        );
    }

    /// A 200 that is not the documented envelope at all — a captive proxy, a
    /// misconfigured self-hosted Bot API server — is also not a delivery.
    #[tokio::test]
    async fn a_200_that_is_not_the_bot_api_envelope_is_a_failure() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>signed in?</html>"))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:SeCrEtValue", 1).unwrap();
        let err = format!("{:#}", transport.send("hello", &[]).await.unwrap_err());

        assert!(err.contains("envelope"), "{err}");
        assert!(!err.contains("SeCrEtValue"), "{err}");
    }

    /// `reqwest::Client::new()` has no timeout whatsoever, and in Task 13 the
    /// caller of this is a scheduler job: one black-holed `api.telegram.org`
    /// would hold it until the process dies. The production value is 30 s;
    /// the test shortens it so the test is fast. What is under test is that
    /// the client has a timeout at all.
    #[tokio::test]
    async fn a_stalled_send_gives_up_instead_of_blocking_forever() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_string(r#"{"ok":true}"#),
            )
            .mount(&server)
            .await;

        let transport = TelegramTransport::build(
            server.uri(),
            "123456:SeCrEtValue",
            1,
            Duration::from_millis(150),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let err = format!("{:#}", transport.send("hello", &[]).await.unwrap_err());
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "the send did not time out; it waited {elapsed:?}"
        );
        assert!(
            !err.contains("SeCrEtValue"),
            "the timeout error leaked the token: {err}"
        );
    }

    /// The production constructor is fallible rather than panicking, and its
    /// happy path still builds.
    #[test]
    fn the_default_transport_builds_rather_than_panicking() {
        assert!(TelegramTransport::new("123456:TOKEN", 1).is_ok());
        assert!(
            HTTP_TIMEOUT > Duration::ZERO,
            "a timeout of zero is no timeout"
        );
    }
    // -- the long-poll half of the wire (Task 13, Addition 1) ---------------
    //
    // Same reasoning as the `sendMessage` stub above: the request shape is the
    // part that cannot be checked by the type system, and a `getUpdates` with
    // the offset spelled wrong would silently replay every tap forever.
    // Pointed at a local stub; nothing here reaches Telegram.

    #[tokio::test]
    async fn get_updates_sends_the_offset_and_parses_a_callback_query() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:TOKEN/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"ok":true,"result":[
                    {"update_id":41,
                     "callback_query":{
                        "id":"cb-1",
                        "from":{"id":10000001,"is_bot":false,"first_name":"G"},
                        "message":{"message_id":9,"chat":{"id":-42,"type":"private"}},
                        "data":"approve:7"}}]}"#,
            ))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        let updates = transport.get_updates(Some(41), 25).await.unwrap();

        assert_eq!(updates.len(), 1);
        let query = updates[0].callback_query.as_ref().unwrap();
        assert_eq!(updates[0].update_id, 41);
        assert_eq!(query.from.id, 10_000_001, "the identity is from.id");
        assert_eq!(
            query.message.as_ref().unwrap().chat.as_ref().unwrap().id,
            -42
        );
        assert_eq!(query.data.as_deref(), Some("approve:7"));

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["offset"], 41);
        assert_eq!(body["timeout"], 25);
        assert_eq!(
            body["allowed_updates"],
            serde_json::json!(["callback_query", "message"]),
            "exactly the two kinds the daemon acts on, and no more"
        );
    }

    /// The very first poll after a fresh install has no offset to confirm.
    #[tokio::test]
    async fn get_updates_omits_the_offset_when_there_is_none() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":[]}"#))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        assert!(transport.get_updates(None, 25).await.unwrap().is_empty());

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("offset").is_none(), "{body}");
    }

    /// An update kind this build has never modelled must not fail the whole
    /// batch: the offset still has to advance past it.
    #[tokio::test]
    async fn an_unmodelled_update_kind_parses_as_an_ignorable_update() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"ok":true,"result":[
                    {"update_id":7,"message":{"message_id":1,"text":"hello"}},
                    {"update_id":8,"some_future_field":{"x":1}}]}"#,
            ))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        let updates = transport.get_updates(None, 25).await.unwrap();
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().all(|u| u.callback_query.is_none()));
        assert_eq!(updates[1].update_id, 8);
    }

    /// The reply to a message goes to the chat the message came from, via
    /// `sendMessage` — not to the configured chat by way of `Transport::send`,
    /// which would answer the wrong room if the two ever differed.
    #[tokio::test]
    async fn send_message_posts_to_the_chat_it_was_given() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:TOKEN/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        // The transport is configured for chat -42; the reply is addressed to
        // 4242, and that is where it must go.
        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        transport
            .send_message(4242, "the tenta is on the 14th")
            .await
            .unwrap();

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(body["chat_id"], 4242);
        assert_eq!(body["text"], "the tenta is on the 14th");
        assert!(
            body.get("reply_markup").is_none(),
            "a chat reply carries no buttons: {body}"
        );
    }

    /// A model can write more than Telegram accepts, and a rejected
    /// `sendMessage` means the owner sees nothing at all.
    #[tokio::test]
    async fn a_long_reply_is_truncated_before_it_is_sent() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        transport.send_message(1, &"x".repeat(9_000)).await.unwrap();

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.len() <= MAX_MESSAGE_BYTES + 8, "{} bytes", text.len());
    }

    /// HTTP 200 with `ok: false` is how Telegram reports "your token is
    /// wrong". A status-code-only check would call that a successful poll and
    /// the loop would spin on it forever without the breaker ever seeing one.
    #[tokio::test]
    async fn get_updates_treats_ok_false_as_a_failure_without_leaking_the_token() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
                ),
            )
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:SECRET", -42).unwrap();
        let err = transport.get_updates(None, 25).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("Unauthorized"), "{text}");
        assert!(
            !text.contains("SECRET"),
            "the token must not reach a log: {text}"
        );
    }

    #[tokio::test]
    async fn answer_callback_query_posts_the_id_and_the_text() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123456:TOKEN/answerCallbackQuery"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":true}"#),
            )
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:TOKEN", -42).unwrap();
        transport
            .answer_callback("cb-1", "Executed #7.")
            .await
            .unwrap();

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["callback_query_id"], "cb-1");
        assert_eq!(body["text"], "Executed #7.");
    }

    #[tokio::test]
    async fn a_refused_callback_answer_is_an_error_without_the_token() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"ok":false,"description":"query is too old"}"#),
            )
            .mount(&server)
            .await;

        let transport =
            TelegramTransport::with_base_url(server.uri(), "123456:SECRET", -42).unwrap();
        let err = transport.answer_callback("cb-1", "hi").await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("too old"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    // -- free text ----------------------------------------------------------

    #[tokio::test]
    async fn the_owners_message_reaches_the_conversation_and_comes_back_answered() {
        let (f, chat) = chat_fixture(FakeChat::answering("the tenta is on the 14th"));

        let reply = f.notifier.handle_message(OWNER, "when is the tenta?").await;

        assert_eq!(reply, "the tenta is on the 14th");
        assert_eq!(
            chat.seen(),
            vec![("telegram".to_string(), "when is the tenta?".to_string())],
            "a message is tagged with the surface it arrived on"
        );
    }

    /// The other half of the identity check, and the reason `handle_message`
    /// exists rather than the update loop calling the chat service directly.
    /// `message.from.id` is the person; `message.chat.id` is the room. A
    /// stranger must get the *same* sentence a stranger's button press gets,
    /// byte for byte, or the two replies become an oracle for probing.
    #[tokio::test]
    async fn a_message_from_anyone_but_the_owner_is_refused_identically_to_a_callback() {
        let (f, chat) = chat_fixture(FakeChat::answering("secret answer"));

        let to_a_message = f.notifier.handle_message(STRANGER, "hello?").await;
        let to_a_callback = f.notifier.handle_callback(STRANGER, "approve:1").await;

        assert_eq!(
            to_a_message, UNRECOGNISED_REPLY,
            "a stranger's message gets the generic reply: {to_a_message}"
        );
        assert_eq!(
            to_a_message, to_a_callback,
            "the two refusals must be byte-identical, or they are an oracle"
        );
        assert!(
            chat.seen().is_empty(),
            "a stranger must not reach the conversation at all: {:?}",
            chat.seen()
        );
        let lower = to_a_message.to_lowercase();
        for leak in ["owner", "allowed", "permission", "conversation", "chat"] {
            assert!(!lower.contains(leak), "the reply leaked {leak:?}");
        }
    }

    /// An off-by-one neighbour of the owner's id is the bug this guards; so is
    /// the *chat* id being used as the person. The configured chat in these
    /// tests is not the owner's user id, and a notifier handed it must refuse.
    #[tokio::test]
    async fn the_chat_id_is_not_an_identity() {
        let (f, chat) = chat_fixture(FakeChat::answering("secret answer"));
        // What `message.chat.id` would be in the owner's own DM with a group
        // bot: a different number entirely.
        let as_if_chat_were_the_person = TelegramUserId(-1_001_234_567_890);

        let reply = f
            .notifier
            .handle_message(as_if_chat_were_the_person, "hello?")
            .await;

        assert_eq!(reply, UNRECOGNISED_REPLY);
        assert!(chat.seen().is_empty());
    }

    #[tokio::test]
    async fn a_message_to_a_daemon_with_no_chat_service_says_so_to_the_owner_only() {
        let f = fixture();
        assert_eq!(
            f.notifier.handle_message(OWNER, "hello").await,
            NO_CHAT_REPLY
        );
        // And a stranger still learns nothing, chat service or not.
        assert_eq!(
            f.notifier.handle_message(STRANGER, "hello").await,
            UNRECOGNISED_REPLY
        );
    }

    #[tokio::test]
    async fn a_message_with_no_text_is_answered_rather_than_ignored() {
        let (f, chat) = chat_fixture(FakeChat::answering("hi"));
        assert_eq!(f.notifier.handle_message(OWNER, "   ").await, NO_TEXT_REPLY);
        assert!(
            chat.seen().is_empty(),
            "nothing to answer, nothing recorded"
        );
    }

    /// The over-budget turn: `ChatService::say` answers the owner *and*
    /// attaches the note saying the day's session budget is spent, so triage
    /// has stopped scoring and no briefing will be written. The phone is the
    /// surface the owner is most likely to be on and least likely to run `ea
    /// status` from, so it must show both. Taking the reply and dropping the
    /// note leaves the owner crossing a spending bound they were never told
    /// about.
    #[tokio::test]
    async fn an_over_budget_turn_shows_the_owner_the_note_as_well_as_the_reply() {
        let (f, _chat) = chat_fixture(FakeChat::answering_with_note(
            "the tenta is on the 14th",
            "answered anyway, but the daily session budget of 60 is spent",
        ));

        let reply = f.notifier.handle_message(OWNER, "when is the tenta?").await;

        assert!(
            reply.contains("the tenta is on the 14th"),
            "the answer must still be there: {reply}"
        );
        assert!(
            reply.contains("the daily session budget of 60 is spent"),
            "the note must reach the phone too: {reply}"
        );
        assert!(
            reply.find("the tenta").unwrap() < reply.find("answered anyway").unwrap(),
            "the note is appended after the answer, not interleaved: {reply}"
        );
    }

    /// The over-budget note is appended *after* the reply, so a near-limit
    /// reply used to push it off the end at `MAX_MESSAGE_BYTES` — the one
    /// case where the note matters most is the one where it vanished, and the
    /// owner was billed past their ceiling without being told. The reply is
    /// what gets cut; the note is reserved before the cut is made.
    #[tokio::test]
    async fn a_long_reply_is_cut_to_make_room_for_the_over_budget_note() {
        const NOTE: &str = "answered anyway, but the daily session budget of 60 is spent";
        // Multi-byte all the way through: the cut has to keep landing on a
        // character boundary, whatever the reserved length pushes it onto.
        let long = "ä".repeat(4_000);
        let (f, _chat) = chat_fixture(FakeChat::answering_with_note(&long, NOTE));

        let reply = f.notifier.handle_message(OWNER, "when is the tenta?").await;

        // What `Transport::send` and `send_message` actually put on the wire.
        let on_the_wire = truncate(&reply, MAX_MESSAGE_BYTES);
        assert!(
            on_the_wire.contains(NOTE),
            "the note has to reach the phone, not be cut off the end of it \
             ({} bytes before the transport's cut)",
            reply.len()
        );
        assert!(
            on_the_wire.contains("ääää"),
            "the answer is still there, cut short"
        );
        assert_eq!(
            on_the_wire,
            reply,
            "the reply is cut to fit the note, so the transport has nothing \
             left to take: {} bytes",
            reply.len()
        );
    }

    /// And a turn with only a note still shows it: that is the no-reply case.
    #[tokio::test]
    async fn a_turn_with_only_a_note_still_shows_the_note() {
        let (f, _chat) = chat_fixture(FakeChat::noting(
            "recorded, but this daemon has no session runner",
        ));

        let reply = f.notifier.handle_message(OWNER, "when is the tenta?").await;

        assert!(reply.contains("no session runner"), "{reply}");
    }

    /// A session that failed stored no assistant message, so the phone is told
    /// the turn failed rather than being handed a plausible-looking answer.
    #[tokio::test]
    async fn a_failed_turn_is_a_sentence_not_a_panic() {
        let (f, _chat) = chat_fixture(FakeChat::failing());
        let reply = f.notifier.handle_message(OWNER, "when is the tenta?").await;
        assert!(reply.contains("Something went wrong"), "{reply}");
        assert!(
            !reply.contains("status 1"),
            "the detail belongs in the log, not on the phone: {reply}"
        );
    }
}
