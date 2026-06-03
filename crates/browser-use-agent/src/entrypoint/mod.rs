//! Run-entrypoint facade: the binary-facing function tui/cli call to run a
//! session on the new async engine.
//!
//! This is the single callable that assembles config + session store + model
//! route + turn loop into one run, driving a session turn through the real
//! [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver) path. It
//! closes the gap where
//! [`build_sampling_driver`](crate::turn::model_path::build_sampling_driver)
//! existed but nothing binary-facing called it (the call lands in
//! [`provider::resolve_provider`], reached from [`run_session_with_config`]).
//!
//! ## Assembly order (parity with the legacy `browser-use-core` run path)
//! The legacy `run_session` / `run_existing_session_from_config` assembled:
//! config → provider selection → workspace-context seed → loop → persistence.
//! This facade reproduces that order on the new primitives:
//!   1. resolve the provider backend → model route → sampling driver via
//!      [`provider::resolve_provider`] (**where `build_sampling_driver` is called**),
//!   2. seed the environment workspace-context durable event before the first
//!      turn ([`append_environment_context_event`]),
//!   3. drive the [`TurnLoop`] to quiescence over a store-backed [`TurnState`] +
//!      a [`TurnObserver`] that persists the run's result,
//!   4. return the [`SessionId`].
//!
//! The run's prompt history is whatever is already in the session's durable log
//! (e.g. a `session.input` the caller appended). [`ProviderRunConfig`] carries no
//! initial-user-text field of its own, so the facade does not synthesize one — it
//! drives over the existing log, exactly as the legacy resume path does for an
//! already-seeded session.
//!
//! ## What runs end-to-end vs. documented boundaries
//! - The **fake** backend runs end-to-end offline (the loop is driven to
//!   completion through an offline scripted [`SamplingDriver`]), proving the
//!   assembly with zero network.
//! - A **real** backend's driver is CONSTRUCTED via the same path
//!   ([`build_sampling_driver`]) and the loop is wired to it; the real
//!   `ModelClient::stream` only fires when a live key is configured, so tests
//!   never touch the network (they assert real-driver *construction* offline and
//!   drive the loop only via the fake/scripted path).
//!
//! ## Phase-E seams (NOT yet wired)
//! The agent crate has no production [`TurnState`]/[`TurnObserver`] for the live
//! turn loop yet (only `compact::CompactingTurnState`, which needs a
//! `ContextManager` + a `CompactionSampler`, and the test fakes). This facade
//! provides minimal store-backed impls so it can actually drive a run today; the
//! richer `ContextManager`-backed `TurnState` (token accounting, mid-turn
//! compaction, pending steer queue), tools/dispatch fusion, goals, hooks,
//! guardian, skills and personality context all remain Phase-E work. Each seam is
//! marked inline with `// Phase-E seam:`.

pub mod provider;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use browser_use_llm::schema::{ContentPart, Message, MessageRole};
use browser_use_protocol::{EventRecord, SessionStatus};
use browser_use_store::Store;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::compact::{
    compacted_history_from_summary, compaction_prompt_item, compaction_request_messages,
    is_summary_message, CompactionSampler, CompactionSummary, COMPACT_USER_MESSAGE_MAX_TOKENS,
};
use crate::config_overrides::{model_context_metadata_for_model, ProviderRunConfig};
use crate::context::accounting::TokenUsage;
use crate::context::assembly::TruncationPolicy;
use crate::context::workspace_context::{
    append_environment_context_event, append_workspace_context_event,
};
use crate::context::{
    typed_user_input_payload_from_items_for_cwd, typed_user_input_payload_from_text_for_cwd,
    ContextManager, Item,
};
use crate::decision::{AutoCompactTokenLimitScope, SamplingOutcome, TokenStatus};
use crate::events::{names, session_done_payload, EventSink, PendingEvent, TurnCtx};
use crate::session::reconstruct::WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND;
use crate::session::SessionId;
use crate::session::SharedStore;
use crate::session::{initial_context_messages_from_events, provider_messages_from_events};
use crate::subagents::display_agent_path_for_session;
use crate::task::TurnLifecycleEvent;
use crate::turn::sampling::FusionRecorder;
use crate::turn::CompactionMode;
use crate::turn::SamplingDriver;
use crate::turn::TurnLoop;
use crate::turn::TurnObserver;
use crate::turn::TurnState;
use crate::AgentError;

use provider::ResolvedProvider;
pub use provider::{
    cleanup_all_unified_exec_managers, cleanup_unified_exec_manager_for_session_id,
};

/// The shared, in-run conversation buffer that BOTH the loop's [`TurnState`] reads
/// (via [`StoreTurnState::clone_history_for_prompt`]) AND the fused driver's
/// [`FusionRecorder`] writes (the assistant message + dispatched tool outputs).
///
/// This is the load-bearing fusion seam: holding the same `Arc<Mutex<Vec<Message>>>`
/// behind both the recorder and the state means a tool output the driver dispatches
/// re-enters the very next prompt the loop samples (codex `try_run_turn` records the
/// call + its output into history before re-sampling).
type RecordedBuffer = Arc<Mutex<Vec<Message>>>;

/// codex `Provider::DEFAULT_STREAM_MAX_RETRIES`. Used when the run config does
/// not carry a `stream_max_retries` of its own.
///
/// Phase-E seam: the legacy run path threads `AgentConfig::stream_max_retries`
/// here; the full `AgentConfig` is not plumbed into [`ProviderRunConfig`] yet, so
/// the facade uses codex's default budget. Wave-E threads the real value through.
const DEFAULT_STREAM_MAX_RETRIES: u32 = 5;
const SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT: &str = "session.followup.pending";
const SESSION_ACTIVE_FOLLOWUP_INTERRUPTED_EVENT: &str = "session.followup.interrupt_sent";
const SESSION_ACTIVE_FOLLOWUP_CANCELLED_EVENT: &str = "session.followup.cancelled";
const AGENT_TURN_QUEUE_DRAINED_EVENT: &str = "agent.turn_queue_drained";
const FOLLOWUP_DELIVERY_AFTER_NEXT_TOOL_CALL: &str = "after_next_tool_call";

#[derive(Clone, Copy, Debug)]
struct AutoCompactWindow {
    ordinal: u64,
    prefill_input_tokens: Option<AutoCompactWindowPrefill>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoCompactWindowPrefill {
    ServerObserved(i64),
    Estimated(i64),
}

impl Default for AutoCompactWindow {
    fn default() -> Self {
        Self {
            ordinal: 1,
            prefill_input_tokens: None,
        }
    }
}

impl AutoCompactWindow {
    fn start_next(&mut self) {
        self.ordinal = self.ordinal.saturating_add(1);
        self.prefill_input_tokens = None;
    }

    fn set_estimated_prefill(&mut self, tokens: i64) {
        if matches!(
            self.prefill_input_tokens,
            Some(AutoCompactWindowPrefill::ServerObserved(_))
        ) {
            return;
        }
        self.prefill_input_tokens = Some(AutoCompactWindowPrefill::Estimated(tokens.max(0)));
    }

    fn ensure_server_observed_prefill_from_usage(&mut self, usage: &TokenUsage) {
        if matches!(
            self.prefill_input_tokens,
            Some(AutoCompactWindowPrefill::ServerObserved(_))
        ) {
            return;
        }
        self.prefill_input_tokens =
            Some(AutoCompactWindowPrefill::ServerObserved(usage.input.max(0)));
    }

