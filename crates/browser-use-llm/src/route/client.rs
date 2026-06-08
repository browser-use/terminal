//! Async model client: drive a [`Route`] over HTTP and stream neutral events.
//!
//! A [`Route`] bundles the three transport concerns — a [`Protocol`] (wire
//! format), an [`Endpoint`] (URL), and [`Auth`] (credentials) — into a single
//! value. The async [`ModelClient`] takes a route plus a neutral [`LlmRequest`]
//! and:
//!
//! 1. builds the provider body via [`Protocol::build_body`],
//! 2. POSTs it with the auth headers and `content-type: application/json`,
//! 3. maps a non-2xx status onto a typed [`LlmError`],
//! 4. feeds the streamed response bytes through the [`SseDecoder`] and the
//!    protocol's [`ProtocolStream`], yielding [`LlmEvent`]s,
//! 5. retries retryable failures with deterministic, attempt-indexed backoff
//!    that honours `Retry-After` / `retry-after-ms` headers.
//!
//! ## Testability
//!
//! The decode loop is factored into the pure [`decode_chunks`] helper, which
//! takes a fresh [`ProtocolStream`] and a list of byte chunks and returns the
//! full event sequence. The streaming client uses the exact same SSE-decoder →
//! protocol-decoder pipeline internally, so a `#[test]` over canned SSE bytes
//! covers the real decode path without any network.
//!
//! The retry policy is likewise pure: [`RetryPolicy::plan`] turns a sequence of
//! simulated [`Outcome`]s into the attempts taken and the delays slept, so its
//! behaviour is asserted without a clock or sockets. Live HTTP is exercised only
//! by [`ModelClient::stream`] / [`ModelClient::generate`], which the in-crate
//! unit tests never call (no live e2e is wired — see the module doc-comment on
//! `decode_chunks` and the report).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::route::framing::{SseDecoder, SseFrame};
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::route::{Auth, Endpoint};
use crate::schema::{
    ContentPart, FinishReason, LlmError, LlmErrorReason, LlmEvent, LlmRequest, LlmResponse, Usage,
};

// ===========================================================================
// Route
// ===========================================================================

/// A fully-resolved route: a wire-format protocol, an HTTP target, and creds.
pub struct Route {
    /// The wire-format adapter for this provider.
    pub protocol: Box<dyn Protocol>,
    /// The HTTP target to POST to.
    pub endpoint: Endpoint,
    /// The credential material for the request.
    pub auth: Auth,
}

impl Route {
    /// Bundle a protocol, endpoint, and auth into a route.
    pub fn new(protocol: Box<dyn Protocol>, endpoint: Endpoint, auth: Auth) -> Self {
        Self {
            protocol,
            endpoint,
            auth,
        }
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `auth` is deliberately omitted: it carries secret material and must
        // never appear in a `Debug` dump.
        f.debug_struct("Route")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

// ===========================================================================
// Typed-error helpers (constructors + retry classification)
// ===========================================================================

/// Construction helpers for the canonical [`LlmError`] by failure category.
///
/// The schema sets `retryable` at construction from the reason; these are thin
/// named constructors so the executor can build typed errors without repeating
/// the `LlmError::new(reason, ..)` boilerplate. The server `Retry-After` hint is
/// deliberately *not* stored on the error (the schema has no field for it and it
/// must never leak through `Display`); it is threaded separately via the
/// retry-loop's `Option<u64>` and [`Outcome::Fail`]'s `retry_after_ms`.
trait LlmErrorExt {
    /// A transport-layer failure (connection reset, DNS, TLS, ...).
    fn transport(message: impl Into<String>) -> LlmError;
    /// A 5xx / provider-internal failure.
    fn provider_internal(message: impl Into<String>) -> LlmError;
    /// A rate-limit (429) failure.
    fn rate_limit(message: impl Into<String>) -> LlmError;
    /// An authentication / authorization failure (401 / 403).
    fn authentication(message: impl Into<String>) -> LlmError;
    /// A non-retryable bad-request style failure (4xx other than auth/429).
    fn invalid_request(message: impl Into<String>) -> LlmError;
}

impl LlmErrorExt for LlmError {
    fn transport(message: impl Into<String>) -> LlmError {
        LlmError::new(LlmErrorReason::Transport, message)
    }
    fn provider_internal(message: impl Into<String>) -> LlmError {
        LlmError::new(LlmErrorReason::ProviderInternal, message)
    }
    fn rate_limit(message: impl Into<String>) -> LlmError {
        LlmError::new(LlmErrorReason::RateLimit, message)
    }
    fn authentication(message: impl Into<String>) -> LlmError {
        LlmError::new(LlmErrorReason::Authentication, message)
    }
    fn invalid_request(message: impl Into<String>) -> LlmError {
        LlmError::new(LlmErrorReason::InvalidRequest, message)
    }
}

/// Whether an error is worth retrying (rate-limit, transport, provider-5xx).
fn is_retryable(err: &LlmError) -> bool {
    err.retryable
        || matches!(
            err.reason,
            LlmErrorReason::RateLimit
                | LlmErrorReason::Transport
                | LlmErrorReason::ProviderInternal
        )
}

/// Map an HTTP status code (and best-effort body) onto a typed [`LlmError`].
fn error_for_status(status: u16, body: &str) -> LlmError {
    let snippet = body.trim();
    let msg = if snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {snippet}")
    };
    let mut err = match status {
        401 | 403 => LlmError::authentication(msg),
        429 => LlmError::rate_limit(msg),
        500..=599 => LlmError::provider_internal(msg),
        _ => LlmError::invalid_request(msg),
    };
    err.status = Some(status);
    err
}

// ===========================================================================
// Retry policy (pure, deterministic)
// ===========================================================================

/// Configuration for retry/backoff behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first). `1` disables retries.
    pub max_attempts: u32,
    /// Base delay for the first backoff step.
    pub base_delay: Duration,
    /// Upper bound on any single backoff delay.
    pub max_delay: Duration,
    /// Optional deterministic jitter seed. When `Some`, a bounded,
    /// attempt-indexed pseudo-jitter is added (no wall clock, no RNG), so the
    /// resulting delays remain reproducible in tests.
    pub jitter_seed: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(60),
            jitter_seed: None,
        }
    }
}

