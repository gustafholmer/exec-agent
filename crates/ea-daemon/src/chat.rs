//! The conversation, owned by the daemon and shared by both surfaces.
//!
//! `ea chat` and a Telegram message are two clients of one thread. Neither
//! owns it: the thread lives in `conversations`/`messages`, the model's side of
//! it lives in the stored `claude` session id, and this module is the only
//! thing that advances either. That is what makes a question asked on the phone
//! answerable in the terminal ten seconds later, mid-sentence.
//!
//! Three properties are load-bearing, and each has a test:
//!
//! * **One turn at a time.** [`ChatService::say`] takes a lock for the whole
//!   turn. A message arriving while a session is running *queues*; it does not
//!   start a second session against the same thread. Two sessions resuming the
//!   same `claude` session id would interleave two half-answers into the
//!   transcript and both would write to the store.
//! * **A failed turn leaves no phantom reply.** The user's message is stored —
//!   nothing a human said is thrown away — but a session that errors returns
//!   the error to the caller and stores no assistant message. A conversation
//!   that shows an answer nobody gave is worse than one with a gap.
//! * **The only way out of a chat session is `propose_action`.** A chat session
//!   gets no connectors, and [`crate::session::ALLOWED_TOOLS`] is a closed list
//!   of two: `propose_action`, which comes back to the daemon's `propose` and
//!   the policy gate, and `remember`, which writes one row to the local `facts`
//!   table and reaches nothing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::Utc;
use chrono_tz::Tz;
use ea_core::store::conversations::ConversationStore;
use ea_core::store::facts::{Fact, FactStore};
use ea_core::store::runs::RunStore;
use serde::Serialize;

use crate::session::SessionRequest;
use crate::triage::SessionBoundary;

/// `kind` recorded on the `runs` row of a chat session.
pub const CHAT_RUN_KIND: &str = "chat";

/// The terminal.
pub const SURFACE_CLI: &str = "cli";

/// The owner's phone.
pub const SURFACE_TELEGRAM: &str = "telegram";

/// The surfaces a message may arrive on.
///
/// A closed list because `surface` is written to every row of the transcript
/// and read back as a string; an unvalidated one would let a typo in a client
/// create a third surface that nothing ever renders.
pub const SURFACES: [&str; 2] = [SURFACE_CLI, SURFACE_TELEGRAM];

/// The longest message a surface may hand in.
///
/// Telegram's own limit is 4096 UTF-16 units; this bounds what the CLI can
/// paste into a prompt too, so one turn cannot push the session past anything
/// useful or write a novel into `messages`.
pub const MAX_MESSAGE_CHARS: usize = 8_000;

/// What a chat session is told it is, before the surface, the clock and the
/// facts are appended. See [`system_prompt`].
pub const CHAT_SYSTEM_PROMPT: &str = concat!(
    "You are the user's executive assistant, answering over a text interface. ",
    "Be brief and concrete. You cannot act directly: the only way to change ",
    "anything in the world is the propose_action tool, which records a proposal ",
    "for the user to approve. Never claim to have done something you only proposed. ",
    "Use the remember tool when the user tells you something worth keeping ",
    "beyond this conversation; it stores a fact and changes nothing else."
);

/// One completed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatTurn {
    pub conversation_id: i64,
    /// Id of the stored *user* message. Always present: the turn records what
    /// the human said before anything can go wrong with the answer.
    pub message_id: i64,
    /// The assistant's answer, or `None` when no session could be run.
    pub reply: Option<String>,
    /// Why there is no reply, in a sentence for a human. `None` when there is
    /// one.
    pub note: Option<String>,
}

/// Something that can answer a free-text message.
///
/// A trait so that the Telegram notifier can be tested against a fake with no
/// database and no session runner behind it, and so that `notify::telegram`
/// does not depend on the store wiring.
pub trait ChatResponder: Send + Sync {
    fn respond<'a>(
        &'a self,
        surface: &'a str,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatTurn>> + Send + 'a>>;
}

/// The conversation, and the one path that advances it.
pub struct ChatService {
    conversations: ConversationStore,
    facts: FactStore,
    runs: RunStore,
    sessions: Option<Arc<dyn SessionBoundary>>,
    daily_session_budget: u32,
    model: String,
    time_zone: Tz,
    /// Held for the whole of a turn. See the module docs: this is what makes a
    /// message arriving mid-session queue instead of interleaving.
    turn: tokio::sync::Mutex<()>,
}