    fn prefill_input_tokens(&self) -> Option<i64> {
        match self.prefill_input_tokens {
            Some(AutoCompactWindowPrefill::ServerObserved(tokens))
            | Some(AutoCompactWindowPrefill::Estimated(tokens)) => Some(tokens),
            None => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ActivePromptTokens {
    active: i64,
    provider_usage: Option<TokenUsage>,
}

#[derive(Clone)]
struct PreviousModelCompaction {
    model: String,
    model_context_window: i64,
    sampler: Arc<dyn DynCompactionSampler>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailboxDeliveryPhase {
    CurrentTurn,
    NextTurn,
}

/// A [`TurnState`] backed by the session's durable event log, with REAL codex-
/// parity token accounting + model-based compaction.
///
/// It lowers the durable history (reduced to provider messages, then lowered to
/// typed [`Message`]s through the pure [`ContextManager`]) into each turn's prompt,
/// and records assistant turns + dispatched tool outputs back into an in-memory
/// buffer (the fusion seam) for the rest of the run.
///
/// ## Token accounting + compaction (this WP)
/// - [`token_status`](TurnState::token_status) prefers provider-reported token
///   usage from the latest post-compaction `token_count`/`model.usage` event,
///   then adds a local estimate for items appended after that model response
///   (`ContextManager::total_token_usage`, codex `get_total_token_usage` shape).
///   If a provider omits usage or no model response has completed yet, it falls
///   back to the whole-prompt byte estimate (`bytes.div_ceil(4)` per item).
///   The resulting active usage is compared to the 90%-of-window auto-compact
///   limit ([`TokenStatus::from_usage`]).
/// - [`compact`](TurnState::compact) runs the model-based no-tools summary pass
///   ([`run_compaction`]) and INSTALLS the codex-parity `[preserved recent user
///   messages] + [PREFIX + summary]` as a `compacted` override that REPLACES the
///   durable-log prompt for the rest of the run. The recorded fusion buffer is
///   cleared (its content is now folded into the summary), so the next prompt is
///   small again and the loop continues.
///
/// Pending input is store-backed: active follow-ups are committed into the
/// durable log, while inter-agent mailbox messages are delivered through a
/// Codex-style current-turn/next-turn phase gate.
struct StoreTurnState {
    store: SharedStore,
    session_id: SessionId,
    /// Assistant turns + dispatched tool outputs recorded this run, so a follow-up
    /// prompt sees them. Shared (`Arc`) with the fused driver's [`FusionRecorder`]
    /// ([`BufferRecorder`]) so what the driver dispatches re-enters the next prompt.
    recorded: RecordedBuffer,
    /// The effective model context-window budget (tokens). Codex applies
    /// `effective_context_window_percent` (95% by default) before using this as a
    /// full-window guard.
    context_window_tokens: i64,
    model_auto_compact_token_limit: Option<i64>,
    auto_compact_scope: AutoCompactTokenLimitScope,
    auto_compact_window: Mutex<AutoCompactWindow>,
    mailbox_delivery_phase: Mutex<MailboxDeliveryPhase>,
    pre_turn_replay_from_seq: Mutex<Option<i64>>,
    compact_prompt: String,
    base_instructions: String,
    current_model: Option<String>,
    previous_model_compaction: Option<PreviousModelCompaction>,
    cancel: CancellationToken,
    /// The model-based summary pass for [`compact`](TurnState::compact). `None`
    /// disables compaction (the no-sampler / `Fake` path); the production run sets
    /// a real [`EntrypointSampler`].
    compaction_sampler: Option<Arc<dyn DynCompactionSampler>>,
    /// Once compaction has run, the compacted replacement history (codex
    /// `[preserved users] + [PREFIX + summary]`, lowered to typed messages). When
    /// `Some`, it REPLACES the durable-log prompt; later recorded turns are
    /// appended after it. `None` until the first compaction.
    compacted: Mutex<Option<Vec<Message>>>,
}

impl StoreTurnState {
    /// Build the state over a SHARED recorded buffer. The same `Arc` is handed to
    /// the fused driver's recorder (so dispatched tool outputs land here and are
    /// re-sampled on the next iteration) and to this state (which reads it into
    /// every prompt). Pass a fresh buffer for the non-fused (`Fake`) path.
    ///
    /// Compaction is OFF by default (no sampler, `0` window). Enable it with
    /// [`with_compaction`](StoreTurnState::with_compaction).
    fn new(store: SharedStore, session_id: SessionId, recorded: RecordedBuffer) -> Self {
        Self {
            store,
            session_id,
            recorded,
            context_window_tokens: 0,
            model_auto_compact_token_limit: None,
            auto_compact_scope: AutoCompactTokenLimitScope::Total,
            auto_compact_window: Mutex::new(AutoCompactWindow::default()),
            mailbox_delivery_phase: Mutex::new(MailboxDeliveryPhase::CurrentTurn),
            pre_turn_replay_from_seq: Mutex::new(None),
            compact_prompt: crate::compact::SUMMARIZATION_PROMPT.to_string(),
            base_instructions: crate::prompts::browser_agent_system_prompt(),
            current_model: None,
            previous_model_compaction: None,
            cancel: CancellationToken::new(),
            compaction_sampler: None,
            compacted: Mutex::new(None),
        }
    }

    /// Enable REAL token accounting + model-based compaction against a context
    /// window, driven by `sampler` for the no-tools summary pass.
    fn with_compaction(
        mut self,
        context_window_tokens: i64,
        configured_limit: Option<i64>,
        scope: AutoCompactTokenLimitScope,
        sampler: Arc<dyn DynCompactionSampler>,
    ) -> Self {
        self.context_window_tokens = context_window_tokens;
        self.model_auto_compact_token_limit = configured_limit;
        self.auto_compact_scope = scope;
        self.compaction_sampler = Some(sampler);
        self
    }

    fn with_compaction_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.compact_prompt = prompt.into();
        self
    }

    fn with_compaction_instructions(
        mut self,
        base_instructions: impl Into<String>,
        _developer_instructions: Option<String>,
    ) -> Self {
        self.base_instructions = base_instructions.into();
        self
    }

    fn with_current_model(mut self, model: impl Into<String>) -> Self {
        self.current_model = Some(model.into());
        self
    }

    fn with_previous_model_compaction(mut self, previous: Option<PreviousModelCompaction>) -> Self {
        self.previous_model_compaction = previous;
        self
    }

    fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Assemble the current prompt as typed [`Message`]s (synchronously). The base
    /// is the compacted override when present, else the lowered durable log; this
    /// run's recorded turns are appended.
    fn assemble_prompt_blocking(&self) -> Vec<Message> {
        let mut msgs = match self.compacted.lock().unwrap().as_ref() {
            Some(compacted) => compacted.clone(),
            None => {
                let session_id = self.session_id.as_str().to_string();
                history_from_store(&self.store, &session_id)
            }
        };
        msgs.extend(self.recorded.lock().unwrap().iter().cloned());
        msgs
    }

    fn current_prompt_items_for_compaction(&self, mode: CompactionMode) -> Vec<Item> {
        let events = events_from_store(&self.store, self.session_id.as_str());
        match mode {
            CompactionMode::PreTurn => {
                let replay_from = *self.pre_turn_replay_from_seq.lock().unwrap();
                let events = events_through_seq(&events, replay_from);
                self.current_prompt_items_from_events(&events)
            }
            CompactionMode::MidTurn => self.current_prompt_items_from_events(&events),
        }
    }

    fn current_prompt_items_from_events(
        &self,
        events: &[browser_use_protocol::EventRecord],
    ) -> Vec<Item> {
        let mut items = provider_messages_from_events(events);
        items.extend(
            self.recorded
                .lock()
                .unwrap()
                .iter()
                .map(message_to_provider_item),
        );
        items
    }

    /// Estimate the current prompt's tokens via a fresh [`ContextManager`] over
    /// the lowered prompt items (`bytes.div_ceil(4)` per item).
    fn estimate_prompt_tokens_from_items(&self, items: Vec<Item>) -> i64 {
        let mut mgr = ContextManager::new();
        mgr.record_items(items, TruncationPolicy::Bytes(usize::MAX));
        mgr.estimate_total_tokens()
    }

    /// Active context usage for compaction:
    ///
    /// - provider-reported latest response usage + locally estimated tail after
    ///   the last model-generated item, when a non-zero usage record exists after
    ///   the latest compaction checkpoint;
    /// - otherwise the whole-prompt local byte/token estimate.
    fn active_prompt_tokens_for_status(&self) -> ActivePromptTokens {
        let events = events_from_store(&self.store, self.session_id.as_str());
        let replay_from = *self.pre_turn_replay_from_seq.lock().unwrap();
        let events = events_through_seq(&events, replay_from);
        self.active_prompt_tokens_from_events(&events)
    }

    fn active_prompt_tokens_from_events(
        &self,
        events: &[browser_use_protocol::EventRecord],
    ) -> ActivePromptTokens {
        let items = self.current_prompt_items_from_events(events);

        if let Some(usage) = latest_provider_token_usage_after_compaction(events) {
            let mut mgr = ContextManager::new();
            mgr.record_items(items, TruncationPolicy::Bytes(usize::MAX));
            mgr.update_token_info(&usage, self.context_window());
            let active = mgr.total_token_usage(true);
            if active > 0 {
                return ActivePromptTokens {
                    active,
                    provider_usage: Some(usage),
                };
            }
            return ActivePromptTokens {
                active: mgr.estimate_total_tokens(),
                provider_usage: None,
            };
        }

        ActivePromptTokens {
            active: self.estimate_prompt_tokens_from_items(items),
            provider_usage: None,
        }
    }

    fn context_window(&self) -> Option<i64> {
        (self.context_window_tokens > 0).then_some(self.context_window_tokens)
    }

    fn token_status_blocking(&self) -> TokenStatus {
        if self.compaction_sampler.is_none() {
            return TokenStatus::default();
        }
        let tokens = self.active_prompt_tokens_for_status();
        let prefill = {
            let mut window = self.auto_compact_window.lock().unwrap();
            if tokens.provider_usage.is_some()
                && matches!(
                    self.auto_compact_scope,
                    AutoCompactTokenLimitScope::BodyAfterPrefix
                )
            {
                let usage = tokens.provider_usage.unwrap();
                window.ensure_server_observed_prefill_from_usage(&usage);
            }
            window.prefill_input_tokens()
        };
        TokenStatus::from_codex_usage(
            tokens.active,
            prefill,
            self.model_auto_compact_token_limit,
            self.context_window(),
            self.auto_compact_scope,
        )
    }

    fn should_run_previous_model_downshift_compaction(&self) -> bool {
        let Some(previous) = &self.previous_model_compaction else {
            return false;
        };
        let Some(current_model) = self.current_model.as_deref() else {
            return false;
        };
        if previous.model == current_model {
            return false;
        }
        let Some(new_context_window) = self.context_window() else {
            return false;
        };
        if previous.model_context_window <= new_context_window {
            return false;
        }

        let active_context_tokens = self.active_prompt_tokens_for_status().active;
        match self.auto_compact_scope {
            AutoCompactTokenLimitScope::Total => {
                active_context_tokens > self.model_auto_compact_token_limit.unwrap_or(i64::MAX)
                    || active_context_tokens >= new_context_window
            }
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                active_context_tokens >= new_context_window
            }
        }
    }

    async fn compact_previous_model_downshift_if_needed(&self) -> Result<bool, AgentError> {
        if !self.should_run_previous_model_downshift_compaction() {
            return Ok(false);
        }
        let previous = self
            .previous_model_compaction
            .as_ref()
            .expect("checked previous model compaction")
            .clone();
        self.compact_with_sampler(CompactionMode::PreTurn, previous.sampler)
            .await?;
        Ok(true)
    }
}

/// A [`FusionRecorder`] that appends into a shared [`RecordedBuffer`].
///
/// The fused [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver)
/// records the assistant message and each dispatched tool output through this; the
/// same `Arc<Mutex<Vec<Message>>>` backs the run's [`StoreTurnState`], so those
/// recorded items are exactly what the loop re-samples on its next iteration
/// (mirrors the test fakes in `turn/fusion_tests.rs`, where one `SharedConversation`
/// is both the `TurnState` and the `FusionRecorder`).
struct BufferRecorder {
    buffer: RecordedBuffer,
}

#[async_trait::async_trait]
impl FusionRecorder for BufferRecorder {
    async fn record(&self, messages: &[Message]) {
        self.buffer.lock().unwrap().extend_from_slice(messages);
    }
}

/// The live [`CompactionSampler`]: drives the real [`ModelClient`] for the
/// no-tools summary pass (codex's compact task streams `OutputItemDone`/`Completed`
/// only and never dispatches tools).
///
/// It builds a tool-free [`LlmRequest`] from the conversation-so-far + the
/// summarization prompt (already appended by [`run_compaction`]), opens the model
/// stream, and concatenates the assistant `TextDelta`s as the summary text. This
/// is the production analogue of the scripted sampler the compaction tests inject;
/// it shares the run's model + route, so the summary uses the same model as the
/// conversation (codex parity).
struct EntrypointSampler {
    client: Arc<browser_use_llm::route::ModelClient>,
    route: browser_use_llm::route::Route,
    model: String,
    provider: String,
    base_instructions: String,
}

impl CompactionSampler for EntrypointSampler {
    async fn summarize(
        &self,
        request: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<CompactionSummary, AgentError> {
        use browser_use_llm::schema::LlmEvent;
        use futures_util::StreamExt;

        // Tool-free request (tools deliberately empty — the summary pass must not
        // call tools). `request` already carries the history + the summarization
        // prompt as its final user message (assembled by `run_compaction`).
        let req = build_summary_llm_request(
            &self.model,
            &self.provider,
            &self.base_instructions,
            request,
        );

        // Open the model stream. `ModelClient::stream` is async; the entrypoint
        // runs on the multi-thread runtime, so we await it directly here.
        let mut stream = match self.client.stream(&self.route, &req).await {
            Ok(s) => s,
            Err(e) => return Err(map_summary_error(&e)),
        };

        // Concatenate assistant text. Tool calls (if any) are ignored — the compact
        // pass never dispatches. Cancellation aborts the pass.
        let mut summary = String::new();
        let mut token_usage = None;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err(AgentError::TurnAborted),
                ev = stream.next() => ev,
            };
            match next {
                Some(Ok(LlmEvent::TextDelta { delta, .. })) => summary.push_str(&delta),
                Some(Ok(LlmEvent::StepFinish { usage, .. }))
                | Some(Ok(LlmEvent::Finish { usage, .. })) => {
                    let usage = TokenUsage::from_llm_usage(&usage);
                    if usage.total > 0 {
                        token_usage = Some(usage);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(map_summary_error(&e)),
                None => break,
            }
        }
        Ok(CompactionSummary {
            text: summary,
            token_usage,
        })
    }
}

fn build_summary_llm_request(
    model: &str,
    provider: &str,
    base_instructions: &str,
    request: Vec<Message>,
) -> browser_use_llm::schema::LlmRequest {
    use browser_use_llm::schema::{LlmRequest, SystemPart};

    let mut req = LlmRequest::new(model.to_string(), provider.to_string());
    req.system
        .push(SystemPart::new(base_instructions.to_string()));
    req.messages.extend(request);
    req
}

/// Object-safe ("dyn-compatible") view of [`CompactionSampler`].
///
/// [`CompactionSampler::summarize`] returns `impl Future` (native RPITIT), which
/// is NOT object-safe, so `Arc<dyn CompactionSampler>` is impossible. The
/// [`StoreTurnState`] holds the summary pass behind a trait object (it is built in
/// one of two branches — real vs. disabled — so a generic would fan out through
/// `drive_run`), so this boxes the future to make it storable. A blanket impl
/// makes every concrete [`CompactionSampler`] usable here, and a forwarding
/// [`CompactionSampler`] impl on `dyn DynCompactionSampler` lets the trait object
/// feed straight back into [`run_compaction`]'s `S: CompactionSampler + ?Sized`.
trait DynCompactionSampler: Send + Sync {
    fn summarize_boxed(
        &self,
        request: Vec<Message>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CompactionSummary, AgentError>> + Send + '_>>;
}

impl<T: CompactionSampler> DynCompactionSampler for T {
    fn summarize_boxed(
        &self,
        request: Vec<Message>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CompactionSummary, AgentError>> + Send + '_>> {
        Box::pin(self.summarize(request, cancel))
    }
}

impl CompactionSampler for dyn DynCompactionSampler + '_ {
    fn summarize(
        &self,
        request: Vec<Message>,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
        self.summarize_boxed(request, cancel)
    }
}

/// Map an [`LlmError`] from the summary pass to an [`AgentError`]. A
/// context-window-exceeded condition becomes [`AgentError::ContextWindowExceeded`]
/// so [`run_compaction`] can drop the oldest item and retry (codex
/// `remove_first_item` loop); everything else becomes a provider error.
fn map_summary_error(e: &browser_use_llm::schema::LlmError) -> AgentError {
    use browser_use_llm::schema::LlmErrorReason;
    let looks_like_window = e.reason == LlmErrorReason::InvalidRequest && {
        let m = e.message.to_ascii_lowercase();
        m.contains("context") && m.contains("window")
    };
    if looks_like_window {
        AgentError::ContextWindowExceeded
    } else {
        AgentError::Provider(e.to_string())
    }
}

/// Build the live [`EntrypointSampler`] for a real backend, reusing the same
/// offline route construction the main driver uses
/// ([`provider::provider_choice_for_backend`] + [`build_route`](crate::turn::build_route)).
/// Returns `None` when the backend has no real provider (Fake) or credentials
/// cannot be resolved from the same env/store sources as the main driver. No
/// network I/O.
fn build_compaction_sampler(
    config: &ProviderRunConfig,
    store: Option<&Store>,
) -> Option<Arc<dyn DynCompactionSampler>> {
    let choice = provider::provider_choice_for_backend(config.backend, store).ok()??;
    let route = crate::turn::build_route(&choice, &config.model).ok()?;
    let client = Arc::new(browser_use_llm::route::ModelClient::default());
    Some(Arc::new(EntrypointSampler {
        client,
        route,
        model: config.model.clone(),
        provider: format!("{:?}", config.backend).to_ascii_lowercase(),
        base_instructions: base_instructions_for_config(config),
    }))
}

fn base_instructions_for_config(config: &ProviderRunConfig) -> String {
    config
        .options
        .base_instructions
        .clone()
        .unwrap_or_else(|| crate::prompts::browser_agent_system_prompt())
}

fn compact_prompt_for_config(config: &ProviderRunConfig) -> String {
    config
        .options
        .compact_prompt
        .clone()
        .or_else(|| config_override_string(&config.options.config_overrides, "compact_prompt"))
        .unwrap_or_else(|| crate::compact::SUMMARIZATION_PROMPT.to_string())
}

fn config_override_string(overrides: &[(String, toml::Value)], key: &str) -> Option<String> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.as_str().map(str::to_string))
}

fn config_for_model(config: &ProviderRunConfig, model: &str) -> ProviderRunConfig {
    let mut config = config.clone();
    config.model = model.to_string();
    let metadata = model_context_metadata_for_model(model);
    if let Some(context_window) = metadata.resolved_context_window() {
        config.context_window_tokens = usize::try_from(context_window).unwrap_or(usize::MAX);
    }
    config
}

fn effective_context_window_for_config(config: &ProviderRunConfig) -> Option<i64> {
    config
        .model_context_metadata()
        .effective_context_window()
        .filter(|window| *window > 0)
}

fn previous_model_compaction_for_config(
    store: &SharedStore,
    session_id: &SessionId,
    config: &ProviderRunConfig,
) -> Option<PreviousModelCompaction> {
    let events = events_from_store(store, session_id.as_str());
    let previous_model = latest_model_request_model(&events)?;
    if previous_model == config.model {
        return None;
    }
    let previous_config = config_for_model(config, &previous_model);
    let previous_context_window = effective_context_window_for_config(&previous_config)?;
    let sampler = {
        let store_guard = store.lock().expect("store mutex poisoned");
        build_compaction_sampler(&previous_config, Some(&store_guard))?
    };
    Some(PreviousModelCompaction {
        model: previous_model,
        model_context_window: previous_context_window,
        sampler,
    })
}

/// Lower a session's durable event log into typed prompt messages: blocking store
/// read + pure reduce ([`provider_messages_from_events`]) + pure lower
/// ([`ContextManager::lower_to_messages`]). Runs synchronously; the caller wraps
/// it in `spawn_blocking` to keep it off the async runtime.
fn history_from_store(store: &SharedStore, session_id: &str) -> Vec<Message> {
    let events = {
        let store = store.lock().expect("store mutex poisoned");
        store.events_for_session(session_id).unwrap_or_default()
    };
    // Pure reduce: durable events -> provider messages (the legacy currency).
    let items = provider_messages_from_events(&events);
    // Pure lower: provider-message Values -> typed Messages for the request.
    ContextManager::new().lower_to_messages(&items)
}

fn followup_delivery_is_after_next_tool_call(event: &EventRecord) -> bool {
    matches!(
        event.event_type.as_str(),
        names::SESSION_FOLLOWUP | SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT
    ) && event
        .payload
        .get("delivery")
        .and_then(serde_json::Value::as_str)
        == Some(FOLLOWUP_DELIVERY_AFTER_NEXT_TOOL_CALL)
}

fn user_input_payload_to_messages(payload: &serde_json::Value) -> Vec<Message> {
    let mut items = Vec::new();
    if let Some(messages) = payload
        .get("skill_context_messages")
        .and_then(serde_json::Value::as_array)
    {
        items.extend(messages.iter().cloned());
    }
    if let Some(messages) = payload
        .get("mention_context_messages")
        .and_then(serde_json::Value::as_array)
    {
        items.extend(messages.iter().cloned());
    }
    if let Some(content) = payload.get("content") {
        items.push(serde_json::json!({
            "role": "user",
            "content": content,
        }));
    } else if let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) {
        items.push(serde_json::json!({
            "role": "user",
            "content": text,
        }));
    }
    ContextManager::new().lower_to_messages(&items)
}

fn followup_marker_seqs(event: &EventRecord) -> Vec<i64> {
    if let Some(seq) = event
        .payload
        .get("followup_seq")
        .or_else(|| event.payload.get("seq"))
        .and_then(serde_json::Value::as_i64)
    {
        return vec![seq];
    }
    event
        .payload
        .get("followup_seqs")
        .and_then(serde_json::Value::as_array)
        .map(|seqs| {
            seqs.iter()
                .filter_map(serde_json::Value::as_i64)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn event_closes_after_next_tool_call_followup(event: &EventRecord, followup_seq: i64) -> bool {
    if event.seq <= followup_seq {
        return false;
    }
    match event.event_type.as_str() {
        AGENT_TURN_QUEUE_DRAINED_EVENT => {
            let drained_session_messages = event
                .payload
                .get("session_messages")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            let last_seq = event
                .payload
                .get("last_seq")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default();
            drained_session_messages > 0 && last_seq >= followup_seq
        }
        SESSION_ACTIVE_FOLLOWUP_INTERRUPTED_EVENT | SESSION_ACTIVE_FOLLOWUP_CANCELLED_EVENT => {
            followup_marker_seqs(event).contains(&followup_seq)
        }
        _ => false,
    }
}

fn pending_after_next_tool_call_followup(events: &[EventRecord], followup: &EventRecord) -> bool {
    followup_delivery_is_after_next_tool_call(followup)
        && !events
            .iter()
            .any(|event| event_closes_after_next_tool_call_followup(event, followup.seq))
}

fn next_pending_active_followup(events: &[EventRecord]) -> Option<&EventRecord> {
    events
        .iter()
        .filter(|event| event.event_type == SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT)
        .filter(|event| event.payload.get("pending_from_seq").is_none())
        .filter(|event| pending_after_next_tool_call_followup(events, event))
        .min_by_key(|event| event.seq)
}

fn has_pending_active_followup(store: &SharedStore, session_id: &str) -> bool {
    let events = {
        let Ok(store) = store.lock() else {
            return false;
        };
        store.events_for_session(session_id).unwrap_or_default()
    };
    next_pending_active_followup(&events).is_some()
}

fn drain_one_pending_active_followup(store: &SharedStore, session_id: &str) {
    let Ok(store) = store.lock() else {
        return;
    };
    let Ok(events) = store.events_for_session(session_id) else {
        return;
    };
    let Some(pending) = next_pending_active_followup(&events) else {
        return;
    };
    let pending_seq = pending.seq;
    let mut payload = pending.payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("delivery");
        obj.insert(
            "pending_from_seq".to_string(),
            serde_json::Value::from(pending_seq),
        );
    }
    let Ok(committed) = store.append_event(session_id, names::SESSION_FOLLOWUP, payload) else {
        return;
    };
    let _ = store.append_event(
        session_id,
        AGENT_TURN_QUEUE_DRAINED_EVENT,
        serde_json::json!({
            "phase": "after_tool_outputs",
            "session_messages": 1,
            "mailbox_messages": 0,
            "last_seq": committed.seq,
        }),
    );
    let _ = store.append_event(
        session_id,
        "model.response.continued",
        serde_json::json!({
            "reason": "active_turn_queue_drained",
            "phase": "after_tool_outputs",
            "session_messages": 1,
            "mailbox_messages": 0,
        }),
    );
}

fn has_pending_agent_mail(store: &SharedStore, session_id: &str) -> bool {
    let Ok(store) = store.lock() else {
        return false;
    };
    store
        .messages_for_agent(session_id)
        .map(|messages| !messages.is_empty())
        .unwrap_or(false)
}

fn has_pending_trigger_turn_agent_mail(store: &SharedStore, session_id: &str) -> bool {
    let Ok(store) = store.lock() else {
        return false;
    };
    store
        .messages_for_agent(session_id)
        .map(|messages| messages.iter().any(|message| message.trigger_turn))
        .unwrap_or(false)
}

fn drain_agent_mailbox_as_pending_input(store: &SharedStore, session_id: &str) -> Vec<Message> {
    let Ok(store) = store.lock() else {
        return Vec::new();
    };
    let messages = store
        .drain_agent_messages_for_agent(session_id)
        .unwrap_or_default();
    messages
        .into_iter()
        .flat_map(|message| {
            let author_path = display_agent_path_for_session(&store, &message.author_session_id)
                .unwrap_or_else(|_| "/root".to_string());
            let recipient_path = display_agent_path_for_session(&store, &message.target_session_id)
                .unwrap_or_else(|_| "/root".to_string());
            let content = message.content.clone();
            let trigger_turn = message.trigger_turn;
            if message.input_kind == "user_input" {
                let cwd = store
                    .load_session(&message.target_session_id)
                    .ok()
                    .flatten()
                    .map(|session| session.cwd)
                    .unwrap_or_else(|| ".".to_string());
                let payload = if let Some(items) = message.input_items.as_ref() {
                    typed_user_input_payload_from_items_for_cwd(items, &cwd)
                } else {
                    typed_user_input_payload_from_text_for_cwd(&content, &cwd)
                };
                if let Ok(payload) = payload {
                    let _ = store.append_event(
                        &message.target_session_id,
                        names::SESSION_FOLLOWUP,
                        payload.clone(),
                    );
                    return user_input_payload_to_messages(&payload);
                }
            } else {
                let _ = store.append_event(
                    &message.target_session_id,
                    "agent.mailbox_input",
                    serde_json::json!({
                        "id": message.id,
                        "author_session_id": message.author_session_id,
                        "target_session_id": message.target_session_id,
                        "author_path": author_path,
                        "recipient_path": recipient_path,
                        "content": content,
                        "trigger_turn": trigger_turn,
                    }),
                );
            }
            let envelope = serde_json::json!({
                "author": author_path,
                "recipient": recipient_path,
                "other_recipients": [],
                "content": content,
                "trigger_turn": trigger_turn,
            });
            vec![Message::new(
                MessageRole::Assistant,
                vec![ContentPart::text(envelope.to_string())],
            )]
        })
        .collect()
}

fn ensure_fallback_capture_recording(store: &SharedStore, session_id: &str) {
    let Ok(store) = store.lock() else {
        return;
    };
    let Ok(events) = store.events_for_session(session_id) else {
        return;
    };
    if events
        .iter()
        .any(|event| event.event_type == "capture.curation")
    {
        return;
    }
    let Ok(Some(session)) = store.load_session(session_id) else {
        return;
    };
    match browser_use_browser::build_uncurated_summary_gif(std::path::Path::new(
        &session.artifact_root,
    )) {
        Ok(Some(gif_path)) => {
            let _ = store.append_event(
                session_id,
                "capture.curation",
                serde_json::json!({
                    "source": "fallback_uncurated",
                    "gif_path": gif_path.display().to_string(),
                }),
            );
            let _ = crate::infra::persistence::record_tool_artifact(
                &store,
                session_id,
                "capture",
                &serde_json::json!({
                    "path": gif_path.display().to_string(),
                    "kind": "summary_gif",
                    "mime": "image/gif",
                }),
            );
        }
        Ok(None) => {}
        Err(error) => {
            let _ = store.append_event(
                session_id,
                "capture.recording_failed",
                serde_json::json!({ "error": format!("{error:#}") }),
            );
        }
    }
}

fn events_from_store(
    store: &SharedStore,
    session_id: &str,
) -> Vec<browser_use_protocol::EventRecord> {
    store
        .lock()
        .expect("store mutex poisoned")
        .events_for_session(session_id)
        .unwrap_or_default()
}

fn events_through_seq(
    events: &[browser_use_protocol::EventRecord],
    through_seq: Option<i64>,
) -> Vec<browser_use_protocol::EventRecord> {
    match through_seq {
        Some(seq) => events
            .iter()
            .filter(|event| event.seq <= seq)
            .cloned()
            .collect(),
        None => events.to_vec(),
    }
}

fn latest_real_user_event_seq(events: &[browser_use_protocol::EventRecord]) -> Option<i64> {
    events.iter().rev().find_map(|event| {
        matches!(
            event.event_type.as_str(),
            names::SESSION_INPUT | names::SESSION_FOLLOWUP
        )
        .then_some(event.seq)
    })
}

fn latest_replay_seq_before_fresh_input(
    events: &[browser_use_protocol::EventRecord],
) -> Option<i64> {
    latest_real_user_event_seq(events).map(|seq| seq.saturating_sub(1))
}

fn latest_model_request_model(events: &[browser_use_protocol::EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        (event.event_type == names::MODEL_TURN_REQUEST)
            .then(|| event.payload.get("model").and_then(Value::as_str))
            .flatten()
            .map(str::to_string)
    })
}

fn latest_provider_token_usage_after_compaction(
    events: &[browser_use_protocol::EventRecord],
) -> Option<TokenUsage> {
    let latest_compaction_seq = events
        .iter()
        .rev()
        .find(|event| event.event_type == "session.compacted")
        .map(|event| event.seq)
        .unwrap_or(0);

    events
        .iter()
        .rev()
        .filter(|event| event.seq > latest_compaction_seq)
        .find_map(provider_token_usage_from_event)
}

fn provider_token_usage_from_event(
    event: &browser_use_protocol::EventRecord,
) -> Option<TokenUsage> {
    match event.event_type.as_str() {
        names::TOKEN_COUNT => event
            .payload
            .get("info")
            .and_then(|info| info.get("last_token_usage"))
            .and_then(token_usage_from_value),
        "model.usage" => event
            .payload
            .get("usage")
            .or(Some(&event.payload))
            .and_then(token_usage_from_value),
        _ => None,
    }
}

fn token_usage_from_value(value: &Value) -> Option<TokenUsage> {
    let input = value_i64_any(value, &["input_tokens"]).unwrap_or(0);
    let cached_input =
        value_i64_any(value, &["cached_input_tokens", "input_cached_tokens"]).unwrap_or(0);
    let output = value_i64_any(value, &["output_tokens"]).unwrap_or(0);
    let reasoning_output = value_i64_any(value, &["reasoning_output_tokens"]).unwrap_or(0);
    let total = value_i64_any(value, &["total_tokens"]).unwrap_or_else(|| {
        input
            .saturating_add(output)
            .saturating_add(reasoning_output)
    });

    let usage = TokenUsage {
        input,
        cached_input,
        output,
        reasoning_output,
        total,
    };
    (usage.input > 0
        || usage.cached_input > 0
        || usage.output > 0
        || usage.reasoning_output > 0
        || usage.total > 0)
        .then_some(usage)
}

fn value_i64_any(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(value_to_nonnegative_i64)
}

fn value_to_nonnegative_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .map(|n| n.max(0))
}

fn provider_item_text(item: &Item) -> String {
    let Some(content) = item.get("content") else {
        return String::new();
    };
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn is_real_user_provider_item(item: &Item) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && item.get("name").and_then(Value::as_str).is_none()
        && !is_summary_message(&provider_item_text(item))
}

fn insert_initial_context_before_last_real_user_or_summary(
    mut replacement: Vec<Item>,
    initial_context: Vec<Item>,
) -> Vec<Item> {
    if initial_context.is_empty() {
        return replacement;
    }
    let last_user_or_summary = replacement
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, item)| {
            (item.get("role").and_then(Value::as_str) == Some("user")).then_some(idx)
        });
    let last_real_user = replacement
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, item)| is_real_user_provider_item(item).then_some(idx));
    let insertion_index = last_real_user
        .or(last_user_or_summary)
        .unwrap_or(replacement.len());
    replacement.splice(insertion_index..insertion_index, initial_context);
    replacement
}