impl RetryPolicy {
    /// Pure backoff for a given zero-based attempt index.
    ///
    /// Delay is `base * 2^attempt`, clamped to `max_delay`. A server-provided
    /// hint (`Retry-After` / `retry-after-ms`) takes precedence when present.
    /// When [`jitter_seed`](RetryPolicy::jitter_seed) is set, a deterministic
    /// per-attempt offset in `[0, base)` is added before clamping; identical
    /// inputs always yield identical delays.
    pub fn backoff(&self, attempt: u32, server_hint: Option<Duration>) -> Duration {
        if let Some(hint) = server_hint {
            return hint.min(self.max_delay);
        }
        let base_ms = self.base_delay.as_millis() as u64;
        // 2^attempt, saturating to avoid overflow on absurd attempt counts.
        let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
        let mut ms = base_ms.saturating_mul(factor);
        if let Some(seed) = self.jitter_seed {
            ms = ms.saturating_add(deterministic_jitter(seed, attempt, base_ms));
        }
        Duration::from_millis(ms).min(self.max_delay)
    }

    /// Plan the attempts and inter-attempt delays for a sequence of outcomes.
    ///
    /// This is the pure core of the retry loop: given the outcome each attempt
    /// would produce, it returns the [`RetryPlan`] describing how many attempts
    /// ran, the delay slept before each retry, and the final result. It performs
    /// no I/O and no sleeping, so the policy is asserted directly in tests.
    pub fn plan(&self, outcomes: &[Outcome]) -> RetryPlan {
        let mut delays = Vec::new();
        let max = self.max_attempts.max(1);
        for attempt in 0..max {
            let outcome = outcomes
                .get(attempt as usize)
                .cloned()
                .unwrap_or(Outcome::Success);
            match outcome {
                Outcome::Success => {
                    return RetryPlan {
                        attempts: attempt + 1,
                        delays,
                        outcome: PlanOutcome::Success,
                    };
                }
                Outcome::Fail {
                    error,
                    retry_after_ms,
                } => {
                    let last_attempt = attempt + 1 >= max;
                    if !is_retryable(&error) || last_attempt {
                        return RetryPlan {
                            attempts: attempt + 1,
                            delays,
                            outcome: PlanOutcome::Failed(error),
                        };
                    }
                    let hint = retry_after_ms.map(Duration::from_millis);
                    delays.push(self.backoff(attempt, hint));
                }
            }
        }
        // Exhausted the attempt budget without a definite outcome; treat the
        // tail as a generic transport failure rather than panicking.
        RetryPlan {
            attempts: max,
            delays,
            outcome: PlanOutcome::Failed(LlmError::transport("retry budget exhausted")),
        }
    }
}

/// Deterministic, bounded pseudo-jitter in `[0, base_ms)`.
///
/// A tiny splitmix-style mix of the seed and attempt index; no global RNG and
/// no clock, so identical inputs always produce identical jitter.
fn deterministic_jitter(seed: u64, attempt: u32, base_ms: u64) -> u64 {
    if base_ms == 0 {
        return 0;
    }
    let mut z = seed
        .wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z % base_ms
}

/// A simulated per-attempt outcome, used to drive [`RetryPolicy::plan`].
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The attempt would succeed.
    Success,
    /// The attempt would fail with this error, optionally carrying a
    /// server-suggested retry delay (ms) parsed from the response headers.
    Fail {
        /// The typed failure.
        error: LlmError,
        /// Server `Retry-After` / `retry-after-ms` hint, if any.
        retry_after_ms: Option<u64>,
    },
}

impl Outcome {
    /// A failure with no server retry hint.
    pub fn fail(error: LlmError) -> Self {
        Outcome::Fail {
            error,
            retry_after_ms: None,
        }
    }

    /// A failure carrying a server retry hint (ms).
    pub fn fail_after(error: LlmError, retry_after_ms: u64) -> Self {
        Outcome::Fail {
            error,
            retry_after_ms: Some(retry_after_ms),
        }
    }
}

/// The resolved outcome of a [`RetryPlan`].
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOutcome {
    /// A successful attempt was reached.
    Success,
    /// All retries were exhausted (or a non-retryable error was hit) with this
    /// terminal error.
    Failed(LlmError),
}

/// The result of planning a retry sequence: how many attempts and what delays.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPlan {
    /// Number of attempts performed (1-based).
    pub attempts: u32,
    /// The delay slept before each retry (length == retries == attempts - 1 on
    /// failure paths; shorter when an earlier success short-circuits).
    pub delays: Vec<Duration>,
    /// The terminal outcome of the sequence.
    pub outcome: PlanOutcome,
}

// ===========================================================================
// Rate-limit headers + redaction
// ===========================================================================

/// Parsed provider rate-limit headers.
///
/// Captures the common OpenAI (`x-ratelimit-*`) and Anthropic
/// (`anthropic-ratelimit-*`) limit/remaining/reset fields. All fields are
/// optional; absent headers leave them `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitInfo {
    /// Request quota for the window, if reported.
    pub limit_requests: Option<u64>,
    /// Requests remaining in the window, if reported.
    pub remaining_requests: Option<u64>,
    /// Token quota for the window, if reported.
    pub limit_tokens: Option<u64>,
    /// Tokens remaining in the window, if reported.
    pub remaining_tokens: Option<u64>,
    /// When the request window resets (raw string, provider-formatted).
    pub reset_requests: Option<String>,
    /// When the token window resets (raw string, provider-formatted).
    pub reset_tokens: Option<String>,
    /// Suggested retry delay (from `retry-after` / `retry-after-ms`), in ms.
    pub retry_after_ms: Option<u64>,
}