impl ChatService {
    pub fn new(
        conversations: ConversationStore,
        facts: FactStore,
        runs: RunStore,
        sessions: Option<Arc<dyn SessionBoundary>>,
        daily_session_budget: u32,
        model: impl Into<String>,
        time_zone: Tz,
    ) -> Self {
        Self {
            conversations,
            facts,
            runs,
            sessions,
            daily_session_budget,
            model: model.into(),
            time_zone,
            turn: tokio::sync::Mutex::new(()),
        }
    }

    pub fn facts(&self) -> &FactStore {
        &self.facts
    }

    /// Whether a `claude` binary was found at startup. `false` is a degraded
    /// daemon, not a broken one: `ea status` says so, and a chat records the
    /// message and explains why there is no answer.
    pub fn sessions_available(&self) -> bool {
        self.sessions.is_some()
    }

    pub fn daily_session_budget(&self) -> u32 {
        self.daily_session_budget
    }

    /// The model a chat turn runs on, reported by `ea status` so that what the
    /// daemon spends is visible without reading the config.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Say something, and wait for the answer.
    ///
    /// The whole turn — store the message, run the session, store the reply —
    /// happens under one lock, so two surfaces cannot be mid-turn on the same
    /// thread at once. A caller that arrives during someone else's turn waits
    /// for it; that is deliberate, and it is why `ea chat`'s client timeout is
    /// generous.
    pub async fn say(&self, surface: &str, message: &str) -> anyhow::Result<ChatTurn> {
        let surface = validated_surface(surface)?;
        let message = message.trim().to_string();
        if message.is_empty() {
            bail!("chat: `message` must not be empty");
        }
        if message.chars().count() > MAX_MESSAGE_CHARS {
            bail!(
                "chat: `message` is {} characters; the limit is {MAX_MESSAGE_CHARS}",
                message.chars().count()
            );
        }

        // Queue behind whatever turn is already running, then take the
        // conversation id: a turn that waited must join the thread as it is
        // *now*, not as it was when the message arrived.
        let _turn = self.turn.lock().await;

        let conversation_id = self.conversations.current()?;
        let stored = self
            .conversations
            .append(conversation_id, "user", surface, &message)?;

        let outcome = self.reply_to(conversation_id, surface, &message).await;

        let (reply, note) = match outcome {
            Ok(Some(reply)) => (Some(reply), None),
            Ok(None) => (None, Some(self.why_no_reply())),
            // Not swallowed into a note: the caller asked a question and did
            // not get an answer, and a turn that failed must not leave an
            // assistant message in the thread.
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "chat session failed");
                return Err(err.context("the chat session failed"));
            }
        };

        if let Some(reply) = &reply {
            self.conversations
                .append(conversation_id, "assistant", surface, reply)?;
        }

        Ok(ChatTurn {
            conversation_id,
            message_id: stored.id,
            reply,
            note,
        })
    }

    /// Run one chat session, or `None` when there is no session runner or the
    /// day's budget is spent.
    async fn reply_to(
        &self,
        conversation_id: i64,
        surface: &str,
        message: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(sessions) = self.sessions.as_ref() else {
            return Ok(None);
        };
        if crate::jobs::session_budget_spent(&self.runs, self.daily_session_budget, Utc::now())? {
            return Ok(None);
        }

        let relevant = self.facts.matching(message)?;
        let now = Utc::now().with_timezone(&self.time_zone);
        let mut request = SessionRequest::new(
            CHAT_RUN_KIND,
            message,
            system_prompt(surface, now, &relevant),
        )
        // Read tools are not in `session::ALLOWED_TOOLS` anyway; handing a
        // chat session connector servers would spawn children it cannot
        // call.
        .with_connectors(Vec::<String>::new())
        // Explicit, and the whole point of the field. Unset, the CLI
        // inherits the model the human last picked for interactive work —
        // `opus-5[1m]` on this machine, around 30x tier 1's rate — and
        // charges it against the same daily session budget, so a chat
        // would silently cost thirty triage passes and the price would
        // change whenever the owner changed an editor setting.
        .with_model(&self.model);
        if let Some(previous) = self.conversations.claude_session(conversation_id)? {
            request = request.with_resume(previous);
        }

        let outcome = sessions.run_session(request).await?;
        if let Some(session_id) = &outcome.session_id {
            // Recorded so the next message — on either surface — continues the
            // same thread rather than starting from nothing.
            self.conversations
                .set_claude_session(conversation_id, session_id)?;
        }
        Ok(Some(outcome.text))
    }

    fn why_no_reply(&self) -> String {
        if self.sessions.is_none() {
            "recorded, but this daemon has no session runner (is `claude` on PATH?), \
             so there is no reply"
                .to_string()
        } else {
            format!(
                "recorded, but the daily session budget of {} is spent, so there is no reply",
                self.daily_session_budget
            )
        }
    }
}