async fn run_compaction_with_retries(
    session_id: &SessionId,
    store: &SharedStore,
    history: &[Item],
    sampler: &dyn DynCompactionSampler,
    compact_prompt: &str,
    token_limit: usize,
    max_retries: u32,
    context_window: Option<i64>,
    cancel: CancellationToken,
) -> Result<crate::compact::CompactedHistory, AgentError> {
    let mut retries = 0;
    let mut working: Vec<Item> = history.to_vec();
    working.push(compaction_prompt_item(compact_prompt));
    loop {
        if cancel.is_cancelled() {
            return Err(AgentError::TurnAborted);
        }
        let request = compaction_request_messages(&working);
        let request_len = request.len();
        match sampler.summarize(request, cancel.clone()).await {
            Ok(summary) => {
                append_compaction_token_usage(
                    store,
                    session_id.as_str(),
                    summary.token_usage.as_ref(),
                    context_window,
                );
                return Ok(compacted_history_from_summary(
                    history,
                    summary,
                    token_limit,
                ));
            }
            Err(AgentError::ContextWindowExceeded) => {
                if request_len > 1 && working.len() > 1 {
                    working.remove(0);
                    retries = 0;
                    continue;
                }
                append_context_window_full(store, session_id.as_str(), context_window);
                append_model_turn_context_overflow(store, session_id.as_str());
                return Err(AgentError::ContextWindowExceeded);
            }
            Err(AgentError::TurnAborted) => return Err(AgentError::TurnAborted),
            Err(error) if error.is_retryable() && retries < max_retries => {
                retries += 1;
                append_stream_error(
                    store,
                    session_id.as_str(),
                    &format!("Reconnecting... {retries}/{max_retries}"),
                );
                let delay = tokio::time::sleep(std::time::Duration::from_millis(
                    crate::decision::backoff_ms(retries),
                ));
                tokio::select! {
                    _ = cancel.cancelled() => return Err(AgentError::TurnAborted),
                    _ = delay => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn append_model_turn_context_overflow(store: &SharedStore, session_id: &str) {
    if let Ok(store) = store.lock() {
        let _ = store.append_event(
            session_id,
            "model.turn.context_overflow",
            json!({ "action": "compaction_failed" }),
        );
        let _ = store.append_event(
            session_id,
            names::MODEL_TURN_ERROR,
            json!({
                "error": "context window exceeded while compacting",
                "code": "context_window_exceeded",
                "recoverable": false,
            }),
        );
    }
}

fn append_stream_error(store: &SharedStore, session_id: &str, message: &str) {
    if let Ok(store) = store.lock() {
        let _ = store.append_event(
            session_id,
            names::STREAM_ERROR,
            json!({ "message": message }),
        );
    }
}

fn append_compaction_started(store: &SharedStore, session_id: &str, mode: CompactionMode) {
    if let Ok(store) = store.lock() {
        let _ = store.append_event(
            session_id,
            "session.compaction_started",
            json!({
                "reason": "token_budget",
                "mode": compaction_mode_name(mode),
            }),
        );
    }
}

fn append_compaction_failed(
    store: &SharedStore,
    session_id: &str,
    mode: CompactionMode,
    error: &AgentError,
) {
    if let Ok(store) = store.lock() {
        let _ = store.append_event(
            session_id,
            "session.compaction_failed",
            json!({
                "reason": "token_budget",
                "mode": compaction_mode_name(mode),
                "error": error.to_string(),
            }),
        );
        if !matches!(error, AgentError::ContextWindowExceeded) {
            let _ = store.append_event(
                session_id,
                names::MODEL_TURN_ERROR,
                json!({
                    "error": error.to_string(),
                    "code": compaction_error_code(error),
                    "recoverable": false,
                }),
            );
        }
    }
}

fn compaction_mode_name(mode: CompactionMode) -> &'static str {
    match mode {
        CompactionMode::PreTurn => "pre_turn",
        CompactionMode::MidTurn => "mid_turn",
    }
}

fn compaction_error_code(error: &AgentError) -> &'static str {
    match error {
        AgentError::ContextWindowExceeded => "context_window_exceeded",
        AgentError::TurnAborted => "interrupted",
        AgentError::UsageLimitReached => "usage_limit_reached",
        _ => "compaction_failed",
    }
}

fn compacted_event_payload(
    summary_text: &str,
    replacement_messages: &[Item],
    initial_context_already_in_history: bool,
    replay_from_seq: Option<i64>,
) -> Value {
    let mut payload = json!({
        "message": summary_text,
        "replacement_messages": replacement_messages,
        "initial_context_already_in_history": initial_context_already_in_history,
    });
    if let Some(seq) = replay_from_seq {
        payload["replay_from_seq"] = json!(seq);
    }
    payload
}

fn recomputed_token_count_payload(
    tokens: i64,
    window: Option<i64>,
    previous_total: &Value,
) -> Value {
    let last_usage = token_usage_value_with_total(tokens);
    let total_usage = if previous_total.is_null() {
        token_usage_value_with_total(0)
    } else {
        previous_total.clone()
    };
    json!({
        "info": {
            "total_token_usage": total_usage,
            "last_token_usage": last_usage,
            "model_context_window": window,
        },
        "turn_idx": 0,
    })
}

fn token_usage_value(usage: &TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input.max(0),
        "cached_input_tokens": usage.cached_input.max(0),
        "output_tokens": usage.output.max(0),
        "reasoning_output_tokens": usage.reasoning_output.max(0),
        "total_tokens": usage.total.max(0),
    })
}

fn token_count_payload_from_usage(
    usage: &TokenUsage,
    window: Option<i64>,
    previous_total: &Value,
) -> Value {
    let last_usage = token_usage_value(usage);
    let total_usage = if previous_total.is_null() {
        last_usage.clone()
    } else {
        add_token_usage_values(previous_total, &last_usage)
    };
    json!({
        "info": {
            "total_token_usage": total_usage,
            "last_token_usage": last_usage,
            "model_context_window": window,
        },
        "turn_idx": 0,
    })
}

fn append_compaction_token_usage(
    store: &SharedStore,
    session_id: &str,
    usage: Option<&TokenUsage>,
    window: Option<i64>,
) {
    let Some(usage) = usage.filter(|usage| usage.total > 0) else {
        return;
    };
    if let Ok(store) = store.lock() {
        let events = store.events_for_session(session_id).unwrap_or_default();
        let previous_total = latest_total_token_usage_value(&events);
        let _ = store.append_event(
            session_id,
            names::TOKEN_COUNT,
            token_count_payload_from_usage(usage, window, &previous_total),
        );
    }
}

fn append_context_window_full(store: &SharedStore, session_id: &str, window: Option<i64>) {
    let Some(window) = window.filter(|window| *window > 0) else {
        return;
    };
    if let Ok(store) = store.lock() {
        let events = store.events_for_session(session_id).unwrap_or_default();
        let previous_total = latest_total_token_usage_value(&events);
        let previous_total_tokens = token_usage_total_tokens(&previous_total);
        let delta = window.saturating_sub(previous_total_tokens).max(0);
        let total_usage = token_usage_value_with_total(window);
        let last_usage = token_usage_value_with_total(delta);
        let _ = store.append_event(
            session_id,
            names::TOKEN_COUNT,
            json!({
                "info": {
                    "total_token_usage": total_usage,
                    "last_token_usage": last_usage,
                    "model_context_window": window,
                },
                "turn_idx": 0,
            }),
        );
    }
}

fn token_usage_value_with_total(total_tokens: i64) -> Value {
    json!({
        "input_tokens": 0,
        "cached_input_tokens": 0,
        "output_tokens": 0,
        "reasoning_output_tokens": 0,
        "total_tokens": total_tokens.max(0),
    })
}

fn latest_total_token_usage_value(events: &[browser_use_protocol::EventRecord]) -> Value {
    events
        .iter()
        .rev()
        .find(|event| event.event_type == names::TOKEN_COUNT)
        .and_then(|event| {
            event
                .payload
                .get("info")
                .and_then(|info| info.get("total_token_usage"))
        })
        .cloned()
        .unwrap_or(Value::Null)
}

fn token_usage_total_tokens(value: &Value) -> i64 {
    value
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .max(0)
}

fn add_token_usage_values(previous: &Value, addition: &Value) -> Value {
    let get = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    json!({
        "input_tokens": get(previous, "input_tokens") + get(addition, "input_tokens"),
        "cached_input_tokens": get(previous, "cached_input_tokens") + get(addition, "cached_input_tokens"),
        "output_tokens": get(previous, "output_tokens") + get(addition, "output_tokens"),
        "reasoning_output_tokens": get(previous, "reasoning_output_tokens") + get(addition, "reasoning_output_tokens"),
        "total_tokens": get(previous, "total_tokens") + get(addition, "total_tokens"),
    })
}

fn enrich_token_count_payload(
    mut payload: Value,
    previous_total: &Value,
    window: Option<i64>,
) -> Value {
    let Some(info) = payload.get_mut("info").and_then(Value::as_object_mut) else {
        return payload;
    };
    let Some(last_token_usage) = info.get("last_token_usage").cloned() else {
        return payload;
    };
    let total_token_usage = if previous_total.is_null() {
        last_token_usage.clone()
    } else {
        add_token_usage_values(previous_total, &last_token_usage)
    };
    info.insert("total_token_usage".to_string(), total_token_usage);
    if let Some(window) = window {
        info.insert("model_context_window".to_string(), json!(window));
    }
    payload
}

impl TurnState for StoreTurnState {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        // Once compacted, the prompt base is the compacted override (codex's
        // replaced history); otherwise it is the lowered durable log. The recorded
        // buffer (this run's assistant turns + the fused driver's dispatched tool
        // outputs) is appended either way, so tool outputs always re-enter the next
        // prompt (the fusion seam is preserved across compaction).
        if self.compacted.lock().unwrap().is_some() {
            return self.assemble_prompt_blocking();
        }
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        // The durable read is synchronous (rusqlite); run it off the async runtime.
        let mut msgs = tokio::task::spawn_blocking(move || history_from_store(&store, &session_id))
            .await
            .unwrap_or_default();
        // The recorded buffer carries this run's assistant turns AND the fused
        // driver's dispatched tool outputs (both append through the same `Arc`), so
        // the next prompt sees everything produced so far.
        msgs.extend(self.recorded.lock().unwrap().iter().cloned());
        msgs
    }

    async fn record_items(&self, items: &[Message]) {
        self.recorded.lock().unwrap().extend_from_slice(items);
    }

    async fn has_pending_input(&self) -> bool {
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        let mailbox_delivery_phase = *self.mailbox_delivery_phase.lock().unwrap();
        tokio::task::spawn_blocking(move || {
            has_pending_active_followup(&store, &session_id)
                || (mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
                    && has_pending_agent_mail(&store, &session_id))
        })
        .await
        .unwrap_or(false)
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        let store_for_followup = Arc::clone(&self.store);
        let session_id_for_followup = self.session_id.as_str().to_string();
        let followup_pending = tokio::task::spawn_blocking(move || {
            has_pending_active_followup(&store_for_followup, &session_id_for_followup)
        })
        .await
        .unwrap_or(false);
        if followup_pending {
            *self.mailbox_delivery_phase.lock().unwrap() = MailboxDeliveryPhase::CurrentTurn;
        }
        let mailbox_delivery_phase = *self.mailbox_delivery_phase.lock().unwrap();
        if !followup_pending && mailbox_delivery_phase == MailboxDeliveryPhase::NextTurn {
            return Vec::new();
        }
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            if followup_pending {
                drain_one_pending_active_followup(&store, &session_id);
            }
            drain_agent_mailbox_as_pending_input(&store, &session_id)
        })
        .await
        .unwrap_or_default()
    }

    async fn token_status(&self) -> TokenStatus {
        self.token_status_blocking()
    }

    async fn compact(&self, mode: CompactionMode) -> Result<(), AgentError> {
        // codex: ask the model to write a handoff summary, then REPLACE history
        // with `[preserved recent user messages] + [PREFIX + summary]`.
        let Some(sampler) = self.compaction_sampler.clone() else {
            return Ok(()); // compaction disabled — keep the loop's no-op default.
        };
        self.compact_with_sampler(mode, sampler).await
    }

    async fn defer_mailbox_delivery_to_next_turn(&self) {
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        let followup_pending =
            tokio::task::spawn_blocking(move || has_pending_active_followup(&store, &session_id))
                .await
                .unwrap_or(false);
        if !followup_pending {
            *self.mailbox_delivery_phase.lock().unwrap() = MailboxDeliveryPhase::NextTurn;
        }
    }
}