impl RateLimitInfo {
    /// Parse rate-limit info from a header map (lower-cased keys).
    ///
    /// Accepts both the OpenAI and Anthropic header families. `retry-after`
    /// (seconds) and `retry-after-ms` (milliseconds) are both honoured, with
    /// the millisecond form taking precedence.
    pub fn from_headers(headers: &BTreeMap<String, String>) -> Self {
        let get = |k: &str| headers.get(k).map(|s| s.as_str());
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());

        let retry_after_ms = num("retry-after-ms").or_else(|| {
            get("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|secs| secs.saturating_mul(1000))
        });

        Self {
            limit_requests: num("x-ratelimit-limit-requests")
                .or_else(|| num("anthropic-ratelimit-requests-limit")),
            remaining_requests: num("x-ratelimit-remaining-requests")
                .or_else(|| num("anthropic-ratelimit-requests-remaining")),
            limit_tokens: num("x-ratelimit-limit-tokens")
                .or_else(|| num("anthropic-ratelimit-tokens-limit")),
            remaining_tokens: num("x-ratelimit-remaining-tokens")
                .or_else(|| num("anthropic-ratelimit-tokens-remaining")),
            reset_requests: get("x-ratelimit-reset-requests")
                .or_else(|| get("anthropic-ratelimit-requests-reset"))
                .map(str::to_string),
            reset_tokens: get("x-ratelimit-reset-tokens")
                .or_else(|| get("anthropic-ratelimit-tokens-reset"))
                .map(str::to_string),
            retry_after_ms,
        }
    }
}

/// Header keys whose values must never appear in logs, errors, or `Debug`.
const SECRET_HEADERS: &[&str] = &["authorization", "x-api-key"];

/// Redact secret header values for safe display.
///
/// Any header in [`SECRET_HEADERS`] (matched case-insensitively) has its value
/// replaced with `<redacted>`; all other headers pass through unchanged. Used
/// whenever request headers might be surfaced in an error message or debug dump.
pub fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            if SECRET_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                (k.clone(), "<redacted>".to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// Remove any secret material that a transport/error string might have captured.
///
/// reqwest occasionally embeds the request URL (and, defensively, we guard
/// against credential echoes) in its error `Display`. This strips known
/// credential tokens so they cannot leak into [`LlmError`] messages.
fn scrub(msg: &str) -> String {
    const REPLACEMENT: &str = "Bearer <redacted>";
    let mut out = msg.to_string();
    for marker in ["Bearer ", "bearer "] {
        // Advance a search cursor past each replacement so we never re-match the
        // "Bearer " inside our own "Bearer <redacted>" output — that self-match
        // was an infinite loop that hung the test binaries.
        let mut search_from = 0;
        while let Some(rel) = out[search_from..].find(marker) {
            let pos = search_from + rel;
            let token_start = pos + marker.len();
            let end = out[token_start..]
                .find(|c: char| c.is_whitespace())
                .map(|e| token_start + e)
                .unwrap_or(out.len());
            out.replace_range(pos..end, REPLACEMENT);
            search_from = pos + REPLACEMENT.len();
            if search_from >= out.len() {
                break;
            }
        }
    }
    out
}

// ===========================================================================
// Pure decode pipeline
// ===========================================================================

/// Run a fresh decoder over a sequence of raw byte chunks, returning all events.
///
/// This is the pure heart of the streaming client: it owns an [`SseDecoder`],
/// feeds each chunk through it to obtain [`SseFrame`]s, hands each frame to the
/// protocol's [`ProtocolStream`], and finally flushes the protocol decoder. It
/// performs no I/O, so the full decode pipeline is tested with canned bytes —
/// and [`ModelClient::stream`] uses this exact same per-chunk pipeline.
pub fn decode_chunks(
    mut decoder: Box<dyn ProtocolStream>,
    chunks: &[&[u8]],
) -> Result<Vec<LlmEvent>, LlmError> {
    let mut sse = SseDecoder::new();
    let mut events = Vec::new();
    for chunk in chunks {
        for frame in sse.push(chunk) {
            events.extend(decoder.on_frame(&frame)?);
        }
    }
    events.extend(decoder.finish()?);
    Ok(events)
}

/// Aggregate a list of streamed events into a single [`LlmResponse`].
///
/// Text/reasoning deltas concatenate into [`ContentPart`]s; completed tool calls
/// become [`ContentPart::ToolCall`]s; the terminal `Finish`/`StepFinish` supply
/// usage and the finish reason.
fn aggregate(events: Vec<LlmEvent>) -> LlmResponse {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ContentPart> = Vec::new();
    let mut usage = Usage::default();
    let mut finish_reason: Option<FinishReason> = None;

    for ev in events {
        match ev {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::ReasoningDelta { delta, .. } => reasoning.push_str(&delta),
            LlmEvent::ToolCall {
                id,
                name,
                namespace,
                input,
            } => {
                tool_calls.push(ContentPart::ToolCall {
                    id,
                    name,
                    input,
                    provider_metadata: namespace
                        .map(|namespace| serde_json::json!({ "namespace": namespace })),
                });
            }
            LlmEvent::Finish {
                usage: u,
                finish_reason: r,
            }
            | LlmEvent::StepFinish {
                usage: u,
                finish_reason: r,
            } => {
                if u != Usage::default() {
                    usage = u;
                }
                if r.is_some() {
                    finish_reason = r;
                }
            }
            // Starts/ends/tool-input deltas/step boundaries carry no aggregate
            // payload; provider errors surface in-stream, not in the response.
            _ => {}
        }
    }

    let mut content: Vec<ContentPart> = Vec::new();
    if !reasoning.is_empty() {
        content.push(ContentPart::Reasoning {
            text: reasoning,
            signature: None,
            provider_metadata: None,
        });
    }
    if !text.is_empty() {
        content.push(ContentPart::text(text));
    }
    content.extend(tool_calls);

    LlmResponse {
        content,
        usage,
        finish_reason,
    }
}

// ===========================================================================
// Async client + streaming state machine
// ===========================================================================

/// Where the streaming state machine is in the response lifecycle.
enum Phase {
    /// Still pulling byte chunks off the HTTP body.
    Streaming,
    /// Byte stream exhausted; flush the protocol decoder's terminal state.
    Flushing,
    /// No more events will be produced.
    Done,
}

/// A boxed, `Send` stream of HTTP byte chunks (as produced by reqwest).
type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

/// Carried state for the `stream::unfold` event pump.
struct StreamState {
    /// The async HTTP byte-chunk stream.
    byte_stream: ByteStream,
    /// Incremental byte→frame decoder.
    sse: SseDecoder,
    /// Per-stream protocol decode state.
    protocol_stream: Box<dyn ProtocolStream>,
    /// Events decoded but not yet yielded.
    ready: VecDeque<Result<LlmEvent, LlmError>>,
    /// Lifecycle phase.
    phase: Phase,
    /// Optional max idle gap between provider stream chunks.
    idle_timeout: Option<Duration>,
}

impl StreamState {
    /// Decode a batch of frames into the ready queue, recording any error and
    /// transitioning to `Done` on the first failure.
    fn decode_frames(&mut self, frames: Vec<SseFrame>) {
        for frame in frames {
            match self.protocol_stream.on_frame(&frame) {
                Ok(events) => self.ready.extend(events.into_iter().map(Ok)),
                Err(e) => {
                    self.ready.push_back(Err(e));
                    self.phase = Phase::Done;
                    return;
                }
            }
        }
    }
}

async fn next_byte_chunk(
    byte_stream: &mut ByteStream,
    idle_timeout: Option<Duration>,
) -> Option<Result<bytes::Bytes, LlmError>> {
    let next = match idle_timeout {
        Some(timeout) => match tokio::time::timeout(timeout, byte_stream.next()).await {
            Ok(next) => next,
            Err(_) => return Some(Err(stream_idle_timeout_error(timeout))),
        },
        None => byte_stream.next().await,
    };
    next.map(|chunk| chunk.map_err(|e| LlmError::transport(scrub(&e.to_string()))))
}

fn stream_idle_timeout_error(timeout: Duration) -> LlmError {
    LlmError::transport(format!(
        "model stream idle timeout after {}ms",
        timeout.as_millis()
    ))
}

fn model_request_timeout_error(timeout: Duration) -> LlmError {
    LlmError::transport(format!(
        "model request timeout after {}ms",
        timeout.as_millis()
    ))
}

// Compile-time guard: the streaming state must stay `Send` so the event stream
// can cross task/thread boundaries (the public `stream` future is `Send`).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<StreamState>();
};