impl ChatResponder for ChatService {
    fn respond<'a>(
        &'a self,
        surface: &'a str,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ChatTurn>> + Send + 'a>> {
        Box::pin(self.say(surface, message))
    }
}

/// `surface`, if it is one.
pub fn validated_surface(surface: &str) -> anyhow::Result<&'static str> {
    SURFACES
        .into_iter()
        .find(|known| *known == surface)
        .with_context(|| format!("chat: unknown surface {surface:?}; expected one of {SURFACES:?}"))
}

/// The system prompt for one turn.
///
/// Pure, so the two things worth asserting about it — that it names the
/// surface and the local time, and that a conversation matching no facts gets
/// no facts block *at all* rather than an empty heading — are testable without
/// a database. An empty heading is not cosmetic: "What you have been told to
/// remember:" followed by nothing invites the model to fill the gap.
pub fn system_prompt(surface: &str, now: chrono::DateTime<Tz>, facts: &[Fact]) -> String {
    let mut prompt = String::from(CHAT_SYSTEM_PROMPT);
    prompt.push_str(&format!(
        "\n\nYou are speaking to the user over: {surface}. \
         The current local time is {} ({}).",
        now.format("%Y-%m-%d %H:%M"),
        now.timezone().name(),
    ));
    if !facts.is_empty() {
        prompt.push_str("\n\nWhat you have been told to remember:");
        for fact in facts {
            prompt.push_str(&format!("\n- {}: {}", fact.topic, fact.body));
        }
        prompt
            .push_str("\nCorrect any of these with the remember tool if the user says otherwise.");
    }
    prompt
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::session::SessionOutcome;
    use crate::triage::BoxedSession;

    /// A database in a temp dir. `ea_core`'s own `test_support` is
    /// crate-private, so this crate keeps its own two lines of it.
    fn temp_store() -> (
        tempfile::TempDir,
        Arc<std::sync::Mutex<rusqlite::Connection>>,
    ) {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = ea_core::db::open(&dir.path().join("state.db")).unwrap();
        (dir, Arc::new(std::sync::Mutex::new(conn)))
    }

    fn a_fact(topic: &str, body: &str) -> Fact {
        Fact {
            id: 1,
            topic: topic.to_string(),
            body: body.to_string(),
            created_at: "2026-09-24T09:00:00Z".to_string(),
            updated_at: None,
        }
    }

    fn noon() -> chrono::DateTime<Tz> {
        use chrono::TimeZone;
        chrono_tz::Europe::Stockholm
            .with_ymd_and_hms(2026, 9, 24, 12, 30, 0)
            .unwrap()
    }

    #[test]
    fn the_prompt_names_the_surface_the_time_and_the_one_way_to_act() {
        let prompt = system_prompt(SURFACE_TELEGRAM, noon(), &[]);
        assert!(prompt.contains("telegram"), "{prompt}");
        assert!(prompt.contains("2026-09-24 12:30"), "{prompt}");
        assert!(prompt.contains("Europe/Stockholm"), "{prompt}");
        assert!(prompt.contains("propose_action"), "{prompt}");
    }

    #[test]
    fn matching_facts_are_injected() {
        let prompt = system_prompt(
            SURFACE_CLI,
            noon(),
            &[a_fact("tenta", "the databases tenta is on the 14th")],
        );
        assert!(
            prompt.contains("What you have been told to remember:"),
            "{prompt}"
        );
        assert!(
            prompt.contains("tenta: the databases tenta is on the 14th"),
            "{prompt}"
        );
    }

    /// An empty heading invites the model to invent what belongs under it.
    #[test]
    fn no_matching_facts_means_no_facts_block_at_all() {
        let prompt = system_prompt(SURFACE_CLI, noon(), &[]);
        assert!(!prompt.contains("remember:"), "{prompt}");
        assert!(!prompt.contains("What you have been told"), "{prompt}");
    }

    #[test]
    fn an_unknown_surface_is_refused() {
        assert_eq!(validated_surface("cli").unwrap(), SURFACE_CLI);
        assert_eq!(validated_surface("telegram").unwrap(), SURFACE_TELEGRAM);
        let err = validated_surface("sms").unwrap_err().to_string();
        assert!(err.contains("sms"), "{err}");
    }

    // -- the service --------------------------------------------------------

    /// A session runner that never spawns anything, records what it was asked,
    /// and reports the highest number of sessions that were ever in flight at
    /// once.
    struct FakeSessions {
        reply: String,
        fail: bool,
        seen: StdMutex<Vec<SessionRequest>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        delay: std::time::Duration,
    }

    impl FakeSessions {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                fail: false,
                seen: StdMutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                delay: std::time::Duration::ZERO,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                reply: String::new(),
                fail: true,
                seen: StdMutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                delay: std::time::Duration::ZERO,
            })
        }

        fn slow(reply: &str, delay: std::time::Duration) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                fail: false,
                seen: StdMutex::new(Vec::new()),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                delay,
            })
        }

        fn requests(&self) -> Vec<SessionRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SessionBoundary for FakeSessions {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            self.seen.lock().unwrap().push(req);
            let reply = self.reply.clone();
            let fail = self.fail;
            let delay = self.delay;
            Box::pin(async move {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                } else {
                    tokio::task::yield_now().await;
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                if fail {
                    anyhow::bail!("claude exited with status 1");
                }
                Ok(SessionOutcome {
                    text: reply,
                    structured: None,
                    session_id: Some("sess-1".to_string()),
                    cost_usd: Some(0.001),
                })
            })
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        service: Arc<ChatService>,
        conversations: ConversationStore,
        facts: FactStore,
        sessions: Arc<FakeSessions>,
    }

    fn harness(sessions: Arc<FakeSessions>) -> Harness {
        let (dir, conn) = temp_store();
        let conversations = ConversationStore::new(Arc::clone(&conn));
        let facts = FactStore::new(Arc::clone(&conn));
        let service = Arc::new(ChatService::new(
            conversations.clone(),
            facts.clone(),
            RunStore::new(Arc::clone(&conn)),
            Some(Arc::clone(&sessions) as Arc<dyn SessionBoundary>),
            60,
            "claude-sonnet-4-5",
            chrono_tz::Europe::Stockholm,
        ));
        Harness {
            _dir: dir,
            service,
            conversations,
            facts,
            sessions,
        }
    }

    impl Harness {
        fn messages(&self) -> Vec<ea_core::store::conversations::Message> {
            let id = self.conversations.current().unwrap();
            self.conversations.recent(id, 100).unwrap()
        }
    }

    #[tokio::test]
    async fn one_message_runs_one_session_and_stores_both_sides() {
        let h = harness(FakeSessions::new("the tenta is on the 14th"));
        let turn = h
            .service
            .say(SURFACE_CLI, "when is the tenta?")
            .await
            .unwrap();

        assert_eq!(turn.reply.as_deref(), Some("the tenta is on the 14th"));
        assert_eq!(turn.note, None);
        assert_eq!(h.sessions.requests().len(), 1, "exactly one session");

        let messages = h.messages();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].body, "when is the tenta?");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].body, "the tenta is on the 14th");
        assert_eq!(
            messages[1].surface, SURFACE_CLI,
            "the reply is tagged with the surface it is going to"
        );
    }

    /// The 50/50 split, made real: the daemon owns the thread and both
    /// surfaces are clients of it.
    #[tokio::test]
    async fn a_thread_started_in_telegram_continues_in_the_terminal() {
        let h = harness(FakeSessions::new("ok"));
        h.service
            .say(SURFACE_TELEGRAM, "Remind me what I asked about the tenta")
            .await
            .unwrap();
        h.service
            .say(SURFACE_CLI, "and what did you suggest?")
            .await
            .unwrap();

        let messages = h.messages();
        assert_eq!(
            messages.len(),
            4,
            "two turns, each a user message and a reply"
        );
        assert_eq!(
            messages
                .iter()
                .map(|m| m.surface.as_str())
                .collect::<Vec<_>>(),
            ["telegram", "telegram", "cli", "cli"]
        );

        // The second turn must resume, not start fresh — asserted on the argv
        // the runner would build, not just on the request field.
        let requests = h.sessions.requests();
        assert!(
            requests[0].resume.is_none(),
            "the first turn has nothing to resume"
        );
        assert_eq!(requests[1].resume.as_deref(), Some("sess-1"));
        let argv = crate::session::build_argv(&requests[1], &empty_mcp());
        assert!(argv.contains(&"--resume".to_string()), "{argv:?}");
        assert!(argv.contains(&"sess-1".to_string()), "{argv:?}");

        let first = crate::session::build_argv(&requests[0], &empty_mcp());
        assert!(!first.contains(&"--resume".to_string()), "{first:?}");
    }

    fn empty_mcp() -> crate::session::McpConfig {
        crate::session::McpConfig::for_session(
            std::path::Path::new("/usr/local/bin/ea-propose"),
            &[],
            &[],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn the_returned_session_id_is_persisted_on_the_first_turn() {
        let h = harness(FakeSessions::new("ok"));
        let turn = h.service.say(SURFACE_CLI, "hello").await.unwrap();
        assert_eq!(
            h.conversations
                .claude_session(turn.conversation_id)
                .unwrap()
                .as_deref(),
            Some("sess-1")
        );
    }

    /// A failed turn must not leave an answer nobody gave.
    #[tokio::test]
    async fn a_failed_session_is_an_error_and_stores_no_assistant_message() {
        let h = harness(FakeSessions::failing());
        let err = h
            .service
            .say(SURFACE_CLI, "when is the tenta?")
            .await
            .expect_err("a failed session must reach the caller");
        assert!(
            format!("{err:#}").contains("chat session failed"),
            "{err:#}"
        );

        let messages = h.messages();
        assert_eq!(
            messages.len(),
            1,
            "only the user's own message: {messages:?}"
        );
        assert_eq!(messages[0].role, "user");
    }

    /// Two messages at once must not become two sessions on one thread: they
    /// would resume the same `claude` session id and interleave two half
    /// answers into the transcript.
    #[tokio::test]
    async fn a_message_arriving_mid_session_queues_rather_than_interleaving() {
        let h = harness(FakeSessions::slow(
            "ok",
            std::time::Duration::from_millis(80),
        ));

        let first = {
            let service = Arc::clone(&h.service);
            tokio::spawn(async move { service.say(SURFACE_TELEGRAM, "the first question").await })
        };
        // Long enough that the second call lands while the first session is
        // still running.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let second = {
            let service = Arc::clone(&h.service);
            tokio::spawn(async move { service.say(SURFACE_CLI, "the second question").await })
        };

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(
            h.sessions.peak.load(Ordering::SeqCst),
            1,
            "two sessions were in flight on one conversation at once"
        );
        let messages = h.messages();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(
            messages.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            ["user", "assistant", "user", "assistant"],
            "the transcript must not interleave"
        );
        // The queued turn resumed the session the first one established.
        let requests = h.sessions.requests();
        assert_eq!(requests[1].resume.as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn the_prompt_carries_the_facts_that_match_the_message() {
        let h = harness(FakeSessions::new("ok"));
        h.facts
            .remember("tenta", "the databases tenta is on the 14th")
            .unwrap();
        h.facts
            .remember("invoicing", "invoices go to Ekonomi AB")
            .unwrap();

        h.service
            .say(SURFACE_CLI, "when is the tenta?")
            .await
            .unwrap();

        let requests = h.sessions.requests();
        let prompt = &requests[0].system_prompt;
        assert!(
            prompt.contains("databases tenta is on the 14th"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("Ekonomi AB"),
            "an unrelated fact must not be injected: {prompt}"
        );
    }

    #[tokio::test]
    async fn a_conversation_matching_no_facts_gets_no_facts_block() {
        let h = harness(FakeSessions::new("ok"));
        h.facts.remember("tenta", "on the 14th").unwrap();

        h.service.say(SURFACE_CLI, "hello there").await.unwrap();

        let requests = h.sessions.requests();
        assert!(
            !requests[0].system_prompt.contains("remember:"),
            "{}",
            requests[0].system_prompt
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_refused_and_stores_nothing() {
        let h = harness(FakeSessions::new("ok"));
        let err = h.service.say(SURFACE_CLI, "   ").await.unwrap_err();
        assert!(format!("{err:#}").contains("must not be empty"), "{err:#}");
        assert!(h.sessions.requests().is_empty());
    }

    #[tokio::test]
    async fn without_a_session_runner_the_message_is_still_recorded() {
        let (_dir, conn) = temp_store();
        let conversations = ConversationStore::new(Arc::clone(&conn));
        let service = ChatService::new(
            conversations.clone(),
            FactStore::new(Arc::clone(&conn)),
            RunStore::new(Arc::clone(&conn)),
            None,
            60,
            "claude-sonnet-4-5",
            chrono_tz::Europe::Stockholm,
        );

        let turn = service.say(SURFACE_TELEGRAM, "hello").await.unwrap();
        assert_eq!(turn.reply, None);
        assert!(turn.note.unwrap().contains("no session runner"));
        let id = conversations.current().unwrap();
        assert_eq!(conversations.recent(id, 10).unwrap().len(), 1);
    }
}