impl StoreTurnState {
    async fn compact_with_sampler(
        &self,
        mode: CompactionMode,
        sampler: Arc<dyn DynCompactionSampler>,
    ) -> Result<(), AgentError> {
        // Snapshot the current prompt as provider-message items (codex
        // `clone_history`): the durable log (or the prior compacted override) plus
        // this run's recorded turns.
        let items = self.current_prompt_items_for_compaction(mode);
        append_compaction_started(&self.store, self.session_id.as_str(), mode);

        // Model-based no-tools summary pass + codex-parity compacted-history
        // assembly (preserved recent user messages capped at 20k tokens + the
        // PREFIX'd summary). Failures propagate so the caller surfaces the same
        // compaction failure Codex would surface.
        let compacted = match run_compaction_with_retries(
            &self.session_id,
            &self.store,
            &items,
            sampler.as_ref(),
            &self.compact_prompt,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            DEFAULT_STREAM_MAX_RETRIES,
            self.context_window(),
            self.cancel.clone(),
        )
        .await
        {
            Ok(compacted) => compacted,
            Err(error) => {
                append_compaction_failed(&self.store, self.session_id.as_str(), mode, &error);
                return Err(error);
            }
        };

        let events = events_from_store(&self.store, self.session_id.as_str());
        let initial_context = initial_context_messages_from_events(&events, None, true, true);
        let initial_context_already_in_history = matches!(mode, CompactionMode::MidTurn);
        let replay_from_seq = match mode {
            CompactionMode::PreTurn => *self.pre_turn_replay_from_seq.lock().unwrap(),
            CompactionMode::MidTurn => None,
        };
        let replacement_messages = if initial_context_already_in_history {
            insert_initial_context_before_last_real_user_or_summary(
                compacted.items,
                initial_context,
            )
        } else {
            compacted.items
        };

        {
            let store = self.store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    self.session_id.as_str(),
                    "session.compacted",
                    compacted_event_payload(
                        &compacted.summary_text,
                        &replacement_messages,
                        initial_context_already_in_history,
                        replay_from_seq,
                    ),
                )
                .map_err(|error| AgentError::Store(error.into()))?;
        }

        let active_after_compact = {
            let lowered = history_from_store(&self.store, self.session_id.as_str());
            let items = lowered
                .iter()
                .map(message_to_provider_item)
                .collect::<Vec<_>>();
            let mut mgr = ContextManager::new();
            mgr.record_items(items, TruncationPolicy::Bytes(usize::MAX));
            *self.compacted.lock().unwrap() = Some(lowered);
            mgr.estimate_total_tokens()
                .saturating_add(crate::compact::approx_token_count(&self.base_instructions) as i64)
        };
        {
            let store = self.store.lock().expect("store mutex poisoned");
            let previous_total = latest_total_token_usage_value(
                &store
                    .events_for_session(self.session_id.as_str())
                    .unwrap_or_default(),
            );
            store
                .append_event(
                    self.session_id.as_str(),
                    names::TOKEN_COUNT,
                    recomputed_token_count_payload(
                        active_after_compact,
                        self.context_window(),
                        &previous_total,
                    ),
                )
                .map_err(|error| AgentError::Store(error.into()))?;
        }
        self.recorded.lock().unwrap().clear();
        let mut window = self.auto_compact_window.lock().unwrap();
        window.start_next();
        if matches!(
            self.auto_compact_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            window.set_estimated_prefill(active_after_compact);
        }
        Ok(())
    }
}

/// Lower a typed [`Message`] back to a provider-message [`Item`] (`Value`) for the
/// token estimate + the [`run_compaction`] input. Mirrors `compact::message_to_item`
/// (the `ContextManager` buffer shape): text/media/tool calls become a role-tagged
/// object; a tool-result lowers to a `tool` item keyed by `tool_call_id`.
fn message_to_provider_item(message: &Message) -> Item {
    use browser_use_llm::schema::MessageRole;
    use serde_json::{json, Value};

    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::Developer => "developer",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    // Tool-result messages -> a `tool` item carrying the textual output.
    if message.role == MessageRole::Tool {
        if let Some(ContentPart::ToolResult {
            tool_call_id,
            content,
            ..
        }) = message.content.first()
        {
            return json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": tool_result_content_to_provider_content(content),
            });
        }
    }

    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                content_parts.push(json!({ "type": "text", "text": text }));
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                content_parts.push(json!({
                    "type": "image",
                    "mime_type": mime_type,
                    "data": data,
                    "url": url,
                    "detail": detail,
                }));
            }
            ContentPart::ToolCall {
                id, name, input, ..
            } => {
                tool_calls.push(json!({
                    "id": id,
                    "name": name,
                    "arguments": input,
                }));
            }
            ContentPart::ToolResult { .. } | ContentPart::Reasoning { .. } => {}
        }
    }

    let mut obj = serde_json::Map::new();
    obj.insert("role".to_string(), json!(role));
    obj.insert("content".to_string(), Value::Array(content_parts));
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(obj)
}

fn tool_result_content_to_provider_content(content: &[ContentPart]) -> serde_json::Value {
    use serde_json::{json, Value};

    let mut text = String::new();
    let mut parts = Vec::new();
    let mut has_non_text = false;
    for part in content {
        match part {
            ContentPart::Text { text: fragment }
            | ContentPart::Reasoning { text: fragment, .. } => {
                text.push_str(fragment);
                if !fragment.is_empty() {
                    parts.push(json!({ "type": "input_text", "text": fragment }));
                }
            }
            ContentPart::Media {
                mime_type,
                data,
                url,
                detail,
            } => {
                has_non_text = true;
                if let Some(media) = media_content_part_for_provider(
                    mime_type,
                    data.as_deref(),
                    url.as_deref(),
                    detail.as_deref(),
                ) {
                    parts.push(media);
                }
            }
            ContentPart::ToolResult { content, .. } => {
                let nested = tool_result_content_to_provider_content(content);
                match nested {
                    Value::String(fragment) => {
                        text.push_str(&fragment);
                        if !fragment.is_empty() {
                            parts.push(json!({ "type": "input_text", "text": fragment }));
                        }
                    }
                    Value::Array(nested_parts) => {
                        has_non_text = true;
                        parts.extend(nested_parts);
                    }
                    _ => {}
                }
            }
            ContentPart::ToolCall { .. } => {}
        }
    }
    if has_non_text {
        Value::Array(parts)
    } else {
        Value::String(text)
    }
}

fn media_content_part_for_provider(
    mime_type: &str,
    data: Option<&str>,
    url: Option<&str>,
    detail: Option<&str>,
) -> Option<serde_json::Value> {
    let resolved = match (url, data) {
        (Some(url), _) => url.to_string(),
        (None, Some(data)) => format!("data:{mime_type};base64,{data}"),
        (None, None) => return None,
    };
    if mime_type.starts_with("image/") {
        Some(serde_json::json!({
            "type": "input_image",
            "image_url": resolved,
            "detail": detail.unwrap_or("auto"),
        }))
    } else {
        Some(serde_json::json!({
            "type": "input_file",
            "file_data": resolved,
        }))
    }
}

/// A [`TurnObserver`] that maps loop lifecycle into the durable UI event log.
///
/// On turn completion it emits the final agent message as a `session.done`
/// event through the durable UI sink, so the run's result is visible to the TUI
/// and protocol reducers. The streaming text deltas are emitted by the sampling
/// driver through the same durable sink.
struct StoreObserver {
    sink: Arc<dyn EventSink>,
    session_id: String,
}

impl StoreObserver {
    fn new(sink: Arc<dyn EventSink>, session_id: String) -> Self {
        Self { sink, session_id }
    }
}

impl TurnObserver for StoreObserver {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        // Phase-E seam: started/aborted lifecycle markers are not surfaced as
        // store events yet (the legacy stack had richer turn-lifecycle telemetry).
        // We persist the terminal session result, which is what readers need today.
        if let TurnLifecycleEvent::TurnComplete {
            last_agent_message: Some(text),
            ..
        } = ev
        {
            self.sink.emit(PendingEvent::new(
                self.session_id.clone(),
                names::SESSION_DONE,
                session_done_payload(Some(&text), None),
            ));
        }
    }
}

/// A network-free scripted driver for the `Fake` backend.
///
/// The `Fake` [`ProviderBackend`] has no real provider
/// ([`provider::ResolvedProvider::Fake`]); this driver lets the facade still drive
/// a run to completion offline. It returns one terminal [`SamplingOutcome`] (no
/// follow-up) carrying a fixed assistant message, so the loop completes in a
/// single iteration. Used for the `Fake` product backend and the facade tests.
struct FakeSamplingDriver {
    message: String,
}