/// An async client that drives a [`Route`] over HTTP.
///
/// Clones cheaply (the inner `reqwest::Client` is reference-counted) and is safe
/// to share across tasks.
#[derive(Clone)]
pub struct ModelClient {
    /// The underlying async HTTP client.
    http: reqwest::Client,
    /// Retry/backoff configuration.
    retry: RetryPolicy,
    /// Optional timeout for opening the provider response stream.
    request_timeout: Option<Duration>,
    /// Optional max idle gap between streamed HTTP byte chunks.
    stream_idle_timeout: Option<Duration>,
}

impl fmt::Debug for ModelClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelClient")
            .field("retry", &self.retry)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for ModelClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelClient {
    /// Construct a client with the default retry policy.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            retry: RetryPolicy::default(),
            request_timeout: None,
            stream_idle_timeout: None,
        }
    }

    /// Construct a client with a custom retry policy.
    pub fn with_retry(retry: RetryPolicy) -> Self {
        Self {
            http: reqwest::Client::new(),
            retry,
            request_timeout: None,
            stream_idle_timeout: None,
        }
    }

    /// Construct a client from an existing `reqwest::Client` and retry policy.
    pub fn from_parts(http: reqwest::Client, retry: RetryPolicy) -> Self {
        Self {
            http,
            retry,
            request_timeout: None,
            stream_idle_timeout: None,
        }
    }

    /// The retry policy in effect.
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }

    /// Bound how long a request may wait for the provider response stream to open.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Disable provider response-open timeout enforcement.
    pub fn without_request_timeout(mut self) -> Self {
        self.request_timeout = None;
        self
    }

    /// The provider response-open timeout in effect.
    pub fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    /// Bound the idle gap between streamed response byte chunks.
    pub fn with_stream_idle_timeout(mut self, timeout: Duration) -> Self {
        self.stream_idle_timeout = Some(timeout);
        self
    }

    /// Disable stream idle timeout enforcement.
    pub fn without_stream_idle_timeout(mut self) -> Self {
        self.stream_idle_timeout = None;
        self
    }

    /// The stream idle timeout in effect.
    pub fn stream_idle_timeout(&self) -> Option<Duration> {
        self.stream_idle_timeout
    }

    /// Build the prepared request body and header list for a route + request.
    fn prepare(
        &self,
        route: &Route,
        req: &LlmRequest,
    ) -> Result<(Value, Vec<(String, String)>), LlmError> {
        let body = route.protocol.build_body(req)?;
        let mut headers = route.auth.headers();
        headers.push(("content-type".to_string(), "application/json".to_string()));
        Ok((body, headers))
    }

    /// Send the request once, returning the response on a 2xx status or a typed
    /// error (plus any server retry hint) otherwise. Retried failures bubble up
    /// to [`send_with_retry`].
    async fn send_once(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<reqwest::Response, (LlmError, Option<u64>)> {
        // Serialize the body ourselves and attach it via `.body(..)` rather than
        // `.json(..)`: reqwest's `.json()` *also* sets a `Content-Type:
        // application/json` header, and the `headers` list already carries an
        // explicit `content-type` (from `prepare`). reqwest's `.header()` APPENDS,
        // so using `.json()` here produced a duplicated
        // `Content-Type: application/json, application/json`, which strict APIs
        // (OpenAI) reject with HTTP 400. Setting the body bytes directly keeps the
        // single `content-type` from the header list as the only one on the wire.
        let body_bytes = serde_json::to_vec(body)
            .map_err(|e| (LlmError::transport(scrub(&e.to_string())), None))?;
        let mut builder = self.http.post(url).body(body_bytes);
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| (LlmError::transport(scrub(&e.to_string())), None))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        // Non-2xx: collect headers for the rate-limit hint, then the body snippet.
        let info = RateLimitInfo::from_headers(&header_map(resp.headers()));
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        Err((error_for_status(code, &scrub(&text)), info.retry_after_ms))
    }

    /// Send with retry/backoff, honouring the configured [`RetryPolicy`].
    async fn send_with_retry(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<reqwest::Response, LlmError> {
        let max = self.retry.max_attempts.max(1);
        let mut last_err: Option<LlmError> = None;
        for attempt in 0..max {
            let send_once = self.send_once(url, headers, body);
            let attempt_result = match self.request_timeout {
                Some(timeout) => match tokio::time::timeout(timeout, send_once).await {
                    Ok(result) => result,
                    Err(_) => Err((model_request_timeout_error(timeout), None)),
                },
                None => send_once.await,
            };
            match attempt_result {
                Ok(resp) => return Ok(resp),
                Err((err, retry_after_ms)) => {
                    let last_attempt = attempt + 1 >= max;
                    if !is_retryable(&err) || last_attempt {
                        return Err(err);
                    }
                    let hint = retry_after_ms.map(Duration::from_millis);
                    let delay = self.retry.backoff(attempt, hint);
                    last_err = Some(err);
                    tokio::time::sleep(delay).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| LlmError::transport("retry budget exhausted")))
    }

    /// Stream neutral events for a request over the given route.
    ///
    /// Builds the body, POSTs with auth + `content-type: application/json`,
    /// maps non-2xx onto a typed [`LlmError`] (with retry/backoff), then decodes
    /// the streamed bytes into [`LlmEvent`]s. The returned stream yields decode
    /// errors inline as `Err` items; transport/setup failures surface as the
    /// outer `Err`.
    pub async fn stream(
        &self,
        route: &Route,
        req: &LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<LlmEvent, LlmError>> + Send>>, LlmError> {
        let (body, headers) = self.prepare(route, req)?;
        let url = route.endpoint.url();
        let resp = self.send_with_retry(&url, &headers, &body).await?;

        let state = StreamState {
            byte_stream: Box::pin(resp.bytes_stream()),
            sse: SseDecoder::new(),
            protocol_stream: route.protocol.decoder(),
            ready: VecDeque::new(),
            phase: Phase::Streaming,
            idle_timeout: self.stream_idle_timeout,
        };

        // Drive the byte stream through the same SSE → protocol pipeline as
        // `decode_chunks`, surfacing events one at a time via `stream::unfold`.
        let event_stream = futures_util::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(ev) = st.ready.pop_front() {
                    return Some((ev, st));
                }
                match st.phase {
                    Phase::Streaming => {
                        match next_byte_chunk(&mut st.byte_stream, st.idle_timeout).await {
                            Some(Ok(chunk)) => {
                                let frames = st.sse.push(chunk.as_ref());
                                st.decode_frames(frames);
                            }
                            Some(Err(e)) => {
                                st.phase = Phase::Done;
                                return Some((Err(e), st));
                            }
                            None => st.phase = Phase::Flushing,
                        }
                    }
                    Phase::Flushing => {
                        st.phase = Phase::Done;
                        match st.protocol_stream.finish() {
                            Ok(events) => st.ready.extend(events.into_iter().map(Ok)),
                            Err(e) => return Some((Err(e), st)),
                        }
                    }
                    Phase::Done => {
                        return st.ready.pop_front().map(|ev| (ev, st));
                    }
                }
            }
        });

        Ok(Box::pin(event_stream))
    }

    /// Drain the stream and aggregate it into a single [`LlmResponse`].
    pub async fn generate(&self, route: &Route, req: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut stream = self.stream(route, req).await?;
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item?);
        }
        Ok(aggregate(events))
    }
}

/// Convert reqwest headers into a lower-cased `BTreeMap`.
fn header_map(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::OpenAiResponsesProtocol;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    // --- decode_chunks: end-to-end OpenAI Responses SSE -----------------

    /// Canned OpenAI-Responses SSE bytes covering text deltas, a function-call
    /// lifecycle, and completion with usage. Event shapes mirror the protocol's
    /// own decoder test (`decoder_text_tool_call_and_usage`).
    const OPENAI_RESPONSES_SSE: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"Let me \"}\n",
        "\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"check.\"}\n",
        "\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"get_weather\"}}\n",
        "\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"city\\\":\"}\n",
        "\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"\\\"NYC\\\"}\"}\n",
        "\n",
        "event: response.function_call_arguments.done\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item_1\",\"arguments\":\"{\\\"city\\\":\\\"NYC\\\"}\"}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"total_tokens\":18}}}\n",
        "\n",
    );

    fn expected_events() -> Vec<LlmEvent> {
        use crate::schema::FinishReason;
        let usage = Usage {
            input_tokens: 11,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 7,
            reasoning_output_tokens: 0,
            total_tokens: 18,
        };
        vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: "msg_1".into() },
            LlmEvent::TextDelta {
                id: "msg_1".into(),
                delta: "Let me ".into(),
            },
            LlmEvent::TextDelta {
                id: "msg_1".into(),
                delta: "check.".into(),
            },
            LlmEvent::TextEnd {
                id: "msg_1".into(),
                phase: None,
            },
            LlmEvent::ToolInputStart {
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_1".into(),
                delta: "{\"city\":".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_1".into(),
                delta: "\"NYC\"}".into(),
            },
            LlmEvent::ToolInputEnd {
                id: "call_1".into(),
            },
            LlmEvent::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                namespace: None,
                input: serde_json::json!({ "city": "NYC" }),
            },
            LlmEvent::StepFinish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
            LlmEvent::Finish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
        ]
    }

    #[test]
    fn decode_chunks_openai_responses_whole() {
        let events = decode_chunks(
            OpenAiResponsesProtocol::new().decoder(),
            &[OPENAI_RESPONSES_SSE.as_bytes()],
        )
        .unwrap();
        assert_eq!(events, expected_events());
    }

    #[test]
    fn decode_chunks_is_split_invariant() {
        // Feeding the same bytes split mid-frame must yield the same events,
        // exercising the SseDecoder's cross-chunk buffering through the pipeline.
        let bytes = OPENAI_RESPONSES_SSE.as_bytes();
        let whole = expected_events();
        for split in [1usize, 17, 40, 63, 130, bytes.len().saturating_sub(1)] {
            let split = split.min(bytes.len());
            let (a, b) = bytes.split_at(split);
            let parts = decode_chunks(OpenAiResponsesProtocol::new().decoder(), &[a, b]).unwrap();
            assert_eq!(parts, whole, "split at {split} diverged");
        }
    }

    #[test]
    fn decode_chunks_byte_at_a_time() {
        // Maximally pathological chunking: one byte per chunk.
        let bytes = OPENAI_RESPONSES_SSE.as_bytes();
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        let events = decode_chunks(OpenAiResponsesProtocol::new().decoder(), &chunks).unwrap();
        assert_eq!(events, expected_events());
    }

    #[test]
    fn aggregate_builds_response_from_events() {
        let events = decode_chunks(
            OpenAiResponsesProtocol::new().decoder(),
            &[OPENAI_RESPONSES_SSE.as_bytes()],
        )
        .unwrap();
        let resp = aggregate(events);

        // Text content part with the concatenated deltas.
        assert!(resp
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text == "Let me check.")));
        // The tool call survives aggregation with parsed input.
        let tc = resp
            .content
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("tool call present");
        assert_eq!(tc.0, "get_weather");
        assert_eq!(tc.1, serde_json::json!({ "city": "NYC" }));
        assert_eq!(resp.usage.input_tokens, 11);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.usage.total_tokens, 18);
        assert_eq!(resp.finish_reason, Some(FinishReason::ToolUse));
    }

    #[test]
    fn decode_chunks_propagates_decode_error() {
        // A malformed JSON data frame must surface as a typed Decode error.
        let bad = b"event: response.output_text.delta\ndata: {not json\n\n";
        let err = decode_chunks(OpenAiResponsesProtocol::new().decoder(), &[bad]).unwrap_err();
        assert_eq!(err.reason, LlmErrorReason::Decode);
    }

    // --- retry policy ----------------------------------------------------

    fn fixed_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            jitter_seed: None,
        }
    }

    #[test]
    fn plan_succeeds_on_first_attempt() {
        let plan = fixed_policy().plan(&[Outcome::Success]);
        assert_eq!(plan.attempts, 1);
        assert!(plan.delays.is_empty());
        assert_eq!(plan.outcome, PlanOutcome::Success);
    }

    #[test]
    fn plan_retries_then_succeeds_with_exponential_delays() {
        let outcomes = vec![
            Outcome::fail(LlmError::transport("net")),
            Outcome::fail(LlmError::provider_internal("500")),
            Outcome::Success,
        ];
        let plan = fixed_policy().plan(&outcomes);
        assert_eq!(plan.attempts, 3);
        // base * 2^0, base * 2^1 => 100ms, 200ms.
        assert_eq!(
            plan.delays,
            vec![Duration::from_millis(100), Duration::from_millis(200)]
        );
        assert_eq!(plan.outcome, PlanOutcome::Success);
    }

    #[test]
    fn plan_stops_immediately_on_non_retryable() {
        let outcomes = vec![Outcome::fail(LlmError::authentication("401"))];
        let plan = fixed_policy().plan(&outcomes);
        assert_eq!(plan.attempts, 1);
        assert!(plan.delays.is_empty());
        match plan.outcome {
            PlanOutcome::Failed(err) => assert_eq!(err.reason, LlmErrorReason::Authentication),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn plan_exhausts_budget_on_persistent_failure() {
        let err = || Outcome::fail(LlmError::provider_internal("500"));
        let outcomes = vec![err(), err(), err(), err(), err()];
        let plan = fixed_policy().plan(&outcomes);
        assert_eq!(plan.attempts, 5);
        // 4 retries => 4 delays: 100, 200, 400, 800.
        assert_eq!(
            plan.delays,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800),
            ]
        );
        match plan.outcome {
            PlanOutcome::Failed(err) => assert_eq!(err.reason, LlmErrorReason::ProviderInternal),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn plan_honors_retry_after_hint() {
        let outcomes = vec![
            Outcome::fail_after(LlmError::rate_limit("429"), 2500),
            Outcome::Success,
        ];
        let plan = fixed_policy().plan(&outcomes);
        assert_eq!(plan.attempts, 2);
        // Server hint wins over exponential backoff.
        assert_eq!(plan.delays, vec![Duration::from_millis(2500)]);
        assert_eq!(plan.outcome, PlanOutcome::Success);
    }

    #[test]
    fn backoff_clamps_to_max_delay() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(5),
            jitter_seed: None,
        };
        // 1000ms * 2^10 -> clamps to 5s.
        assert_eq!(policy.backoff(10, None), Duration::from_secs(5));
        // Server hint also clamped.
        assert_eq!(
            policy.backoff(0, Some(Duration::from_secs(60))),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn backoff_jitter_is_deterministic_and_bounded() {
        let policy = RetryPolicy {
            jitter_seed: Some(42),
            ..fixed_policy()
        };
        let a = policy.backoff(1, None);
        let b = policy.backoff(1, None);
        assert_eq!(a, b, "jitter must be reproducible");
        // base*2^1 = 200ms; jitter in [0, base=100ms) => [200ms, 300ms).
        assert!(a >= Duration::from_millis(200), "below base: {a:?}");
        assert!(a < Duration::from_millis(300), "exceeds bound: {a:?}");
    }

    // --- rate-limit header parsing --------------------------------------

    #[test]
    fn parses_openai_rate_limit_headers() {
        let mut h = BTreeMap::new();
        h.insert("x-ratelimit-limit-requests".to_string(), "5000".to_string());
        h.insert(
            "x-ratelimit-remaining-requests".to_string(),
            "4999".to_string(),
        );
        h.insert("x-ratelimit-limit-tokens".to_string(), "160000".to_string());
        h.insert(
            "x-ratelimit-remaining-tokens".to_string(),
            "159000".to_string(),
        );
        h.insert("x-ratelimit-reset-requests".to_string(), "12ms".to_string());
        h.insert("retry-after-ms".to_string(), "1500".to_string());
        let info = RateLimitInfo::from_headers(&h);
        assert_eq!(info.limit_requests, Some(5000));
        assert_eq!(info.remaining_requests, Some(4999));
        assert_eq!(info.limit_tokens, Some(160000));
        assert_eq!(info.remaining_tokens, Some(159000));
        assert_eq!(info.reset_requests.as_deref(), Some("12ms"));
        assert_eq!(info.retry_after_ms, Some(1500));
    }

    #[test]
    fn parses_anthropic_rate_limit_headers() {
        let mut h = BTreeMap::new();
        h.insert(
            "anthropic-ratelimit-requests-limit".to_string(),
            "50".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-requests-remaining".to_string(),
            "49".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-tokens-limit".to_string(),
            "40000".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-tokens-remaining".to_string(),
            "39000".to_string(),
        );
        h.insert(
            "anthropic-ratelimit-tokens-reset".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        );
        h.insert("retry-after".to_string(), "3".to_string());
        let info = RateLimitInfo::from_headers(&h);
        assert_eq!(info.limit_requests, Some(50));
        assert_eq!(info.remaining_requests, Some(49));
        assert_eq!(info.limit_tokens, Some(40000));
        assert_eq!(info.remaining_tokens, Some(39000));
        assert_eq!(info.reset_tokens.as_deref(), Some("2026-01-01T00:00:00Z"));
        // retry-after seconds -> milliseconds.
        assert_eq!(info.retry_after_ms, Some(3000));
    }

    #[test]
    fn retry_after_ms_takes_precedence_over_seconds() {
        let mut h = BTreeMap::new();
        h.insert("retry-after".to_string(), "10".to_string());
        h.insert("retry-after-ms".to_string(), "250".to_string());
        let info = RateLimitInfo::from_headers(&h);
        assert_eq!(info.retry_after_ms, Some(250));
    }

    #[test]
    fn missing_headers_parse_to_none() {
        let info = RateLimitInfo::from_headers(&BTreeMap::new());
        assert_eq!(info, RateLimitInfo::default());
    }

    // --- secret redaction ------------------------------------------------

    #[test]
    fn redacts_authorization_and_api_key() {
        let headers = vec![
            (
                "Authorization".to_string(),
                "Bearer sk-secret-123".to_string(),
            ),
            ("x-api-key".to_string(), "sk-ant-secret".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let red = redact_headers(&headers);
        let get = |k: &str| {
            red.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Authorization"), Some("<redacted>"));
        assert_eq!(get("x-api-key"), Some("<redacted>"));
        assert_eq!(get("content-type"), Some("application/json"));
        // The secret value must not appear anywhere in the redacted view.
        let dump = format!("{red:?}");
        assert!(!dump.contains("sk-secret-123"), "leaked bearer token");
        assert!(!dump.contains("sk-ant-secret"), "leaked api key");
    }

    #[test]
    fn scrub_removes_bearer_tokens_from_messages() {
        let scrubbed = scrub("connect error to Bearer sk-leak-9 endpoint");
        assert!(!scrubbed.contains("sk-leak-9"));
        assert!(scrubbed.contains("Bearer <redacted>"));
    }

    #[test]
    fn route_debug_omits_auth() {
        let route = Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new("https://api.example.com/v1", "/responses"),
            Auth::bearer("sk-super-secret"),
        );
        let dump = format!("{route:?}");
        assert!(
            !dump.contains("sk-super-secret"),
            "Route Debug leaked token"
        );
    }

    #[test]
    fn rate_limit_error_message_is_clean() {
        // The error message is the plain HTTP snippet; the server Retry-After
        // hint is carried structurally (via Outcome / the retry loop), never
        // smuggled into the message or leaked through Display.
        let err = LlmError::rate_limit("HTTP 429: slow down");
        assert_eq!(err.message, "HTTP 429: slow down");
        let shown = format!("{err}");
        assert!(!shown.contains("retry_after_ms="), "leaked hint: {shown}");
    }

    // --- status -> error mapping ----------------------------------------

    #[test]
    fn maps_status_codes_to_typed_errors() {
        assert_eq!(
            error_for_status(401, "").reason,
            LlmErrorReason::Authentication
        );
        assert_eq!(
            error_for_status(403, "").reason,
            LlmErrorReason::Authentication
        );
        let rl = error_for_status(429, "slow down");
        assert_eq!(rl.reason, LlmErrorReason::RateLimit);
        assert_eq!(rl.status, Some(429));
        assert_eq!(
            error_for_status(500, "").reason,
            LlmErrorReason::ProviderInternal
        );
        assert_eq!(
            error_for_status(503, "").reason,
            LlmErrorReason::ProviderInternal
        );
        assert_eq!(
            error_for_status(400, "bad").reason,
            LlmErrorReason::InvalidRequest
        );
    }

    #[test]
    fn error_retryability_matches_taxonomy() {
        assert!(is_retryable(&LlmError::transport("x")));
        assert!(is_retryable(&LlmError::provider_internal("x")));
        assert!(is_retryable(&LlmError::rate_limit("x")));
        assert!(!is_retryable(&LlmError::authentication("x")));
        assert!(!is_retryable(&LlmError::invalid_request("x")));
        assert!(!is_retryable(&LlmError::new(LlmErrorReason::Decode, "x")));
    }

    // --- async path smoke test (no live provider call) ------------------

    fn spawn_idle_sse_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .expect("write headers");
            thread::sleep(Duration::from_millis(200));
        });
        (format!("http://{addr}/v1"), handle)
    }

    fn spawn_no_response_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(200));
        });
        (format!("http://{addr}/v1"), handle)
    }

    fn spawn_timeout_then_sse_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buf).expect("read request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                if attempt == 0 {
                    thread::spawn(move || {
                        let _keep_open = stream;
                        thread::sleep(Duration::from_millis(100));
                    });
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                        .expect("write headers");
                }
            }
        });
        (format!("http://{addr}/v1"), handle)
    }

    #[tokio::test]
    async fn stream_open_timeout_yields_retryable_transport_error() {
        let (base_url, handle) = spawn_no_response_server();
        let client = ModelClient::with_retry(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        })
        .with_request_timeout(Duration::from_millis(20));
        let route = Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new(base_url, "/responses"),
            Auth::bearer("sk-not-used"),
        );
        let mut req = LlmRequest::new("gpt-5.1-codex", "openai");
        req.messages.push(crate::schema::Message::user_text("hi"));

        let err = match client.stream(&route, &req).await {
            Ok(_) => panic!("request-open timeout should fail before stream opens"),
            Err(err) => err,
        };
        handle.join().expect("idle server thread");

        assert_eq!(err.reason, LlmErrorReason::Transport);
        assert!(err.retryable);
        assert!(
            err.message.contains("model request timeout after 20ms"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn stream_open_timeout_is_per_attempt_not_whole_retry_loop() {
        let (base_url, handle) = spawn_timeout_then_sse_server();
        let client = ModelClient::with_retry(RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter_seed: None,
        })
        .with_request_timeout(Duration::from_millis(20));
        let route = Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new(base_url, "/responses"),
            Auth::bearer("sk-not-used"),
        );
        let mut req = LlmRequest::new("gpt-5.1-codex", "openai");
        req.messages.push(crate::schema::Message::user_text("hi"));

        let _stream = client
            .stream(&route, &req)
            .await
            .expect("second request-open attempt should succeed");
        handle.join().expect("server thread");
    }

    #[tokio::test]
    async fn stream_idle_timeout_yields_retryable_transport_error() {
        let (base_url, handle) = spawn_idle_sse_server();
        let client = ModelClient::with_retry(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        })
        .with_stream_idle_timeout(Duration::from_millis(20));
        let route = Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new(base_url, "/responses"),
            Auth::bearer("sk-not-used"),
        );
        let mut req = LlmRequest::new("gpt-5.1-codex", "openai");
        req.messages.push(crate::schema::Message::user_text("hi"));

        let mut stream = client.stream(&route, &req).await.expect("open stream");
        let err = stream
            .next()
            .await
            .expect("idle stream should yield an error")
            .expect_err("idle stream should fail");
        handle.join().expect("idle server thread");

        assert_eq!(err.reason, LlmErrorReason::Transport);
        assert!(err.retryable);
        assert!(
            err.message.contains("model stream idle timeout after 20ms"),
            "{err}"
        );
    }

    /// Exercises the real async stack — the tokio runtime, `generate`, the
    /// `send_with_retry` → `send_once` path, and transport-error mapping —
    /// without contacting any LLM provider. The endpoint is a reserved-TLD host
    /// that never resolves, so the request fails locally at the transport layer.
    /// `max_attempts: 1` keeps it fast and sleep-free.
    #[tokio::test]
    async fn generate_maps_transport_failure_without_live_call() {
        let client = ModelClient::with_retry(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        // `.invalid` is a reserved TLD (RFC 6761) guaranteed not to resolve.
        let route = Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new("http://host.invalid", "/v1/responses"),
            Auth::bearer("sk-not-used"),
        );
        let mut req = LlmRequest::new("gpt-5.1-codex", "openai");
        req.messages.push(crate::schema::Message::user_text("hi"));

        let err = client.generate(&route, &req).await.unwrap_err();
        assert_eq!(err.reason, LlmErrorReason::Transport);
        // The bearer token must never leak into the transport error message.
        assert!(!err.message.contains("sk-not-used"), "leaked token: {err}");
    }
}