impl FakeSamplingDriver {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl SamplingDriver for FakeSamplingDriver {
    async fn run_sampling_request(
        &self,
        _input: Vec<Message>,
        _cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        Ok(SamplingOutcome {
            model_needs_follow_up: false,
            last_agent_message: Some(self.message.clone()),
            defers_mailbox_delivery_to_next_turn: true,
            finish_reason: None,
        })
    }
}

/// Build the per-turn [`TurnCtx`] identity from a config + session id.
fn turn_ctx(session_id: &SessionId, config: &ProviderRunConfig) -> TurnCtx {
    TurnCtx {
        session_id: session_id.as_str().to_string(),
        model: config.model.clone(),
        // Best-effort wire-provider label from the backend; the request's real
        // provider identity is fixed inside `build_route`.
        provider: format!("{:?}", config.backend).to_ascii_lowercase(),
        base_instructions: base_instructions_for_config(config),
        browser_mode_instruction: config
            .options
            .browser_mode
            .as_deref()
            .map(crate::prompts::browser_mode_instruction),
        turn_idx: 0,
        attempt: 0,
    }
}

/// A no-op UI [`EventSink`] for tests that need to suppress emitted events.
#[cfg(test)]
struct DiscardSink;

#[cfg(test)]
impl EventSink for DiscardSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Drive a loop run to quiescence with `driver`, over a store-backed state +
/// observer. Returns the final assistant message (`None` if no text was produced).
///
/// `recorded` is the SHARED conversation buffer: for the real fused path it is the
/// SAME `Arc` the driver's [`FusionRecorder`] writes (so dispatched tool outputs
/// re-enter the next prompt). The state is built over it AFTER the driver so the
/// driver/recorder and the loop read/write the one buffer.
async fn drive_run<Sd: SamplingDriver>(
    store: SharedStore,
    session_id: SessionId,
    ctx: TurnCtx,
    driver: Sd,
    turn_has_fresh_input: bool,
    recorded: RecordedBuffer,
    compaction: Option<(
        i64,
        Option<i64>,
        AutoCompactTokenLimitScope,
        Arc<dyn DynCompactionSampler>,
    )>,
    current_model: Option<String>,
    compact_prompt: Option<String>,
    base_instructions: Option<String>,
    developer_instructions: Option<String>,
    previous_model_compaction: Option<PreviousModelCompaction>,
    cancel: CancellationToken,
) -> Result<Option<String>, AgentError> {
    let state = StoreTurnState::new(Arc::clone(&store), session_id.clone(), recorded);
    // Enable REAL token accounting + model-based compaction when a sampler is
    // available (the real backend path). The Fake/no-credential path passes `None`
    // and keeps the inert (never-compacts) behavior.
    let state = match compaction {
        Some((window, configured_limit, scope, sampler)) => {
            state.with_compaction(window, configured_limit, scope, sampler)
        }
        None => state,
    }
    .with_current_model(current_model.unwrap_or_default())
    .with_compaction_prompt(
        compact_prompt.unwrap_or_else(|| crate::compact::SUMMARIZATION_PROMPT.to_string()),
    )
    .with_compaction_instructions(
        base_instructions.unwrap_or_else(|| crate::prompts::browser_agent_system_prompt()),
        developer_instructions,
    )
    .with_previous_model_compaction(previous_model_compaction)
    .with_cancel(cancel.clone());

    let pre_turn_replay_from_seq = if turn_has_fresh_input {
        let events = events_from_store(&store, session_id.as_str());
        latest_replay_seq_before_fresh_input(&events)
    } else {
        None
    };
    *state.pre_turn_replay_from_seq.lock().unwrap() = pre_turn_replay_from_seq;
    state.compact_previous_model_downshift_if_needed().await?;
    if state.token_status().await.token_limit_reached {
        state.compact(CompactionMode::PreTurn).await?;
    }
    *state.pre_turn_replay_from_seq.lock().unwrap() = None;

    // The observer persists the terminal agent message through a synchronous
    // durable sink over the SharedStore. (The async `events::StoreSink` writer
    // needs sole ownership of the Store, which the facade does not have — it keeps
    // a SharedStore clone — so a small shared-lock adapter is used instead.)
    let sink: Arc<dyn EventSink> = make_ui_sink(Arc::clone(&store));
    let observer = StoreObserver::new(sink, session_id.as_str().to_string());

    let turn_loop = TurnLoop::new(state, driver, observer);
    let cancel_monitor =
        spawn_store_cancel_monitor(Arc::clone(&store), session_id.clone(), cancel.clone());
    let result = turn_loop
        .run(ctx, turn_has_fresh_input, cancel.clone())
        .await;
    cancel_monitor.abort();
    if result.is_ok() {
        ensure_fallback_capture_recording(&store, session_id.as_str());
    }
    result
}

fn spawn_store_cancel_monitor(
    store: SharedStore,
    session_id: SessionId,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let store = Arc::clone(&store);
            let session_id = session_id.as_str().to_string();
            let should_cancel = tokio::task::spawn_blocking(move || {
                let store = store.lock().expect("store mutex poisoned");
                store
                    .load_session(&session_id)
                    .ok()
                    .flatten()
                    .is_some_and(|session| session.status == SessionStatus::Cancelled)
            })
            .await
            .unwrap_or(false);
            if should_cancel {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

/// Build the durable UI sink for loop lifecycle events.
///
/// The async `events::StoreSink::spawn` requires sole ownership of the `Store`,
/// which the facade does not have (the caller keeps the `SharedStore`). So the
/// lifecycle observer persists through a small synchronous adapter over the
/// `SharedStore` instead.
fn make_ui_sink(store: SharedStore) -> Arc<dyn EventSink> {
    make_ui_sink_with_context_window(store, None)
}

fn make_ui_sink_with_context_window(
    store: SharedStore,
    model_context_window: Option<i64>,
) -> Arc<dyn EventSink> {
    Arc::new(SharedStoreSink {
        store,
        model_context_window,
    })
}

/// A synchronous [`EventSink`] over a [`SharedStore`] for lifecycle persistence.
///
/// The async durable sink needs sole ownership of the `Store`; the facade holds a
/// shared handle, so this adapter appends events directly under the shared lock.
/// Best-effort: append errors are swallowed (the loop's return value also carries
/// the result), matching the infallible-fan-out contract of [`EventSink::emit`].
struct SharedStoreSink {
    store: SharedStore,
    model_context_window: Option<i64>,
}

impl EventSink for SharedStoreSink {
    fn emit(&self, ev: PendingEvent) {
        if let Ok(store) = self.store.lock() {
            let payload = if ev.event_type == names::TOKEN_COUNT {
                let events = store.events_for_session(&ev.session_id).unwrap_or_default();
                let previous_total = latest_total_token_usage_value(&events);
                enrich_token_count_payload(ev.payload, &previous_total, self.model_context_window)
            } else {
                ev.payload
            };
            let _ = store.append_event(&ev.session_id, &ev.event_type, payload);
        }
    }
}

/// Run a session on the new async engine using a resolved provider config.
///
/// This is the binary-facing facade. It seeds the environment workspace-context
/// and the optional initial user message into the durable log under `session_id`,
/// builds the real sampling driver via [`provider::resolve_provider`] (which calls
/// [`build_sampling_driver`](crate::turn::model_path::build_sampling_driver)), and
/// drives the turn loop to quiescence, persisting the run's result. Returns the
/// [`SessionId`].
///
/// The `Fake` backend is driven offline via a scripted driver (so the assembly is
/// testable network-free); every other backend is driven with the real
/// [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver) (which
/// performs network I/O only when its `run_sampling_request` awaits the model
/// stream).
pub async fn run_session_with_config(
    store: SharedStore,
    session_id: &str,
    config: ProviderRunConfig,
) -> anyhow::Result<SessionId> {
    run_session_with_config_with_cancel(store, session_id, config, CancellationToken::new()).await
}

pub async fn run_session_with_config_with_cancel(
    store: SharedStore,
    session_id: &str,
    config: ProviderRunConfig,
    cancel: CancellationToken,
) -> anyhow::Result<SessionId> {
    let session_id = SessionId(session_id.to_string());
    loop {
        run_session_once_with_config_with_cancel(
            Arc::clone(&store),
            session_id.clone(),
            config.clone(),
            cancel.clone(),
        )
        .await?;

        if cancel.is_cancelled()
            || !has_pending_trigger_turn_agent_mail(&store, session_id.as_str())
        {
            return Ok(session_id);
        }
    }
}

async fn run_session_once_with_config_with_cancel(
    store: SharedStore,
    session_id: SessionId,
    config: ProviderRunConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let ctx = turn_ctx(&session_id, &config);

    // The single in-run conversation buffer, shared (by `Arc`) between the fused
    // driver's `FusionRecorder` (which records the assistant message + dispatched
    // tool outputs) and the loop's `StoreTurnState` (which reads it into each
    // prompt). Built FIRST so the recorder can be attached to the driver below and
    // the SAME buffer handed to `drive_run` for the state — closing the fusion loop.
    let recorded: RecordedBuffer = Arc::new(Mutex::new(Vec::new()));

    // (1) resolve provider → driver. This reaches `build_sampling_driver` for
    //     every real backend; `Fake` yields the offline-driver signal. For a real
    //     backend the driver is fused with the production tool dispatcher + a
    //     recorder writing into `recorded`, so model tool-calls EXECUTE and their
    //     outputs re-enter the prompt.
    let model_context_window = effective_context_window_for_config(&config);
    let driver_sink: Arc<dyn EventSink> =
        make_ui_sink_with_context_window(Arc::clone(&store), model_context_window);
    let fusion_recorder: Arc<dyn FusionRecorder> = Arc::new(BufferRecorder {
        buffer: Arc::clone(&recorded),
    });
    // Thread the run's credential store into provider resolution so API keys (and
    // codex tokens) resolve env-first, then from the stored `auth.<provider>.*`
    // settings the `auth login` command writes (fixes the env-only regression).
    // Provider/tool construction builds live store-backed helpers that lock the
    // SharedStore, so do not hold the shared mutex while resolving.
    let user_input_ctx = Some((Arc::clone(&store), session_id.clone()));
    let (state_dir, tool_cwd, tool_artifact_root) = {
        let store_guard = store.lock().expect("store mutex poisoned");
        let state_dir = store_guard.state_dir().to_path_buf();
        let session_meta = store_guard.load_session(session_id.as_str())?;
        let tool_cwd = session_meta
            .as_ref()
            .map(|session| std::path::PathBuf::from(&session.cwd))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        let tool_artifact_root = session_meta
            .as_ref()
            .map(|session| std::path::PathBuf::from(&session.artifact_root))
            .unwrap_or_else(|| tool_cwd.clone());
        (state_dir, tool_cwd, tool_artifact_root)
    };
    let provider_store = Store::open(&state_dir)?;
    let resolved = provider::resolve_provider_with_tool_paths(
        &config,
        Some(&provider_store),
        driver_sink,
        ctx.clone(),
        max_retries(&config),
        fusion_recorder,
        user_input_ctx,
        tool_cwd,
        tool_artifact_root,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // (2) seed the environment workspace-context durable event (de-duped per kind).
    let env_content = environment_context_content(&config);
    append_environment_context_event(Arc::clone(&store), session_id.as_str(), env_content).await?;
    if let Some(usage_hint) = multi_agent_v2_usage_hint_content(
        &config,
        session_is_spawned_subagent(&store, session_id.as_str())?,
    ) {
        append_workspace_context_event(
            Arc::clone(&store),
            session_id.as_str(),
            WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND,
            usage_hint,
        )
        .await?;
    }

    // The run drives over the session's existing durable history (the prompt the
    // caller already seeded). Fresh user input is sampled before queued steer
    // unless the reason this run exists is already-buffered mailbox mail; that
    // case matches Codex's "next turn" delivery and must drain immediately.
    let pending_mail_at_start = has_pending_agent_mail(&store, session_id.as_str());
    let turn_has_fresh_input =
        log_has_user_input(&store, session_id.as_str()) && !pending_mail_at_start;

    // (3) drive the loop to quiescence with the resolved driver. The SAME
    //     `recorded` buffer the recorder writes is handed to the state, so the
    //     fused tool outputs re-enter the prompt on the loop's next iteration.
    // The real path enables codex-parity compaction: real token accounting (the
    // 90%-of-window auto-compact trigger) + a model-based summary pass over the
    // SAME backend/model (`build_compaction_sampler`). Long runs auto-summarize
    // before hitting the context window. Fake/no-sampler paths stay inert because
    // there is no real model to write the handoff summary.
    match resolved {
        ResolvedProvider::Real(driver) => {
            let metadata = config.model_context_metadata();
            let context_window = metadata
                .effective_context_window()
                .unwrap_or(config.context_window_tokens as i64);
            let auto_compact_limit = metadata.auto_compact_token_limit_with_override(
                config.options.model_auto_compact_token_limit,
            );
            let previous_model_compaction =
                previous_model_compaction_for_config(&store, &session_id, &config);
            let compaction_sampler = {
                let store_guard = store.lock().expect("store mutex poisoned");
                build_compaction_sampler(&config, Some(&store_guard))
            };
            let compaction = compaction_sampler.map(|sampler| {
                (
                    context_window,
                    auto_compact_limit,
                    config.options.model_auto_compact_token_limit_scope,
                    sampler,
                )
            });
            drive_run(
                Arc::clone(&store),
                session_id.clone(),
                ctx,
                *driver,
                turn_has_fresh_input,
                Arc::clone(&recorded),
                compaction,
                Some(config.model.clone()),
                Some(compact_prompt_for_config(&config)),
                Some(base_instructions_for_config(&config)),
                config.options.developer_instructions.clone(),
                previous_model_compaction,
                cancel.clone(),
            )
            .await?;
        }
        ResolvedProvider::Fake => {
            // The fake backend has no real driver; drive offline so the facade is
            // exercisable end-to-end without a network. The recorder is unused here
            // (the fake driver does not dispatch), but the same buffer is still the
            // state's record sink so `record_items` works identically. Compaction is
            // disabled (no real model to summarize with).
            let driver = FakeSamplingDriver::new(fake_response_text(&config));
            drive_run(
                Arc::clone(&store),
                session_id.clone(),
                ctx,
                driver,
                turn_has_fresh_input,
                Arc::clone(&recorded),
                None,
                None,
                None,
                None,
                None,
                None,
                cancel.clone(),
            )
            .await?;
        }
    }

    Ok(())
}

/// True iff the session's durable log already contains a real user turn
/// (`session.input`), used to seed the loop's initial drain gate.
fn log_has_user_input(store: &SharedStore, session_id: &str) -> bool {
    let store = store.lock().expect("store mutex poisoned");
    store
        .events_for_session(session_id)
        .map(|events| {
            events.iter().any(|e| {
                matches!(
                    e.event_type.as_str(),
                    "session.input" | "agent.mailbox_input"
                )
            })
        })
        .unwrap_or(false)
}

/// The retry budget for the sampling driver (see [`DEFAULT_STREAM_MAX_RETRIES`]).
fn max_retries(_config: &ProviderRunConfig) -> u32 {
    DEFAULT_STREAM_MAX_RETRIES
}

/// The environment workspace-context content string.
///
/// Phase-E seam: the legacy stack assembles a rich `<environment_context>` block
/// (cwd + shell + network + AGENTS.md). That assembly depends on AGENTS.md loading
/// / environment snapshotting that are not ported into the agent crate yet, so the
/// facade seeds a minimal-but-real environment block carrying the configured cwd
/// (from the first environment, when present). Wave-E swaps in the full assembly.
fn environment_context_content(config: &ProviderRunConfig) -> String {
    let cwd = config
        .options
        .environment_context_environments
        .first()
        .map(|e| e.cwd.as_str())
        .unwrap_or(".");
    format!("<environment_context>\n<cwd>{cwd}</cwd>\n</environment_context>")
}

fn session_is_spawned_subagent(store: &SharedStore, session_id: &str) -> anyhow::Result<bool> {
    let store = store.lock().expect("store mutex poisoned");
    let has_parent = store
        .load_session(session_id)?
        .and_then(|session| session.parent_id)
        .is_some();
    if !has_parent {
        return Ok(false);
    }
    Ok(store
        .events_for_session(session_id)?
        .iter()
        .any(|event| event.event_type == "agent.context"))
}

fn multi_agent_v2_usage_hint_content(
    config: &ProviderRunConfig,
    is_spawned_subagent: bool,
) -> Option<String> {
    let options = &config.options.multi_agent_v2;
    if !options.enabled {
        return None;
    }
    let hint = if is_spawned_subagent {
        options.subagent_usage_hint_text.as_deref()
    } else {
        options.root_agent_usage_hint_text.as_deref()
    }?;
    let hint = hint.trim();
    (!hint.is_empty()).then(|| hint.to_string())
}

/// The fixed assistant reply the `Fake` backend emits.
///
/// Honors a configured [`ProviderRunConfig::with_fake_result`] when present
/// (parity: the legacy fake provider replays the scripted result, carried on the
/// run config's `fake_result` field), else a stable placeholder.
fn fake_response_text(config: &ProviderRunConfig) -> String {
    config
        .fake_result
        .clone()
        .unwrap_or_else(|| "(fake response)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::AgentRunOptions;
    use crate::config_overrides::MultiAgentV2Options;
    use crate::config_overrides::ProviderBackend;
    use crate::config_overrides::ProviderRunConfig;
    use browser_use_store::Store;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// A tempdir-backed `SharedStore` with a fresh session row (the `events` table
    /// has a FK on `sessions(id)`, so the session must exist before we append).
    /// Returns the `TempDir` so the caller keeps the on-disk sqlite db alive.
    fn store_with_session() -> (TempDir, SharedStore, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        let session_id = store
            .create_session(None, std::path::Path::new("/work"))
            .expect("create session")
            .id;
        (dir, Arc::new(Mutex::new(store)), session_id)
    }

    fn events(store: &SharedStore, session_id: &str) -> Vec<browser_use_protocol::EventRecord> {
        store
            .lock()
            .unwrap()
            .events_for_session(session_id)
            .unwrap()
    }

    fn fake_config() -> ProviderRunConfig {
        ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_fake_result("hi from fake")
    }

    /// Seed a real user turn into the durable log before driving.
    ///
    /// Appends straight through the store lock (the sync `Store::append_event`)
    /// rather than the `EventSink` trait, to avoid the two-`EventSink`-traits
    /// ambiguity in test scope.
    async fn seed_user_input(store: &SharedStore, session_id: &str, text: &str) {
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                "session.input",
                serde_json::json!({ "text": text }),
            )
            .expect("seed user input");
    }

    fn append_user_input(store: &SharedStore, session_id: &str, text: &str) -> i64 {
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                "session.input",
                serde_json::json!({ "text": text }),
            )
            .expect("seed user input")
            .seq
    }

    fn seed_token_count(store: &SharedStore, session_id: &str, total_tokens: i64) {
        seed_token_count_usage(store, session_id, total_tokens, 0, total_tokens);
    }

    fn seed_token_count_usage(
        store: &SharedStore,
        session_id: &str,
        input_tokens: i64,
        output_tokens: i64,
        total_tokens: i64,
    ) {
        let usage = serde_json::json!({
            "input_tokens": input_tokens,
            "cached_input_tokens": 0,
            "output_tokens": output_tokens,
            "reasoning_output_tokens": 0,
            "total_tokens": total_tokens,
        });
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                names::TOKEN_COUNT,
                serde_json::json!({
                    "info": {
                        "total_token_usage": usage,
                        "last_token_usage": usage,
                        "model_context_window": 1_000,
                    },
                    "turn_idx": 0,
                }),
            )
            .expect("seed token_count");
    }

    fn seed_model_turn_request(store: &SharedStore, session_id: &str, model: &str) {
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                names::MODEL_TURN_REQUEST,
                serde_json::json!({
                    "model": model,
                    "provider": "fake",
                    "turn_idx": 0,
                    "attempt": 0,
                }),
            )
            .expect("seed model turn request");
    }

    fn assistant_text_message(text: &str) -> Message {
        Message::new(
            browser_use_llm::schema::MessageRole::Assistant,
            vec![browser_use_llm::schema::ContentPart::text(text)],
        )
    }

    fn user_text_message(text: &str) -> Message {
        Message::new(
            browser_use_llm::schema::MessageRole::User,
            vec![browser_use_llm::schema::ContentPart::text(text)],
        )
    }

    fn seed_workspace_context(store: &SharedStore, session_id: &str, content: &str) {
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                "workspace.context",
                serde_json::json!({
                    "kind": "environment_context",
                    "content": content,
                }),
            )
            .expect("seed workspace context");
    }

    struct StaticSummarySampler(&'static str);

    impl CompactionSampler for StaticSummarySampler {
        fn summarize(
            &self,
            _request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            let summary = self.0.to_string();
            async move { Ok(CompactionSummary::text(summary)) }
        }
    }

    struct UsageSummarySampler {
        summary: &'static str,
        usage: TokenUsage,
    }

    impl CompactionSampler for UsageSummarySampler {
        fn summarize(
            &self,
            _request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            let summary = self.summary.to_string();
            let usage = self.usage;
            async move { Ok(CompactionSummary::with_usage(summary, usage)) }
        }
    }

    struct FlakySummarySampler {
        attempts: AtomicUsize,
    }

    impl CompactionSampler for FlakySummarySampler {
        fn summarize(
            &self,
            _request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    Err(AgentError::Provider("temporary stream failure".to_string()))
                } else {
                    Ok(CompactionSummary::text("handoff after retry"))
                }
            }
        }
    }

    struct CapturingSummarySampler {
        summary: &'static str,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl CapturingSummarySampler {
        fn new(summary: &'static str) -> Self {
            Self {
                summary,
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl CompactionSampler for CapturingSummarySampler {
        fn summarize(
            &self,
            request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            self.requests.lock().unwrap().push(request);
            let summary = self.summary.to_string();
            async move { Ok(CompactionSummary::text(summary)) }
        }
    }

    struct AlwaysWindowExceededSampler;

    impl CompactionSampler for AlwaysWindowExceededSampler {
        fn summarize(
            &self,
            _request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            async { Err(AgentError::ContextWindowExceeded) }
        }
    }

    struct WaitForCancelSampler;

    impl CompactionSampler for WaitForCancelSampler {
        fn summarize(
            &self,
            _request: Vec<Message>,
            cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            async move {
                cancel.cancelled().await;
                Err(AgentError::TurnAborted)
            }
        }
    }

    struct WindowThenRetryableThenSuccessSampler {
        attempts: AtomicUsize,
        request_lens: Mutex<Vec<usize>>,
    }

    impl WindowThenRetryableThenSuccessSampler {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                request_lens: Mutex::new(Vec::new()),
            }
        }
    }

    impl CompactionSampler for WindowThenRetryableThenSuccessSampler {
        fn summarize(
            &self,
            request: Vec<Message>,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<CompactionSummary, AgentError>> + Send {
            self.request_lens.lock().unwrap().push(request.len());
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                match attempt {
                    0 => Err(AgentError::ContextWindowExceeded),
                    1 => Err(AgentError::Provider("temporary stream failure".to_string())),
                    _ => Ok(CompactionSummary::text("handoff after mixed failures")),
                }
            }
        }
    }

    /// The full config-driven facade drives the Fake backend end-to-end over a
    /// seeded session, persisting the env-context + the agent reply to quiescence.
    #[tokio::test]
    async fn config_facade_drives_fake_backend_to_quiescence() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "do a thing").await;

        let id = run_session_with_config(Arc::clone(&store), &session_id, fake_config())
            .await
            .expect("config facade must run the fake backend");
        assert_eq!(id.as_str(), session_id);

        let log = events(&store, &session_id);
        // env workspace-context was seeded.
        assert!(
            log.iter().any(|e| e.event_type == "workspace.context"),
            "expected a seeded workspace.context event"
        );
        // the terminal agent message was persisted as the visible session result.
        assert!(
            log.iter().any(|e| e.event_type == "session.done"
                && e.payload.get("result").and_then(|v| v.as_str()) == Some("hi from fake")),
            "expected the fake assistant reply persisted; log={log:?}"
        );
    }

    #[tokio::test]
    async fn config_facade_seeds_multi_agent_v2_usage_hint_context() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "do a thing").await;
        let config = fake_config().with_options(AgentRunOptions {
            multi_agent_v2: MultiAgentV2Options {
                enabled: true,
                root_agent_usage_hint_text: Some("Root delegation guidance.".to_string()),
                ..Default::default()
            },
            ..AgentRunOptions::default()
        });

        run_session_with_config(Arc::clone(&store), &session_id, config)
            .await
            .expect("config facade must run the fake backend");
        let log = events(&store, &session_id);
        assert!(log.iter().any(|event| {
            event.event_type == "workspace.context"
                && event
                    .payload
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    == Some(WORKSPACE_CONTEXT_MULTI_AGENT_USAGE_HINT_KIND)
                && event
                    .payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    == Some("Root delegation guidance.")
        }));
    }

    /// With no user turn in the log the facade still drives (env-context only) and
    /// completes — proving the seed/loop wiring is independent of fresh input.
    #[tokio::test]
    async fn config_facade_drives_without_initial_input() {
        let (_dir, store, session_id) = store_with_session();
        run_session_with_config(Arc::clone(&store), &session_id, fake_config())
            .await
            .expect("facade must run with no user input");
        let log = events(&store, &session_id);
        assert!(log.iter().any(|e| e.event_type == "workspace.context"));
        assert!(
            !log.iter().any(|e| e.event_type == "session.input"),
            "no user input should be present"
        );
        // The agent still produced (and persisted) a visible result.
        assert!(log.iter().any(|e| e.event_type == "session.done"));
    }

    #[tokio::test]
    async fn config_facade_drains_trigger_turn_mail_at_run_start() {
        let (_dir, store, root_id) = store_with_session();
        {
            let store = store.lock().expect("store mutex poisoned");
            let child = store
                .create_child_session(
                    &root_id,
                    std::path::Path::new("/work"),
                    Some("/root/worker"),
                    Some("Atlas"),
                    Some("explorer"),
                )
                .expect("child session");
            store
                .send_agent_message(&child.id, &root_id, "queued trigger update", true)
                .expect("agent message");
        }

        run_session_with_config(Arc::clone(&store), &root_id, fake_config())
            .await
            .expect("facade must run with queued trigger-turn mail");

        assert!(!has_pending_agent_mail(&store, &root_id));
        let log = events(&store, &root_id);
        assert!(log
            .iter()
            .any(|event| event.event_type == "agent.mailbox_input"));
    }

    /// The codex backend is a REAL provider again: with codex OAuth creds in the
    /// store, the facade resolves + drives it (the live `ModelClient::stream` only
    /// fires when actually sampled, so constructing the run is network-free; the
    /// fake-less codex path is exercised for construction here via the store creds).
    ///
    /// We assert the facade does NOT reject codex with a "cut" error anymore. With
    /// no codex login present at all, it surfaces an honest missing-credentials
    /// error (not the old "codex is cut" rejection).
    #[tokio::test]
    async fn config_facade_codex_backend_missing_creds_is_honest_error() {
        // Ensure no env codex creds leak in from the test environment.
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        std::env::remove_var("CODEX_ACCOUNT_ID");
        // Point CODEX_HOME at an empty dir so the on-disk `~/.codex/auth.json`
        // fallback resolves to "no login" rather than reading a real user file.
        let codex_home = tempfile::tempdir().expect("codex home");
        std::env::set_var("CODEX_HOME", codex_home.path());
        let (_dir, store, session_id) = store_with_session();
        let cfg = ProviderRunConfig::new(ProviderBackend::Codex, "codex-model");
        let err = run_session_with_config(store, &session_id, cfg)
            .await
            .expect_err("codex with no login must error on missing creds");
        std::env::remove_var("CODEX_HOME");
        let msg = err.to_string();
        assert!(
            msg.contains("codex login") || msg.contains("CODEX_ACCESS_TOKEN"),
            "error should be an honest missing-codex-login message, not 'cut': {msg}"
        );
        assert!(
            !msg.contains("cut"),
            "codex is no longer cut; error must not say so: {msg}"
        );
    }

    /// The store-backed [`TurnState`] lowers the durable log into the prompt and
    /// records assistant turns back — proving the loop's conversation seam.
    #[tokio::test]
    async fn store_turn_state_lowers_history_and_records() {
        let (_dir, store, session_id) = store_with_session();
        // Seed a real user input event into the durable log (straight through the
        // store lock; see `seed_user_input` for why we avoid the EventSink trait).
        let sid = SessionId(session_id.clone());
        {
            let store = store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    &session_id,
                    "session.input",
                    serde_json::json!({ "text": "hello" }),
                )
                .expect("append");
        }

        let state = StoreTurnState::new(Arc::clone(&store), sid, Arc::new(Mutex::new(Vec::new())));
        let before = state.clone_history_for_prompt().await;
        assert!(
            !before.is_empty(),
            "the seeded user message must lower into the prompt"
        );

        let assistant = Message::new(
            browser_use_llm::schema::MessageRole::Assistant,
            vec![browser_use_llm::schema::ContentPart::text("hi")],
        );
        state.record_items(std::slice::from_ref(&assistant)).await;
        let after = state.clone_history_for_prompt().await;
        assert_eq!(
            after.len(),
            before.len() + 1,
            "recorded assistant turn should be visible on the next prompt"
        );
        assert!(!state.has_pending_input().await);
        assert!(!state.token_status().await.token_limit_reached);
    }

    #[tokio::test]
    async fn pending_active_followup_drains_into_history_once() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "initial").await;
        let pending_seq = {
            let store = store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    &session_id,
                    SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT,
                    serde_json::json!({
                        "text": "steer after tool",
                        "delivery": FOLLOWUP_DELIVERY_AFTER_NEXT_TOOL_CALL,
                    }),
                )
                .expect("pending followup")
                .seq
        };

        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );

        assert!(state.has_pending_input().await);
        assert!(
            state.take_pending_input().await.is_empty(),
            "drained followups are made durable before prompt assembly, so the returned ad-hoc input stays empty"
        );
        assert!(!state.has_pending_input().await);

        let prompt = state.clone_history_for_prompt().await;
        let prompt_text = prompt
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt_text.contains("steer after tool"));

        let log = events(&store, &session_id);
        let committed_followup_seq = log
            .iter()
            .find(|event| {
                event.event_type == names::SESSION_FOLLOWUP
                    && event
                        .payload
                        .get("pending_from_seq")
                        .and_then(Value::as_i64)
                        == Some(pending_seq)
            })
            .map(|event| event.seq)
            .expect("pending followup should be committed as a durable followup");
        assert!(log.iter().any(|event| {
            event.event_type == AGENT_TURN_QUEUE_DRAINED_EVENT
                && event.payload.get("last_seq").and_then(Value::as_i64)
                    == Some(committed_followup_seq)
        }));
        assert!(log.iter().any(|event| {
            event.event_type == "model.response.continued"
                && event.payload.get("reason").and_then(Value::as_str)
                    == Some("active_turn_queue_drained")
        }));
    }

    #[tokio::test]
    async fn token_status_prefers_provider_usage_over_whole_prompt_estimate() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, &"x".repeat(2_000)).await;
        seed_token_count(&store, &session_id, 100);

        let recorded = Arc::new(Mutex::new(vec![assistant_text_message("done")]));
        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state =
            StoreTurnState::new(Arc::clone(&store), SessionId(session_id.clone()), recorded)
                .with_compaction(1_000, Some(300), AutoCompactTokenLimitScope::Total, sampler);

        let status = state.token_status().await;
        assert_eq!(
            status.auto_compact_scope_tokens, 100,
            "provider-reported usage should be the active token source"
        );
        assert!(
            !status.token_limit_reached,
            "the huge old prompt would trip a whole-prompt estimate, but provider usage should not"
        );
    }

    #[tokio::test]
    async fn token_status_falls_back_to_local_estimate_without_provider_usage() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, &"x".repeat(2_000)).await;

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, Some(300), AutoCompactTokenLimitScope::Total, sampler);

        let status = state.token_status().await;
        assert!(
            status.auto_compact_scope_tokens >= 300,
            "without provider usage, the whole-prompt local estimate is the fallback"
        );
        assert!(status.token_limit_reached);
    }

    #[tokio::test]
    async fn token_status_ignores_provider_usage_before_latest_compaction() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, &"x".repeat(2_000)).await;
        seed_token_count(&store, &session_id, 900);
        {
            let store = store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    &session_id,
                    "session.compacted",
                    serde_json::json!({
                        "message": "handoff",
                        "replacement_messages": [
                            { "role": "user", "content": "small compacted prompt" }
                        ],
                        "initial_context_already_in_history": true,
                    }),
                )
                .expect("seed compaction checkpoint");
        }

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, Some(300), AutoCompactTokenLimitScope::Total, sampler);

        let status = state.token_status().await;
        assert!(
            !status.token_limit_reached,
            "pre-compaction provider usage must not leak into the compacted prompt window"
        );
        assert!(
            status.auto_compact_scope_tokens < 300,
            "post-compaction prompt should be sized from the compacted replacement history"
        );
    }

    #[test]
    fn shared_store_sink_accumulates_token_counts_and_sets_context_window() {
        let (_dir, store, session_id) = store_with_session();
        let sink = make_ui_sink_with_context_window(Arc::clone(&store), Some(950));
        sink.emit(PendingEvent::new(
            session_id.clone(),
            names::TOKEN_COUNT,
            serde_json::json!({
                "info": {
                    "total_token_usage": token_usage_value_with_total(10),
                    "last_token_usage": token_usage_value_with_total(10),
                    "model_context_window": null,
                },
                "turn_idx": 0,
            }),
        ));
        sink.emit(PendingEvent::new(
            session_id.clone(),
            names::TOKEN_COUNT,
            serde_json::json!({
                "info": {
                    "total_token_usage": token_usage_value_with_total(20),
                    "last_token_usage": token_usage_value_with_total(20),
                    "model_context_window": null,
                },
                "turn_idx": 0,
            }),
        ));

        let log = events(&store, &session_id);
        let latest = log
            .iter()
            .rev()
            .find(|event| event.event_type == names::TOKEN_COUNT)
            .expect("token count");
        assert_eq!(
            latest.payload["info"]["total_token_usage"]["total_tokens"].as_i64(),
            Some(30)
        );
        assert_eq!(
            latest.payload["info"]["model_context_window"].as_i64(),
            Some(950)
        );
    }

    #[tokio::test]
    async fn body_after_prefix_prefill_uses_first_provider_usage_sample() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "initial request").await;
        seed_token_count_usage(&store, &session_id, 80, 20, 100);

        let recorded = Arc::new(Mutex::new(vec![assistant_text_message("done")]));
        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::clone(&recorded),
        )
        .with_compaction(
            1_000,
            Some(50),
            AutoCompactTokenLimitScope::BodyAfterPrefix,
            sampler,
        );

        let first = state.token_status().await;
        assert_eq!(
            first.auto_compact_scope_tokens, 20,
            "the first response output remains body growth; only provider input is the prefix baseline"
        );

        recorded.lock().unwrap().push(user_text_message(
            "small follow-up after the provider response",
        ));

        let second = state.token_status().await;
        assert!(
            second.auto_compact_scope_tokens > 0,
            "new user input after the provider response should count against the body budget"
        );
        assert!(
            second.auto_compact_scope_tokens < 50,
            "the baseline should be provider usage, not the full prompt"
        );
    }

    #[tokio::test]
    async fn body_after_prefix_uses_effective_context_window_for_full_guard() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "initial request").await;
        seed_token_count_usage(&store, &session_id, 95, 0, 95);

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(
            95,
            Some(1_000),
            AutoCompactTokenLimitScope::BodyAfterPrefix,
            sampler,
        );

        let status = state.token_status().await;
        assert!(
            status.full_context_window_limit_reached,
            "BodyAfterPrefix should use Codex's effective context window as the full-window guard"
        );
        assert!(status.token_limit_reached);
    }

    #[tokio::test]
    async fn mid_turn_compaction_persists_checkpoint_with_initial_context_in_history() {
        let (_dir, store, session_id) = store_with_session();
        seed_workspace_context(
            &store,
            &session_id,
            "<environment_context>\n<cwd>/work</cwd>\n</environment_context>",
        );
        seed_user_input(&store, &session_id, "please compact this session").await;

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);

        state
            .compact(CompactionMode::MidTurn)
            .await
            .expect("mid-turn compaction should persist a checkpoint");

        let log = events(&store, &session_id);
        let compacted = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted event");
        assert_eq!(
            compacted
                .payload
                .get("initial_context_already_in_history")
                .and_then(Value::as_bool),
            Some(true)
        );
        let replacement = compacted
            .payload
            .get("replacement_messages")
            .and_then(Value::as_array)
            .expect("replacement messages");

        let workspace_idx = replacement
            .iter()
            .position(|item| item.get("name").and_then(Value::as_str) == Some("workspace_context"))
            .expect("workspace context is inserted into mid-turn replacement history");
        let user_idx = replacement
            .iter()
            .position(|item| provider_item_text(item).contains("please compact this session"))
            .expect("real user input is preserved");
        let summary_idx = replacement
            .iter()
            .position(|item| provider_item_text(item).contains("handoff"))
            .expect("summary is appended");
        assert!(
            workspace_idx < user_idx,
            "initial context should be reinserted before the real user message"
        );
        assert_eq!(
            summary_idx,
            replacement.len() - 1,
            "summary should remain the final replacement message"
        );
        assert!(provider_item_text(&replacement[summary_idx])
            .starts_with(crate::compact::SUMMARY_PREFIX));

        let reconstructed = provider_messages_from_events(&log);
        let workspace_count = reconstructed
            .iter()
            .filter(|item| item.get("name").and_then(Value::as_str) == Some("workspace_context"))
            .count();
        assert_eq!(
            workspace_count, 1,
            "mid-turn replay should not prepend duplicate initial context"
        );
    }

    #[tokio::test]
    async fn pre_turn_compaction_reducer_prepends_initial_context_to_checkpoint() {
        let (_dir, store, session_id) = store_with_session();
        seed_workspace_context(
            &store,
            &session_id,
            "<environment_context>\n<cwd>/work</cwd>\n</environment_context>",
        );
        seed_user_input(&store, &session_id, "please compact before the turn").await;

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);

        state
            .compact(CompactionMode::PreTurn)
            .await
            .expect("pre-turn compaction should persist a checkpoint");

        let log = events(&store, &session_id);
        let compacted = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted event");
        assert_eq!(
            compacted
                .payload
                .get("initial_context_already_in_history")
                .and_then(Value::as_bool),
            Some(false)
        );
        let replacement = compacted
            .payload
            .get("replacement_messages")
            .and_then(Value::as_array)
            .expect("replacement messages");
        assert!(
            replacement
                .iter()
                .all(|item| item.get("name").and_then(Value::as_str).is_none()),
            "pre-turn checkpoint should store only compacted conversation, not initial context"
        );

        let reconstructed = provider_messages_from_events(&log);
        let workspace_idx = reconstructed
            .iter()
            .position(|item| item.get("name").and_then(Value::as_str) == Some("workspace_context"))
            .expect("replay prepends workspace context");
        let user_idx = reconstructed
            .iter()
            .position(|item| provider_item_text(item).contains("please compact before the turn"))
            .expect("real user input remains in replayed history");
        let summary_idx = reconstructed
            .iter()
            .position(|item| provider_item_text(item).contains("handoff"))
            .expect("summary remains in replayed history");
        assert!(
            workspace_idx < user_idx,
            "pre-turn replay should prepend initial context before the compacted user history"
        );
        assert!(user_idx < summary_idx);
    }

    #[tokio::test]
    async fn pre_turn_compaction_replays_fresh_input_after_checkpoint() {
        let (_dir, store, session_id) = store_with_session();
        append_user_input(&store, &session_id, "old request");
        let fresh_seq = append_user_input(&store, &session_id, "fresh request");

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);
        *state.pre_turn_replay_from_seq.lock().unwrap() = Some(fresh_seq - 1);

        state
            .compact(CompactionMode::PreTurn)
            .await
            .expect("pre-turn compaction should persist a checkpoint");

        let log = events(&store, &session_id);
        let compacted = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted event");
        assert_eq!(
            compacted
                .payload
                .get("replay_from_seq")
                .and_then(Value::as_i64),
            Some(fresh_seq - 1)
        );
        let replacement_text = compacted
            .payload
            .get("replacement_messages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(provider_item_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(replacement_text.contains("old request"));
        assert!(
            !replacement_text.contains("fresh request"),
            "fresh input is recorded after pre-turn compaction, not summarized into it"
        );

        let replayed_text = provider_messages_from_events(&log)
            .iter()
            .map(provider_item_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(replayed_text.contains("old request"));
        assert!(replayed_text.contains("fresh request"));
    }

    #[tokio::test]
    async fn compaction_recomputes_token_count_after_checkpoint() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "please compact").await;
        seed_token_count(&store, &session_id, 900);

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(StaticSummarySampler("handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);

        state
            .compact(CompactionMode::MidTurn)
            .await
            .expect("compaction should succeed");

        let log = events(&store, &session_id);
        let compacted_seq = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted")
            .seq;
        let token_count = log
            .iter()
            .find(|event| event.event_type == names::TOKEN_COUNT && event.seq > compacted_seq)
            .expect("post-compaction token_count");
        assert!(
            token_count.payload["info"]["last_token_usage"]["total_tokens"]
                .as_i64()
                .unwrap_or_default()
                > 0
        );
        assert_eq!(
            token_count.payload["info"]["total_token_usage"]["total_tokens"].as_i64(),
            Some(900),
            "Codex recompute preserves cumulative total usage and replaces only last usage"
        );
    }

    #[tokio::test]
    async fn compaction_summary_usage_is_counted_before_recompute() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "please compact").await;
        seed_token_count(&store, &session_id, 900);

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(UsageSummarySampler {
            summary: "handoff",
            usage: TokenUsage {
                input: 30,
                cached_input: 5,
                output: 20,
                reasoning_output: 0,
                total: 50,
            },
        });
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);

        state
            .compact(CompactionMode::MidTurn)
            .await
            .expect("compaction should succeed");

        let log = events(&store, &session_id);
        let compacted_seq = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted")
            .seq;
        let summary_usage = log
            .iter()
            .find(|event| {
                event.event_type == names::TOKEN_COUNT
                    && event.seq < compacted_seq
                    && event.payload["info"]["last_token_usage"]["total_tokens"].as_i64()
                        == Some(50)
            })
            .expect("summary token usage is recorded before the checkpoint");
        assert_eq!(
            summary_usage.payload["info"]["total_token_usage"]["total_tokens"].as_i64(),
            Some(950)
        );
        let recomputed = log
            .iter()
            .find(|event| event.event_type == names::TOKEN_COUNT && event.seq > compacted_seq)
            .expect("post-compaction token_count");
        assert_eq!(
            recomputed.payload["info"]["total_token_usage"]["total_tokens"].as_i64(),
            Some(950),
            "post-compaction recompute preserves cumulative usage including the summary model call"
        );
    }

    #[tokio::test]
    async fn compaction_retries_retryable_summary_failures() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "please compact").await;

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(FlakySummarySampler {
            attempts: AtomicUsize::new(0),
        });
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler);

        state
            .compact(CompactionMode::MidTurn)
            .await
            .expect("retryable provider failure should be retried");

        let log = events(&store, &session_id);
        assert!(log.iter().any(|event| {
            event.event_type == names::STREAM_ERROR
                && event
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.contains("Reconnecting... 1/"))
        }));
        assert!(log
            .iter()
            .any(|event| event.event_type == "session.compacted"));
    }

    #[tokio::test]
    async fn compaction_retry_preserves_context_window_trimmed_history() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "old request").await;
        seed_user_input(&store, &session_id, "new request").await;

        let sampler = Arc::new(WindowThenRetryableThenSuccessSampler::new());
        let sampler_dyn: Arc<dyn DynCompactionSampler> = sampler.clone();

        let compacted = run_compaction_with_retries(
            &SessionId(session_id.clone()),
            &store,
            &[
                serde_json::json!({"role": "user", "content": [{"type": "text", "text": "old request"}]}),
                serde_json::json!({"role": "user", "content": [{"type": "text", "text": "new request"}]}),
            ],
            sampler_dyn.as_ref(),
            crate::compact::SUMMARIZATION_PROMPT,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            1,
            Some(1_000),
            CancellationToken::new(),
        )
        .await
        .expect("mixed window and retryable errors should eventually succeed");

        assert!(compacted
            .summary_text
            .contains("handoff after mixed failures"));
        assert_eq!(
            *sampler.request_lens.lock().unwrap(),
            vec![3, 2, 2],
            "after ContextWindowExceeded trims the oldest item, the retryable provider failure must retry the trimmed request"
        );
    }

    #[tokio::test]
    async fn compaction_marks_tokens_full_when_summary_cannot_fit() {
        let (_dir, store, session_id) = store_with_session();
        seed_token_count(&store, &session_id, 400);

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(AlwaysWindowExceededSampler);
        let err = run_compaction_with_retries(
            &SessionId(session_id.clone()),
            &store,
            &[],
            sampler.as_ref(),
            crate::compact::SUMMARIZATION_PROMPT,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            0,
            Some(1_000),
            CancellationToken::new(),
        )
        .await
        .expect_err("unshrinkable summary request should fail");
        assert!(matches!(err, AgentError::ContextWindowExceeded));

        let log = events(&store, &session_id);
        let token_count = log
            .iter()
            .rev()
            .find(|event| event.event_type == names::TOKEN_COUNT)
            .expect("token full event");
        assert_eq!(
            token_count.payload["info"]["total_token_usage"]["total_tokens"].as_i64(),
            Some(1_000)
        );
        assert_eq!(
            token_count.payload["info"]["last_token_usage"]["total_tokens"].as_i64(),
            Some(600)
        );
        assert!(log
            .iter()
            .any(|event| event.event_type == "model.turn.context_overflow"));
    }

    #[tokio::test]
    async fn compaction_uses_callers_cancellation_token() {
        let (_dir, store, session_id) = store_with_session();
        let cancel = CancellationToken::new();
        let cancel_from_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_from_task.cancel();
        });

        let sampler: Arc<dyn DynCompactionSampler> = Arc::new(WaitForCancelSampler);
        let err = tokio::time::timeout(
            Duration::from_secs(2),
            run_compaction_with_retries(
                &SessionId(session_id.clone()),
                &store,
                &[],
                sampler.as_ref(),
                crate::compact::SUMMARIZATION_PROMPT,
                COMPACT_USER_MESSAGE_MAX_TOKENS,
                0,
                Some(1_000),
                cancel,
            ),
        )
        .await
        .expect("compaction should observe cancellation")
        .expect_err("cancelled compaction should fail");

        assert!(matches!(err, AgentError::TurnAborted));
    }

    #[tokio::test]
    async fn previous_model_downshift_compacts_before_current_model_sampling() {
        let (_dir, store, session_id) = store_with_session();
        seed_model_turn_request(&store, &session_id, "larger-model");
        seed_token_count(&store, &session_id, 800);

        let current_sampler: Arc<dyn DynCompactionSampler> =
            Arc::new(StaticSummarySampler("current model handoff"));
        let previous_sampler: Arc<dyn DynCompactionSampler> =
            Arc::new(StaticSummarySampler("previous model handoff"));
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(
            700,
            Some(700),
            AutoCompactTokenLimitScope::Total,
            current_sampler,
        )
        .with_current_model("smaller-model")
        .with_previous_model_compaction(Some(PreviousModelCompaction {
            model: "larger-model".to_string(),
            model_context_window: 1_000,
            sampler: previous_sampler,
        }));

        assert!(state
            .compact_previous_model_downshift_if_needed()
            .await
            .expect("downshift compaction succeeds"));
        let log = events(&store, &session_id);
        let compacted = log
            .iter()
            .find(|event| event.event_type == "session.compacted")
            .expect("session.compacted");
        assert!(
            compacted
                .payload
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("previous model handoff")),
            "downshift compaction should use the previous model sampler"
        );
    }

    #[tokio::test]
    async fn store_compaction_uses_custom_compact_prompt() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "please compact").await;

        let sampler = Arc::new(CapturingSummarySampler::new("handoff"));
        let sampler_dyn: Arc<dyn DynCompactionSampler> = sampler.clone();
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .with_compaction(1_000, None, AutoCompactTokenLimitScope::Total, sampler_dyn)
        .with_compaction_prompt("CUSTOM COMPACT PROMPT")
        .with_compaction_instructions(
            "BASE INSTRUCTIONS",
            Some("DEVELOPER INSTRUCTIONS".to_string()),
        );

        state
            .compact(CompactionMode::MidTurn)
            .await
            .expect("compaction should succeed");

        let requests = sampler.requests.lock().unwrap();
        let request = requests.first().expect("request captured");
        let texts = request
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(texts.contains(&"CUSTOM COMPACT PROMPT"));
        assert!(
            !texts.contains(&crate::compact::SUMMARIZATION_PROMPT),
            "custom prompt should replace the built-in compact prompt"
        );
    }

    #[test]
    fn summary_llm_request_carries_base_instructions_without_extra_developer_message() {
        let req = build_summary_llm_request(
            "model",
            "provider",
            "BASE INSTRUCTIONS",
            vec![user_text_message("summary prompt")],
        );

        assert_eq!(req.system.len(), 1);
        assert_eq!(req.system[0].text, "BASE INSTRUCTIONS");
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(req.messages[0].role, MessageRole::User));
    }

    #[tokio::test]
    async fn store_turn_state_drains_agent_mailbox_as_pending_input() {
        let (_dir, store, root_id) = store_with_session();
        let child_id = {
            let store = store.lock().expect("store mutex poisoned");
            let child = store
                .create_child_session(
                    &root_id,
                    std::path::Path::new("/work"),
                    Some("/root/worker"),
                    Some("Atlas"),
                    Some("explorer"),
                )
                .expect("child session");
            store
                .send_agent_message(&child.id, &root_id, "child update", false)
                .expect("agent message");
            child.id
        };

        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(root_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );
        assert!(state.has_pending_input().await);
        let drained = state.take_pending_input().await;
        assert_eq!(drained.len(), 1);
        assert!(!state.has_pending_input().await);

        assert_eq!(drained[0].role, MessageRole::Assistant);
        let ContentPart::Text { text } = &drained[0].content[0] else {
            panic!("mailbox input should be a text envelope");
        };
        let envelope: serde_json::Value =
            serde_json::from_str(text).expect("mailbox envelope json");
        assert_eq!(envelope["author"], "/root/worker");
        assert_eq!(envelope["recipient"], "/root");
        assert_eq!(envelope["content"], "child update");
        assert_eq!(envelope["trigger_turn"], false);

        let root_events = events(&store, &root_id);
        let mailbox_event = root_events
            .iter()
            .find(|event| event.event_type == "agent.mailbox_input")
            .expect("mailbox input event");
        assert_eq!(mailbox_event.payload["author_session_id"], child_id);
        assert_eq!(mailbox_event.payload["target_session_id"], root_id);
    }

    #[tokio::test]
    async fn store_turn_state_defers_mailbox_after_answer_boundary() {
        let (_dir, store, root_id) = store_with_session();
        {
            let store = store.lock().expect("store mutex poisoned");
            let child = store
                .create_child_session(
                    &root_id,
                    std::path::Path::new("/work"),
                    Some("/root/worker"),
                    Some("Atlas"),
                    Some("explorer"),
                )
                .expect("child session");
            store
                .send_agent_message(&child.id, &root_id, "late queue-only update", false)
                .expect("agent message");
        }

        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(root_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );
        state.defer_mailbox_delivery_to_next_turn().await;

        assert!(
            !state.has_pending_input().await,
            "mailbox-only input should not extend the current turn after a visible answer"
        );
        assert!(
            state.take_pending_input().await.is_empty(),
            "deferred mailbox input should remain buffered for the next turn"
        );
        {
            let store = store.lock().expect("store mutex poisoned");
            assert_eq!(store.messages_for_agent(&root_id).unwrap().len(), 1);
        }

        let next_turn_state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(root_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );
        assert!(next_turn_state.has_pending_input().await);
        assert_eq!(next_turn_state.take_pending_input().await.len(), 1);
    }

    #[tokio::test]
    async fn store_turn_state_defers_trigger_turn_mail_after_answer_boundary() {
        let (_dir, store, root_id) = store_with_session();
        {
            let store = store.lock().expect("store mutex poisoned");
            let child = store
                .create_child_session(
                    &root_id,
                    std::path::Path::new("/work"),
                    Some("/root/worker"),
                    Some("Atlas"),
                    Some("explorer"),
                )
                .expect("child session");
            store
                .send_agent_message(&child.id, &root_id, "late trigger update", true)
                .expect("agent message");
        }

        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(root_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );
        state.defer_mailbox_delivery_to_next_turn().await;

        assert!(
            !state.has_pending_input().await,
            "trigger-turn mailbox input should not stretch the just-answered turn"
        );
        assert!(has_pending_trigger_turn_agent_mail(&store, &root_id));
    }

    #[tokio::test]
    async fn store_turn_state_active_followup_reopens_deferred_mailbox() {
        let (_dir, store, root_id) = store_with_session();
        {
            let store = store.lock().expect("store mutex poisoned");
            let child = store
                .create_child_session(
                    &root_id,
                    std::path::Path::new("/work"),
                    Some("/root/worker"),
                    Some("Atlas"),
                    Some("explorer"),
                )
                .expect("child session");
            store
                .send_agent_message(&child.id, &root_id, "queued child update", false)
                .expect("agent message");
        }

        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(root_id.clone()),
            Arc::new(Mutex::new(Vec::new())),
        );
        state.defer_mailbox_delivery_to_next_turn().await;
        {
            let store = store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    &root_id,
                    SESSION_PENDING_ACTIVE_FOLLOWUP_EVENT,
                    json!({
                        "text": "operator follow-up",
                        "delivery": FOLLOWUP_DELIVERY_AFTER_NEXT_TOOL_CALL,
                    }),
                )
                .expect("pending follow-up");
        }

        assert!(
            state.has_pending_input().await,
            "active follow-up input should reopen the current turn mailbox"
        );
        let drained = state.take_pending_input().await;
        assert_eq!(drained.len(), 1);
        assert!(!has_pending_agent_mail(&store, &root_id));

        let log = events(&store, &root_id);
        assert!(log
            .iter()
            .any(|event| event.event_type == names::SESSION_FOLLOWUP));
        assert!(log
            .iter()
            .any(|event| event.event_type == "agent.mailbox_input"));
    }

    struct CancelAwareDriver;

    impl SamplingDriver for CancelAwareDriver {
        async fn run_sampling_request(
            &self,
            _input: Vec<Message>,
            cancel: CancellationToken,
        ) -> Result<SamplingOutcome, AgentError> {
            cancel.cancelled().await;
            Err(AgentError::TurnAborted)
        }
    }

    #[tokio::test]
    async fn drive_run_observes_store_cancel_requests() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "long task").await;
        let sid = SessionId(session_id.clone());
        let config = fake_config();
        let ctx = turn_ctx(&sid, &config);
        let store_for_cancel = Arc::clone(&store);
        let session_for_cancel = session_id.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let store = store_for_cancel.lock().expect("store mutex poisoned");
            store
                .request_cancel(&session_for_cancel, "test cancellation")
                .expect("request cancel");
        });

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            drive_run(
                Arc::clone(&store),
                sid,
                ctx,
                CancelAwareDriver,
                true,
                Arc::new(Mutex::new(Vec::new())),
                None,
                None,
                None,
                None,
                None,
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("cancel monitor should abort the hanging driver")
        .expect("turn abort should be a graceful run result");
        assert_eq!(result, None);

        let log = events(&store, &session_id);
        assert!(log
            .iter()
            .any(|event| event.event_type == "session.cancel_requested"));
    }

    // -----------------------------------------------------------------------
    // Fusion seam: a scripted tool-call drives a REAL registry dispatch, and the
    // loop re-samples with the tool output in the next prompt — exactly the wiring
    // `run_session_with_config` assembles (BufferRecorder + StoreTurnState sharing
    // one buffer + a fused ModelSamplingDriver), but driven offline by a scripted
    // transport instead of a live ModelClient (so the test is network-free).
    // -----------------------------------------------------------------------

    use crate::turn::dispatch::{RegistryRunner, ToolDispatcher};
    use crate::turn::sampling::{EventStream, ModelSamplingDriver, SamplingTransport};
    use crate::turn::TurnLoop;
    use browser_use_llm::schema::{
        ContentPart, FinishReason, LlmError, LlmEvent, LlmRequest, MessageRole, Usage,
    };

    /// A transport that replays a fixed per-iteration `LlmEvent` script (no
    /// `ModelClient`, no socket) — the offline analogue of the live transport the
    /// entrypoint builds. Mirrors `turn/fusion_tests.rs::ScriptedTransport`.
    struct ScriptedTransport {
        scripts: Mutex<std::collections::VecDeque<Vec<LlmEvent>>>,
    }

    impl SamplingTransport for ScriptedTransport {
        fn open_stream<'a>(&'a self, _req: &LlmRequest) -> Result<EventStream<'a>, LlmError> {
            let events = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            let items: Vec<Result<LlmEvent, LlmError>> = events.into_iter().map(Ok).collect();
            Ok(Box::pin(::futures_util::stream::iter(items)))
        }
    }

    /// A no-op observer for the loop.
    struct NoopObserver;
    impl TurnObserver for NoopObserver {
        fn on_lifecycle(&self, _ev: TurnLifecycleEvent) {}
    }

    /// Build the production tool dispatcher (registry + orchestrator stub) over a
    /// shell-only registry, so a scripted `shell` tool-call dispatches for real.
    fn shell_dispatcher() -> Arc<ToolDispatcher<RegistryRunner>> {
        use crate::tools::handlers::shell::{ShellRequest, ShellTool};
        use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
        use crate::tools::registry::{definitions, ToolRegistry};
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;

        let mut reg = ToolRegistry::new();
        reg.register::<_, ShellRequest>("shell", definitions::shell(), false, ShellTool::new());
        let runner = RegistryRunner::new(
            Arc::new(reg),
            Arc::new(ToolOrchestrator::stub()),
            ToolCtx {
                call_id: "c".to_string(),
                tool_name: "shell".to_string(),
                cwd: std::env::temp_dir(),
                artifact_root: std::env::temp_dir().join("artifacts"),
            },
            TurnEnv {
                file_system_sandbox_policy: FileSystemSandboxPolicy {
                    restricted: false,
                    denied_read: false,
                },
                managed_network_active: false,
                strict_auto_review: false,
                use_guardian: false,
            },
            crate::tools::approval::AskForApproval::Never,
        );
        Arc::new(ToolDispatcher::with_runner(
            runner, /* model_supports */ true,
        ))
    }

    /// Iteration 1's scripted model response emits a `shell` echo tool-call; the
    /// fused driver dispatches it THROUGH the real registry+orchestrator and the
    /// `BufferRecorder` writes the assistant message + tool output into the SAME
    /// buffer the `StoreTurnState` reads. Iteration 2 (whose prompt is built from
    /// that buffer) emits only text, so the loop completes. Proves the entrypoint's
    /// fusion seam: scripted tool-call → real dispatch → re-sample sees the output.
    #[tokio::test]
    async fn fused_entrypoint_driver_dispatches_and_resamples_with_output() {
        let (_dir, store, session_id) = store_with_session();

        // The single shared buffer — exactly what `run_session_with_config` wires:
        // the recorder writes it, the state reads it.
        let recorded: RecordedBuffer = Arc::new(Mutex::new(Vec::new()));
        let recorder: Arc<dyn FusionRecorder> = Arc::new(BufferRecorder {
            buffer: Arc::clone(&recorded),
        });
        let state = StoreTurnState::new(
            Arc::clone(&store),
            SessionId(session_id.clone()),
            Arc::clone(&recorded),
        );

        let ctx = TurnCtx {
            session_id: session_id.clone(),
            model: "m".to_string(),
            provider: "fake".to_string(),
            base_instructions: crate::prompts::browser_agent_system_prompt(),
            browser_mode_instruction: None,
            turn_idx: 0,
            attempt: 0,
        };

        // iter 1: text + a `shell` echo tool-call; iter 2: final text, no tools.
        let scripts = vec![
            vec![
                LlmEvent::TextDelta {
                    id: "t0".to_string(),
                    delta: "running shell".to_string(),
                },
                LlmEvent::ToolCall {
                    id: "call-1".to_string(),
                    name: "shell".to_string(),
                    namespace: None,
                    input: serde_json::json!({ "command": ["echo", "fusion-ok"] }),
                },
                LlmEvent::Finish {
                    usage: Usage::default(),
                    finish_reason: Some(FinishReason::Stop),
                },
            ],
            vec![
                LlmEvent::TextDelta {
                    id: "t1".to_string(),
                    delta: "all done".to_string(),
                },
                LlmEvent::Finish {
                    usage: Usage::default(),
                    finish_reason: Some(FinishReason::Stop),
                },
            ],
        ];
        let transport = ScriptedTransport {
            scripts: Mutex::new(scripts.into_iter().collect()),
        };

        // The fused driver: scripted transport + production dispatcher + the
        // entrypoint's BufferRecorder, all over the shared buffer.
        let driver = ModelSamplingDriver::new(transport, Arc::new(DiscardSink), ctx.clone(), 3)
            .without_jitter()
            .with_fusion(shell_dispatcher(), recorder);

        let turn = TurnLoop::new(state, driver, NoopObserver);
        let out = turn
            .run(
                ctx,
                /* turn_has_fresh_input */ false,
                CancellationToken::new(),
            )
            .await
            .expect("fused entrypoint turn should complete");

        // The loop ran two iterations and returned the final text.
        assert_eq!(out.as_deref(), Some("all done"));

        // The dispatched shell output landed in the shared buffer (so iteration 2
        // re-sampled with it). Assert a Tool-role message carrying the echo output.
        let buf = recorded.lock().unwrap().clone();
        let tool_texts: Vec<String> = buf
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .flat_map(|m| m.content.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|c| match c {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_texts.len(),
            1,
            "exactly one tool output recorded: {buf:?}"
        );
        assert!(
            tool_texts[0].contains("fusion-ok"),
            "the dispatched shell tool output must re-enter the prompt: {:?}",
            tool_texts[0]
        );

        // Transcript shape in the shared buffer: assistant(text+call) then the
        // tool result — the recorder fed the loop's re-sample buffer in order.
        let roles: Vec<MessageRole> = buf.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![MessageRole::Assistant, MessageRole::Tool]);
    }
}
