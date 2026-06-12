//! Tool registry: name -> handler dispatch for the async agent engine.
//!
//! This is the seam STATUS.md flagged as missing: turning a model-emitted
//! `ContentPart::ToolCall { name, input }` into a concrete, typed tool
//! execution. The typed [`ToolRuntime<Req, Out>`](crate::tools::ToolRuntime)
//! trait is generic and therefore not object-safe, so the registry stores
//! type-erased [`DynTool`] trait objects. A blanket [`ToolAdapter`] wraps any
//! typed handler whose `Req` is [`serde::de::DeserializeOwned`] and whose `Out`
//! is [`ExecOutput`], deserializing the call's `input` value into the handler's
//! `Req` and running it THROUGH the [`ToolOrchestrator`] so the uniform
//! approval/sandbox policy still applies.
//!
//! ## Why metadata is supplied at registration, not read from the handler
//!
//! The handlers (`tools/handlers/*.rs`) implement only `ToolRuntime`: they carry
//! NO model-facing `name`/`description`/`schema`, and their `parallel_safe`
//! takes a `&Req` (it is per-request, not a static property). The registry
//! therefore takes the advertised name, the [`ToolDefinition`], and a static
//! `parallel_safe` flag at registration time. This matches codex, where the
//! registry maps an advertised name to `(handler, spec)`: the spec is not
//! derived from the handler trait either.
//!
//! ## Default handlers register (registry gap closed)
//!
//! [`register`](ToolRegistry::register) requires the handler's `Req` to be
//! [`DeserializeOwned`] so the registry can build it from the model-emitted JSON
//! `input`. The default callable handlers satisfy that, so they register via the
//! single [`register`](ToolRegistry::register) path:
//!
//! * TEN have a `Req` that maps DIRECTLY to the model's argument object: the
//!   three originally-`Deserialize` tools (`update_plan`, `tool_search`,
//!   `web_search`) plus `shell` ([`ShellRequest`]),
//!   `apply_patch` ([`ApplyPatchRequest`]), `view_image` ([`ViewImageRequest`]),
//!   `python` ([`PythonRequest`]), `done`, and the unified exec helpers — each
//!   now derives `serde::Deserialize` with `#[serde(default)]` on the
//!   carried-but-optional plumbing fields so the MODEL's argument object
//!   deserializes cleanly.
//! * TWO carry a PARSED / namespaced `Req` that is not a direct match for the
//!   model's JSON: `browser` ([`BrowserRequest`]) and `mcp`
//!   ([`McpToolCallRequest`]). Each defines a small `Deserialize`-able wire-args
//!   struct ([`BrowserWireArgs`] / [`McpWireArgs`]) that matches the model object,
//!   plus an `impl From<Wire> for Req`, and the `Req` deserializes THROUGH it via
//!   `#[serde(from = "…WireArgs")]`. That makes the `Req` itself `Deserialize`, so
//!   it registers through the same `register` path (no separate adapter needed).
//!
//! See [`crate::tools::registry::definitions`] for the per-tool
//! [`ToolDefinition`] (name + description + input schema) supplied at
//! registration, and [`default_registry`] for a registry preloaded with the
//! default callable tool set.
//!
//! [`ShellRequest`]: crate::tools::handlers::shell::ShellRequest
//! [`ApplyPatchRequest`]: crate::tools::handlers::apply_patch::ApplyPatchRequest
//! [`ViewImageRequest`]: crate::tools::handlers::view_image::ViewImageRequest
//! [`BrowserRequest`]: crate::tools::handlers::browser::BrowserRequest
//! [`BrowserWireArgs`]: crate::tools::handlers::browser::BrowserWireArgs
//! [`PythonRequest`]: crate::tools::handlers::python::PythonRequest
//! [`McpToolCallRequest`]: crate::tools::handlers::mcp::McpToolCallRequest
//! [`McpWireArgs`]: crate::tools::handlers::mcp::McpWireArgs
//!
//! ## Parity
//!
//! Mirrors codex-rs `core/src/tools/registry.rs` (the
//! `ToolRegistry { handlers: HashMap<String, Arc<dyn ToolHandler>> }` keyed by
//! the advertised tool name, with `handler(name)` lookup and a `specs()`
//! exposure of the model-visible tool list) and `core/src/tools/router.rs`
//! (`dispatch_tool_call`: look the handler up by name, error if unknown, then
//! run it through the orchestrator). The type-erased codex `trait ToolHandler`
//! with `async fn handle(...)` (codex `core/src/tools/handlers/mod.rs`) is the
//! direct analogue of our [`DynTool`]. We use trait objects rather than codex's
//! `ToolKind` enum-match — and rather than the legacy `browser-use-core`
//! `ToolHandlerKind` enum registry with its `model_visible_specs` /
//! `deferred_tool_search_entries`
//! (`browser-use-core/src/tools/mod.rs`) — because the trait-object form is the
//! design `docs/agent-design/DESIGN.md` prescribes for the dispatch loop
//! ("registry, name->handler routing, parallel/serial", DESIGN.md:34).

use std::collections::BTreeMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use browser_use_llm::schema::ToolDefinition;
use serde::de::DeserializeOwned;

use crate::tools::approval::AskForApproval;
use crate::tools::handlers::tool_search::ToolSearchEntry;
use crate::tools::orchestrator::TurnEnv;
use crate::tools::runtime::{Approvable, AutoApprover, ToolRuntime};
use crate::tools::sandbox::{NoneSandboxProvider, SandboxProvider};
use crate::tools::{Approver, ExecOutput, ToolCtx, ToolError, ToolOrchestrator};

/// A type-erased tool handler the registry can dispatch to by name.
///
/// Erases each concrete handler's `Req` so a heterogeneous set of typed
/// [`ToolRuntime`] implementations (each with its own `ShellRequest`,
/// `ApplyPatchRequest`, …) can live behind one `Box<dyn DynTool>`. Generic over
/// the orchestrator's sandbox/approver seams `(S, A)` — defaulting to the
/// `None`/auto seams — so `call` can route through a concrete
/// [`ToolOrchestrator<S, A>`] while the trait object itself stays object-safe
/// (the type params are fixed per registry).
///
/// Analogous to codex's `trait ToolHandler` (codex
/// `core/src/tools/handlers/mod.rs`).
#[async_trait]
pub trait DynTool<S = NoneSandboxProvider, A = AutoApprover>: Send + Sync
where
    S: SandboxProvider,
    A: Approver,
{
    /// Stable snake_case tool name as advertised to the model.
    fn name(&self) -> &str;

    /// Provider-neutral definition (name + description + input schema) the
    /// engine exposes to the model.
    fn definition(&self) -> ToolDefinition;

    /// Whether this tool may run in parallel with other parallel-safe tools.
    fn parallel_safe(&self) -> bool;

    /// Run the tool from an erased JSON `input`, routing THROUGH the
    /// orchestrator so approval/sandbox policy still applies.
    ///
    /// Deserializes `input` into the concrete handler's `Req`; a deserialize
    /// failure surfaces as [`ToolError::Other`] naming the offending tool.
    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError>;

    async fn call_with_cancel(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, ToolError>;
}

/// Adapter that lifts a typed [`ToolRuntime<Req, ExecOutput>`] into a
/// [`DynTool`].
///
/// The orchestrator's `run` is generic over the handler's `Req`; this adapter
/// pins `Req` (and `Out = ExecOutput`) so the resulting `DynTool` is
/// object-safe. It also carries the model-facing metadata the handler trait
/// does not provide: the advertised `name`, the [`ToolDefinition`], and a static
/// `parallel_safe` flag.
pub struct ToolAdapter<T, Req> {
    tool: T,
    name: String,
    definition: ToolDefinition,
    parallel_safe: bool,
    _req: PhantomData<fn() -> Req>,
}

impl<T, Req> ToolAdapter<T, Req> {
    /// Wrap a typed handler with the metadata the registry advertises for it.
    pub fn new(
        tool: T,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
    ) -> Self {
        Self {
            tool,
            name: name.into(),
            definition,
            parallel_safe,
            _req: PhantomData,
        }
    }
}

#[async_trait]
impl<T, Req, S, A> DynTool<S, A> for ToolAdapter<T, Req>
where
    Req: DeserializeOwned + Send + Sync,
    T: ToolRuntime<Req, ExecOutput> + Send + Sync,
    // The orchestrator's `run` future holds a slice of the tool's approval keys
    // across the `.await`; it is only `Send` if the key type is `Send + Sync`.
    // Every handler's `ApprovalKey` is a plain owned value type, so this bound
    // holds for all 10 tools.
    <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    S: SandboxProvider,
    A: Approver,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn parallel_safe(&self) -> bool {
        self.parallel_safe
    }

    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        let req: Req = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        // Route through the orchestrator so approval/sandbox/escalation policy
        // applies uniformly (parity with codex router.rs, where dispatch goes
        // through the handler under the orchestrator's policy wrapper).
        let result = orchestrator.run(&self.tool, &req, ctx, env, policy).await?;
        Ok(result.output)
    }

    async fn call_with_cancel(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, ToolError> {
        let req: Req = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        let result = orchestrator
            .run_with_cancel(&self.tool, &req, ctx, env, policy, cancel)
            .await?;
        Ok(result.output)
    }
}

/// Adapter that lifts a typed [`ToolRuntime<Req, ExecOutput>`] into a [`DynTool`]
/// when the MODEL's wire arguments are a DIFFERENT type `Wire` than the handler's
/// `Req`, bridged by `Wire: Into<Req>`.
///
/// Two handlers (`browser`, `mcp`) take a PARSED / namespaced `Req` that is not a
/// direct match for the model's JSON argument object (the browser `Req` carries a
/// tagged [`BrowserAction`] enum; the mcp `Req` carries `server`/`tool` split out
/// of the namespaced function NAME). For those, we deserialize a small
/// `Wire`-args struct that matches the model JSON and convert it into the typed
/// `Req` via [`From`]. The orchestrator still runs the typed `Req`, so
/// approval/sandbox policy and behavior are unchanged.
///
/// [`BrowserAction`]: crate::tools::handlers::browser::BrowserAction
pub struct WireToolAdapter<T, Wire, Req> {
    tool: T,
    name: String,
    definition: ToolDefinition,
    parallel_safe: bool,
    _wire: PhantomData<fn() -> Wire>,
    _req: PhantomData<fn() -> Req>,
}

impl<T, Wire, Req> WireToolAdapter<T, Wire, Req> {
    /// Wrap a typed handler whose model wire args are `Wire` (convertible into
    /// the handler's `Req`).
    pub fn new(
        tool: T,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
    ) -> Self {
        Self {
            tool,
            name: name.into(),
            definition,
            parallel_safe,
            _wire: PhantomData,
            _req: PhantomData,
        }
    }
}

#[async_trait]
impl<T, Wire, Req, S, A> DynTool<S, A> for WireToolAdapter<T, Wire, Req>
where
    Wire: DeserializeOwned + Send + Sync + Into<Req>,
    Req: Send + Sync,
    T: ToolRuntime<Req, ExecOutput> + Send + Sync,
    <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    S: SandboxProvider,
    A: Approver,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn parallel_safe(&self) -> bool {
        self.parallel_safe
    }

    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        // Deserialize the MODEL's wire-args type, then convert into the handler's
        // parsed `Req`. A deserialize failure surfaces as `ToolError::Other`
        // naming the offending tool (same shape as `ToolAdapter::call`).
        let wire: Wire = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        let req: Req = wire.into();
        let result = orchestrator.run(&self.tool, &req, ctx, env, policy).await?;
        Ok(result.output)
    }

    async fn call_with_cancel(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, ToolError> {
        let wire: Wire = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        let req: Req = wire.into();
        let result = orchestrator
            .run_with_cancel(&self.tool, &req, ctx, env, policy, cancel)
            .await?;
        Ok(result.output)
    }
}

/// Registry of tool handlers keyed by the name advertised to the model.
///
/// Parity: codex `ToolRegistry { handlers: HashMap<String, Arc<dyn ToolHandler>> }`.
/// We key on a [`BTreeMap`] for deterministic ordering of model-visible
/// definitions for free. Generic over the orchestrator seams `(S, A)`, defaulting
/// to the `None`/auto seams.
pub struct ToolRegistry<S = NoneSandboxProvider, A = AutoApprover>
where
    S: SandboxProvider,
    A: Approver,
{
    tools: BTreeMap<ToolKey, Box<dyn DynTool<S, A>>>,
    deferred: Vec<ToolSearchEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ToolKey {
    namespace: Option<String>,
    name: String,
}

impl<S, A> Default for ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    fn default() -> Self {
        Self {
            tools: BTreeMap::new(),
            deferred: Vec::new(),
        }
    }
}

impl<S, A> ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed handler under `name`, with its model-facing definition
    /// and static `parallel_safe` flag.
    ///
    /// The handler is wrapped in a [`ToolAdapter`] and stored type-erased. A
    /// later registration with the same name replaces the earlier one (last
    /// write wins), matching codex's `HashMap::insert` semantics.
    pub fn register<T, Req>(
        &mut self,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
        tool: T,
    ) where
        Req: DeserializeOwned + Send + Sync + 'static,
        T: ToolRuntime<Req, ExecOutput> + Send + Sync + 'static,
        <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    {
        let name = name.into();
        let adapter = ToolAdapter::<T, Req>::new(tool, name.clone(), definition, parallel_safe);
        let key = ToolKey::new(adapter.definition.namespace.as_deref(), &name);
        self.tools.insert(key, Box::new(adapter));
    }

    /// Register an already-erased [`DynTool`] under its own `name()`.
    pub fn register_dyn(&mut self, tool: Box<dyn DynTool<S, A>>) {
        let definition = tool.definition();
        let key = ToolKey::new(definition.namespace.as_deref(), tool.name());
        self.tools.insert(key, tool);
    }

    /// Look up a handler by name.
    pub fn get(&self, name: &str) -> Option<&dyn DynTool<S, A>> {
        self.tools.get(&ToolKey::plain(name)).map(|b| b.as_ref())
    }

    /// Look up a handler by Responses namespace + function name.
    pub fn get_namespaced(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<&dyn DynTool<S, A>> {
        self.tools
            .get(&ToolKey::new(namespace, name))
            .map(|b| b.as_ref())
    }

    /// Whether a handler is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(&ToolKey::plain(name))
    }

    /// Whether a handler is registered under a namespace + name.
    pub fn contains_namespaced(&self, namespace: Option<&str>, name: &str) -> bool {
        self.tools.contains_key(&ToolKey::new(namespace, name))
    }

    /// Number of registered handlers.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry holds no handlers.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Retain only registered tools accepted by `keep`.
    pub fn retain_registered_tools(&mut self, mut keep: impl FnMut(Option<&str>, &str) -> bool) {
        self.tools
            .retain(|key, _| keep(key.namespace.as_deref(), &key.name));
    }

    /// Set the deferred tool catalog that `tool_search` searches over.
    ///
    /// Parity: legacy `browser-use-core` exposed these via
    /// `deferred_tool_search_entries`; codex likewise keeps the deferred set out
    /// of the always-on spec list. The entries are the same
    /// [`ToolSearchEntry`] catalog the `tool_search` handler is constructed with.
    pub fn set_deferred_search_entries(&mut self, entries: Vec<ToolSearchEntry>) {
        self.deferred = entries;
    }

    /// The deferred tool catalog entries fed to `tool_search`.
    ///
    /// Parity: legacy `browser-use-core::deferred_tool_search_entries`.
    pub fn deferred_search_entries(&self) -> &[ToolSearchEntry] {
        &self.deferred
    }

    /// Model-visible tool definitions for every registered handler, in
    /// deterministic (name-sorted) order.
    ///
    /// Parity: codex `ToolRegistry::specs()` /
    /// legacy `browser-use-core::model_visible_specs`.
    pub fn model_visible_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Whether a registered tool is parallel-safe; `None` if not registered.
    ///
    /// The dispatcher feeds this into its parallel/serial gate
    /// (`turn::decision`).
    pub fn parallel_safe(&self, name: &str) -> Option<bool> {
        self.tools
            .get(&ToolKey::plain(name))
            .map(|t| t.parallel_safe())
    }

    /// Whether a namespaced registered tool is parallel-safe.
    pub fn parallel_safe_namespaced(&self, namespace: Option<&str>, name: &str) -> Option<bool> {
        self.tools
            .get(&ToolKey::new(namespace, name))
            .map(|t| t.parallel_safe())
    }

    /// Dispatch a tool call by name, routing through the orchestrator.
    ///
    /// Parity: codex `router.rs::dispatch_tool_call` (look up the handler by
    /// name, error if unknown, then run it under the orchestrator's policy).
    /// An unknown name is a [`ToolError::Other`] naming the missing tool.
    pub async fn dispatch(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        self.dispatch_with_cancel(
            name,
            input,
            ctx,
            env,
            policy,
            orchestrator,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    /// Dispatch a tool call by name with a live turn cancellation token.
    pub async fn dispatch_with_cancel(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("unknown tool `{name}`")))?;
        tool.call_with_cancel(input, ctx, env, policy, orchestrator, cancel)
            .await
    }

    /// Dispatch a Responses namespace function call.
    pub async fn dispatch_namespaced(
        &self,
        namespace: Option<&str>,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        let tool = self.get_namespaced(namespace, name).ok_or_else(|| {
            ToolError::Other(anyhow::anyhow!(
                "unknown tool `{}`",
                ToolKey::new(namespace, name)
            ))
        })?;
        tool.call(input, ctx, env, policy, orchestrator).await
    }

    /// Dispatch a Responses namespace function call with a live turn
    /// cancellation token.
    pub async fn dispatch_namespaced_with_cancel(
        &self,
        namespace: Option<&str>,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, ToolError> {
        let tool = self.get_namespaced(namespace, name).ok_or_else(|| {
            ToolError::Other(anyhow::anyhow!(
                "unknown tool `{}`",
                ToolKey::new(namespace, name)
            ))
        })?;
        tool.call_with_cancel(input, ctx, env, policy, orchestrator, cancel)
            .await
    }
}

impl ToolKey {
    fn new(namespace: Option<&str>, name: &str) -> Self {
        Self {
            namespace: namespace.map(ToOwned::to_owned),
            name: name.to_string(),
        }
    }

    fn plain(name: &str) -> Self {
        Self::new(None, name)
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(namespace) => write!(f, "{namespace}{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

impl<S, A> std::fmt::Debug for ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("deferred", &self.deferred.len())
            .finish()
    }
}

/// Model-facing [`ToolDefinition`] builders.
///
/// The registry takes a tool's name + description + input schema at registration
/// (it does NOT derive them from the handler trait — see the module docs). These
/// builders centralize the parity-grounded schema shape for each tool so the
/// dispatch loop (and tests) register a consistent definition. Each schema is a
/// JSON-Schema object mirroring the codex/legacy tool spec the field names come
/// from. Field names match the handlers' `Req` / wire-args structs.
pub mod definitions {
    use browser_use_llm::schema::ToolDefinition;
    use serde_json::json;
    use serde_json::Map;
    use serde_json::Value;

    pub const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
    const MULTI_AGENT_V1_NAMESPACE_DESCRIPTION: &str =
        "Tools for spawning and managing sub-agents.";
    const SPAWN_AGENT_INHERITED_MODEL_GUIDANCE: &str = "Spawned agents inherit your current model by default. Omit `model` to use that preferred default; set `model` only when an explicit override is needed. If you set `agent_type`, `model`, or `reasoning_effort`, also set `fork_turns` to `none` or a positive integer; full-history forks inherit those fields.";
    const SPAWN_AGENT_MODEL_OVERRIDE_DESCRIPTION: &str = "Optional model override for the new agent. Leave unset to inherit the same model as the parent, which is the preferred default. Only set this when the user explicitly asks for a different model or the task clearly requires one.";
    const SPAWN_AGENT_SERVICE_TIER_OVERRIDE_DESCRIPTION: &str =
        "Optional service tier override for the new agent. Leave unset unless the user explicitly asks for one.";

    #[derive(Clone, Debug)]
    pub struct SpawnAgentDefinitionOptions {
        pub agent_type_description: String,
        pub available_models_description: Option<String>,
        pub hide_agent_type_model_reasoning: bool,
        pub include_usage_hint: bool,
        pub usage_hint_text: Option<String>,
        pub max_concurrent_threads_per_session: Option<usize>,
        pub is_spawned_subagent: bool,
    }

    impl Default for SpawnAgentDefinitionOptions {
        fn default() -> Self {
            Self {
                agent_type_description:
                    "Optional role for the new agent. If omitted, `default` is used.".to_string(),
                available_models_description: Some(
                    "No picker-visible model overrides are currently loaded.".to_string(),
                ),
                hide_agent_type_model_reasoning: false,
                include_usage_hint: false,
                usage_hint_text: None,
                max_concurrent_threads_per_session: None,
                is_spawned_subagent: false,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct WaitAgentDefinitionOptions {
        pub default_timeout_ms: i64,
        pub min_timeout_ms: i64,
        pub max_timeout_ms: i64,
    }

    /// `get_goal`: report the current thread goal + token-budget usage. Parity:
    /// codex goal-spec read tool (`goal_spec.rs` / `spec_plan.rs`).
    pub fn get_goal() -> ToolDefinition {
        ToolDefinition {
            name: "get_goal".to_string(),
            description: "Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `create_goal`: set the active thread goal (objective + optional token
    /// budget). Parity: codex goal-spec create tool.
    pub fn create_goal() -> ToolDefinition {
        ToolDefinition {
            name: "create_goal".to_string(),
            description: "Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks.\nSet token_budget only when an explicit token budget is requested. Fails if a goal exists; use update_goal only for status.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "Required. The concrete objective to start pursuing. This starts a new active goal only when no goal is currently defined; if a goal already exists, this tool fails."
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Optional positive token budget for the new active goal."
                    }
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `update_goal`: mark the existing goal complete or blocked.
    /// Parity: codex goal-spec update tool.
    pub fn update_goal() -> ToolDefinition {
        ToolDefinition {
            name: "update_goal".to_string(),
            description: "Update the existing goal. Set status to `complete` only when the objective has actually been achieved and no required work remains; set status to `blocked` only when the same blocking condition has repeated for at least three consecutive goal turns (counting the original/user-triggered turn and any automatic continuations, and restarting a fresh audit when a previously blocked goal is resumed) and the agent cannot progress without user input or an external-state change.\nDo not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification; do not mark complete merely because the budget is nearly exhausted or you are stopping; and do not use this tool to pause, resume, budget-limit, or usage-limit a goal (those are controlled by the user or system). When marking a budgeted goal `complete`, report the final token usage from the tool result to the user.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["complete", "blocked"],
                        "description": "Required. Set to `complete` only when the objective is achieved and no required work remains. Set to `blocked` only after the same blocking condition has recurred for at least three consecutive goal turns and the agent is at an impasse. After a previously blocked goal is resumed, the resumed run starts a fresh blocked audit."
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `shell`: argv command + optional cwd/timeout/env. Parity: codex
    /// `ExecParams` (core/src/exec.rs:83-96) / legacy shell spec
    /// (`browser-use-core/src/tools/mod.rs`).
    pub fn shell() -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command (argv-style) and capture its output.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command and arguments, argv-style (first element is the program)."
                    },
                    "cwd": { "type": "string", "description": "Working directory (defaults to the session cwd)." },
                    "timeout_ms": { "type": "integer", "description": "Per-command timeout in milliseconds." },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum number of tokens to return. Excess output will be truncated."
                    },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Extra environment variables for the child process."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `exec_command`: Codex-style process execution with live output snapshots
    /// and a process id when the command is still running.
    pub fn exec_command() -> ToolDefinition {
        ToolDefinition {
            name: "exec_command".to_string(),
            description:
                "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command to execute." },
                    "workdir": { "type": "string", "description": "Optional working directory to run the command in; defaults to the turn cwd." },
                    "shell": { "type": "string", "description": "Shell binary to launch. Defaults to the user's default shell." },
                    "login": { "type": "boolean", "description": "Whether to run the shell with -l/-i semantics. Defaults to true." },
                    "tty": { "type": "boolean", "description": "Whether to allocate a TTY for the command. Defaults to false (plain pipes); set to true to open a PTY and access TTY process." },
                    "yield_time_ms": {
                        "type": "integer",
                        "description": "How long to wait (in milliseconds) for output before yielding."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Maximum wall time in milliseconds before the process tree is killed. Defaults to a generous failsafe cap."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum number of tokens to return. Excess output will be truncated."
                    }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `write_stdin`: send input to or poll a live `exec_command` process.
    pub fn write_stdin() -> ToolDefinition {
        ToolDefinition {
            name: "write_stdin".to_string(),
            description:
                "Send characters to a live command process, or poll it by sending an empty string."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "integer", "description": "Identifier of the running unified exec session." },
                    "chars": { "type": "string", "description": "Characters to write to stdin; empty string polls output." },
                    "yield_time_ms": {
                        "type": "integer",
                        "description": "How long to wait for output after writing."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum number of tokens to return. Excess output will be truncated."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `apply_patch`: a V4A patch envelope. Parity: codex apply-patch
    /// (`apply-patch/src/parser.rs`) / legacy `browser-use-core/src/tools/files.rs`.
    pub fn apply_patch() -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a V4A patch envelope to files under the workspace root."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The full V4A patch text (*** Begin Patch ... *** End Patch)."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `view_image`: a local image path. Parity: codex `ViewImageArgs { path }`
    /// (core/src/tools/handlers/view_image.rs:53-58) / legacy `view_image`
    /// (`files.rs`).
    pub fn view_image() -> ToolDefinition {
        ToolDefinition {
            name: "view_image".to_string(),
            description: "Read a local image file and return it as model-visible image content."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to a local image (png/jpeg/gif/webp)." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `browser`: browser-use's browser control-plane command tool.
    pub fn browser() -> ToolDefinition {
        ToolDefinition {
            name: "browser".to_string(),
            description: crate::prompts::browser_tool_description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "CLI-like browser command. The leading word `browser` is optional, e.g. `status --json` or `browser connect local`."
                    }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `browser_script`: browser-use page/data-plane interaction surface.
    pub fn browser_script() -> ToolDefinition {
        ToolDefinition {
            name: "browser_script".to_string(),
            description: crate::prompts::browser_script_tool_description().to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "observe", "cancel"],
                        "description": "start launches code and returns either a final result or a run_id; observe listens for new output/final status; cancel stops a running script. Defaults to start when code is provided and observe when only run_id is provided."
                    },
                    "code": {
                        "type": "string",
                        "description": "Python code to run in a fresh process with browser helpers preimported. Omit when action is observe or cancel."
                    },
                    "run_id": {
                        "type": "string",
                        "description": "Running browser_script id returned by a previous start call. Required for observe and cancel."
                    },
                    "observe_timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 120000,
                        "description": "How long observe should wait for new output or completion before returning still-running/no-new-output. Defaults to 30000. Use longer windows, up to 120000, for long-running extraction or navigation scripts instead of repeatedly polling the same run_id."
                    }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `submit_capture_curation`: build browser recording summary artifacts from
    /// selected frame seqs.
    pub fn submit_capture_curation() -> ToolDefinition {
        ToolDefinition {
            name: "submit_capture_curation".to_string(),
            description: "Finalize the visual summary of this browser task. Review the capture \
contact sheet you were shown (each pane is labeled with its frame seq) and select the frames \
that best tell the story of what happened, dropping redundant or uninformative ones. For each \
chosen frame give its seq and a short caption, ordered as they should play. Set confirmation_seq \
to the single frame that proves the task succeeded."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "frames": {
                        "type": "array",
                        "description": "Chosen frames in playback order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "seq": { "type": "integer", "description": "Frame seq from the contact sheet." },
                                "caption": { "type": "string", "description": "Short caption for this frame." }
                            },
                            "required": ["seq", "caption"],
                            "additionalProperties": false
                        }
                    },
                    "confirmation_seq": {
                        "type": "integer",
                        "description": "Seq of the frame that confirms task success."
                    }
                },
                "required": ["frames", "confirmation_seq"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `python`: a Python snippet + optional timeout. Parity: legacy
    /// `dispatch_python_tool` (`browser-use-core/src/lib.rs`).
    pub fn python() -> ToolDefinition {
        ToolDefinition {
            name: "python".to_string(),
            description: "Run a Python snippet in a persistent worker and return its output."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The Python source to execute." },
                    "session_id": { "type": "string", "description": "Worker session id (persistent namespace)." },
                    "timeout_secs": { "type": "number", "description": "Optional timeout in seconds." }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `mcp`: a resolved MCP `tools/call`. Parity: legacy
    /// `dispatch_mcp_tool(server, tool, arguments, ..)`
    /// (`browser-use-core/src/lib.rs:13398-13403`) / codex
    /// `core/src/mcp_tool_call.rs`.
    pub fn mcp() -> ToolDefinition {
        ToolDefinition {
            name: "mcp".to_string(),
            description: "Call a tool on a connected MCP server.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "The MCP server name." },
                    "tool": { "type": "string", "description": "The tool name on that server." },
                    "arguments": { "type": "object", "description": "JSON arguments for the call." },
                    "read_only": { "type": "boolean", "description": "Whether the tool is read-only (parallel-safe)." }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `update_plan`: the structured plan. Parity: codex `UpdatePlanArgs`
    /// (core/src/tools/handlers/plan.rs) / legacy `UpdatePlanArgs`.
    pub fn update_plan() -> ToolDefinition {
        ToolDefinition {
            name: "update_plan".to_string(),
            description: "Record a structured task plan the model is tracking.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": true
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `tool_search`: BM25 over the deferred catalog. Parity: legacy
    /// `tool_search` args `{ query, limit? }`
    /// (`browser-use-core/src/tools/mod.rs`).
    pub fn tool_search() -> ToolDefinition {
        ToolDefinition {
            name: "tool_search".to_string(),
            description: "Search the deferred tool catalog by free-text query.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The free-text query." },
                    "limit": { "type": "integer", "description": "Max ranked entries to return." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `done`: the completion tool the model calls to declare the task finished,
    /// carrying its final answer. The handler accepts Browser Use-style
    /// `{ "result"?: string, "result_file"?: string }` and the legacy
    /// `{ "text"?: string }` alias.
    pub fn done() -> ToolDefinition {
        ToolDefinition {
            name: "done".to_string(),
            description:
                "Signal that the user-facing task is complete, carrying the final answer. Call done only after the requested result is complete and verified or persisted. If the remaining step budget is nearly exhausted and a complete answer is no longer possible, call done with an explicitly incomplete fallback result that names the missing requirements."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "The complete final answer to show the user or evaluator. Include all requested data here when the task asks for inline JSON, CSV, markdown, a table, links, or a schema-shaped response."
                    },
                    "text": {
                        "type": "string",
                        "description": "Legacy alias for result. Prefer result for new calls."
                    },
                    "result_file": {
                        "type": "string",
                        "description": "Optional path to a saved final-result artifact when a file pointer satisfies the task or supplements the inline result."
                    }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `web_search`: a hosted/passthrough web search. Parity: codex
    /// `WebSearchArgs { query }` / legacy web_search args.
    pub fn web_search() -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web for a free-text query.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The free-text search query." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `search`: a LOCALLY-executed DuckDuckGo (Lite) web search. Unlike the
    /// hosted [`web_search`](definitions::web_search), the client performs the
    /// HTTP request itself and returns the parsed results as text. Ported from
    /// the Python `search` action's description.
    pub fn search() -> ToolDefinition {
        ToolDefinition {
            name: "search".to_string(),
            description: "Search the web using DuckDuckGo and return results directly as text – \
                 no browser navigation occurs. The returned results are final and complete. \
                 NEVER open a search engine website after calling this action."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to look up on the web."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    fn agent_status_output_schema() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "string",
                    "enum": ["pending_init", "running", "interrupted", "shutdown", "not_found"]
                },
                {
                    "type": "object",
                    "properties": {
                        "completed": { "type": ["string", "null"] }
                    },
                    "required": ["completed"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "errored": { "type": "string" }
                    },
                    "required": ["errored"],
                    "additionalProperties": false
                }
            ]
        })
    }

    fn spawn_agent_output_schema_v2(hide_agent_metadata: bool) -> Value {
        if hide_agent_metadata {
            return json!({
                "type": "object",
                "properties": {
                    "task_name": {
                        "type": "string",
                        "description": "Canonical task name for the spawned agent."
                    }
                },
                "required": ["task_name"],
                "additionalProperties": false
            });
        }
        json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Canonical task name for the spawned agent."
                },
                "nickname": {
                    "type": ["string", "null"],
                    "description": "User-facing nickname for the spawned agent when available."
                }
            },
            "required": ["task_name", "nickname"],
            "additionalProperties": false
        })
    }

    fn spawn_agent_output_schema_v1() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Thread identifier for the spawned agent."
                },
                "nickname": {
                    "type": ["string", "null"],
                    "description": "User-facing nickname for the spawned agent when available."
                }
            },
            "required": ["agent_id", "nickname"],
            "additionalProperties": false
        })
    }

    fn spawned_subagent_concurrency_guidance(limit: Option<usize>) -> String {
        limit
            .map(|limit| {
                let spawned_limit = limit.saturating_sub(1);
                format!(
                    " This session is configured with `max_concurrent_threads_per_session = {limit}`; the root agent counts toward that cap, so at most {spawned_limit} spawned subagent(s) may be open concurrently. If that spawned-subagent cap is already reached, wait for an existing subagent to finish and close it before spawning another, or spawn fewer subagents."
                )
            })
            .unwrap_or_default()
    }

    fn spawn_agent_description_v2(options: &SpawnAgentDefinitionOptions) -> String {
        let available_models_description = if options.hide_agent_type_model_reasoning {
            ""
        } else {
            options
                .available_models_description
                .as_deref()
                .unwrap_or("")
        };
        let concurrency_guidance =
            spawned_subagent_concurrency_guidance(options.max_concurrent_threads_per_session);
        let mut description = format!(
            r#"
        {}
        Spawns an agent to work on the specified task. If your current task is `/root/task1` and you spawn_agent with task_name "task_3" the agent will have canonical task name `/root/task1/task_3`.
You are then able to refer to this agent as `task_3` or `/root/task1/task_3` interchangeably. However an agent `/root/task2/task_3` would only be able to communicate with this agent via its canonical name `/root/task1/task_3`.
The spawned agent will have the same tools as you and the ability to spawn its own subagents.
{SPAWN_AGENT_INHERITED_MODEL_GUIDANCE}
It will be able to send you and other running agents messages, and its final answer will be provided to you when it finishes.
The new agent's canonical task name will be provided to it along with the message.
When the user asks you to spawn subagents to research, compare, inspect, or report back on something, spawn the subagents, then call `wait_agent` and synthesize their returned mailbox results. Do not end with only a list of spawned agents or "I'll wait when you want" unless the user explicitly asked for background-only work.
If multiple spawned agents are expected to contribute to the answer, keep waiting and integrating mailbox updates until the requested comparison/summary is complete or every spawned agent has finished/failed.
{concurrency_guidance}"#,
            available_models_description
        );
        if options.include_usage_hint {
            if let Some(usage_hint) = options
                .usage_hint_text
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                description.push('\n');
                description.push_str(usage_hint);
            }
        }
        if options.is_spawned_subagent {
            description.push_str("\nYou are already a subagent. Do not use spawn_agent merely because inherited parent history asked for agents. Only use spawn_agent if your direct current subagent task explicitly asks you to delegate further.");
        }
        description
    }

    fn spawn_agent_tool_description_v1(options: &SpawnAgentDefinitionOptions) -> String {
        let available_models_description = if options.hide_agent_type_model_reasoning {
            ""
        } else {
            options
                .available_models_description
                .as_deref()
                .unwrap_or("")
        };
        let concurrency_guidance =
            spawned_subagent_concurrency_guidance(options.max_concurrent_threads_per_session);
        let mut description = format!(
            r#"
        {}
This spawn_agent tool provides you access to sub-agents that inherit your current model by default. Do not set the `model` field unless the user explicitly asks for a different model or there is a clear task-specific reason. You should follow the rules and guidelines below to use this tool.
{concurrency_guidance}

Only use `spawn_agent` if and only if the user explicitly asks for sub-agents, delegation, or parallel agent work.
Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn.
Agent-role guidance below only helps choose which agent to use after spawning is already authorized; it never authorizes spawning by itself.

### When to delegate vs. do the subtask yourself
- First, quickly analyze the overall user task and form a succinct high-level plan. Identify which tasks are immediate blockers on the critical path, and which tasks are sidecar tasks that are needed but can run in parallel without blocking the next local step. As part of that plan, explicitly decide what immediate task you should do locally right now. Do this planning step before delegating to agents so you do not hand off the immediate blocking task to a submodel and then waste time waiting on it.
- Use a subagent when a subtask is easy enough for it to handle and can run in parallel with your local work. Prefer delegating concrete, bounded sidecar tasks that materially advance the main task without blocking your immediate next local step.
- Do not delegate urgent blocking work when your immediate next step depends on that result. If the very next action is blocked on that task, the main rollout should usually do it locally to keep the critical path moving.
- Keep work local when the subtask is too difficult to delegate well and when it is tightly coupled, urgent, or likely to block your immediate next step.

### Designing delegated subtasks
- Subtasks must be concrete, well-defined, and self-contained.
- Delegated subtasks must materially advance the main task.
- Do not duplicate work between the main rollout and delegated subtasks.
- Avoid issuing multiple delegate calls on the same unresolved thread unless the new delegated task is genuinely different and necessary.
- Narrow the delegated ask to the concrete output you need next.
- For coding tasks, prefer delegating concrete code-change worker subtasks over read-only explorer analysis when the subagent can make a bounded patch in a clear write scope.
- When delegating coding work, instruct the submodel to edit files directly in its forked workspace and list the file paths it changed in the final answer.
- For code-edit subtasks, decompose work so each delegated task has a disjoint write set.

### After you delegate
- Call wait_agent very sparingly. Only call wait_agent when you need the result immediately for the next critical-path step and you are blocked until it returns.
- Do not redo delegated subagent tasks yourself; focus on integrating results or tackling non-overlapping work.
- While the subagent is running in the background, do meaningful non-overlapping work immediately.
- Do not repeatedly wait by reflex.
- When a delegated coding task returns, quickly review the uploaded changes, then integrate or refine them.

### Parallel delegation patterns
- Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.
- Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.
- Delegate verification only when it can run in parallel with ongoing implementation and is likely to catch a concrete risk before final integration.
- The key is to find opportunities to spawn multiple independent subtasks in parallel within the same round, while ensuring each subtask is well-defined, self-contained, and materially advances the main task."#,
            available_models_description
        );
        if options.include_usage_hint {
            if let Some(usage_hint) = options
                .usage_hint_text
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                description.push('\n');
                description.push_str(usage_hint);
            }
        }
        description
    }

    fn namespace_v1(mut definition: ToolDefinition) -> ToolDefinition {
        definition.namespace = Some(MULTI_AGENT_V1_NAMESPACE.to_string());
        definition.namespace_description = Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string());
        definition
    }

    fn wait_output_schema_v2() -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Brief wait summary without the agent's final content."
                },
                "timed_out": {
                    "type": "boolean",
                    "description": "Whether the wait call returned because no mailbox update arrived before the timeout."
                }
            },
            "required": ["message", "timed_out"],
            "additionalProperties": false
        })
    }

    fn send_input_output_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "submission_id": {
                    "type": "string",
                    "description": "Identifier for the queued input submission."
                }
            },
            "required": ["submission_id"],
            "additionalProperties": false
        })
    }

    fn resume_agent_output_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": agent_status_output_schema()
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    fn wait_output_schema_v1() -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "object",
                    "description": "Final statuses keyed by agent id.",
                    "additionalProperties": agent_status_output_schema()
                },
                "timed_out": {
                    "type": "boolean",
                    "description": "Whether the wait call returned due to timeout before any agent reached a final status."
                }
            },
            "required": ["status", "timed_out"],
            "additionalProperties": false
        })
    }

    fn collab_input_items_schema() -> Value {
        json!({
            "type": "array",
            "description": "Structured input items. Use this to pass explicit mentions (for example app:// connector paths).",
            "items": {
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "description": "Input item type: text, image, local_image, skill, or mention."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text content when type is text."
                    },
                    "image_url": {
                        "type": "string",
                        "description": "Image URL when type is image."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path when type is local_image/skill, or structured mention target such as app://<connector-id> or plugin://<plugin-name>@<marketplace-name> when type is mention."
                    },
                    "name": {
                        "type": "string",
                        "description": "Display name when type is skill or mention."
                    },
                    "detail": {
                        "type": "string",
                        "description": "Optional image detail hint when type is image or local_image.",
                        "enum": ["high", "original"]
                    },
                    "text_elements": {
                        "type": "array",
                        "description": "UI-defined spans within text that should be treated as special elements.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "byte_range": {
                                    "type": "object",
                                    "properties": {
                                        "start": {
                                            "type": "integer",
                                            "minimum": 0
                                        },
                                        "end": {
                                            "type": "integer",
                                            "minimum": 0
                                        }
                                    },
                                    "required": ["start", "end"],
                                    "additionalProperties": false
                                },
                                "placeholder": {
                                    "type": "string"
                                }
                            },
                            "required": ["byte_range"],
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }
        })
    }

    fn list_agents_output_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent_name": {
                                "type": "string",
                                "description": "Canonical task name for the agent when available, otherwise the agent id."
                            },
                            "agent_status": {
                                "description": "Last known status of the agent.",
                                "allOf": [agent_status_output_schema()]
                            },
                            "last_task_message": {
                                "type": ["string", "null"],
                                "description": "Most recent user or inter-agent instruction received by the agent, when available."
                            }
                        },
                        "required": ["agent_name", "agent_status", "last_task_message"],
                        "additionalProperties": false
                    },
                    "description": "Live agents visible in the current root thread tree."
                }
            },
            "required": ["agents"],
            "additionalProperties": false
        })
    }

    fn close_agent_output_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "previous_status": {
                    "description": "The agent status observed before shutdown was requested.",
                    "allOf": [agent_status_output_schema()]
                }
            },
            "required": ["previous_status"],
            "additionalProperties": false
        })
    }

    /// `spawn_agent`: delegate a task to a child sub-agent. Parity: codex
    /// `create_spawn_agent_tool_v2` (`multi_agents_spec.rs:75-109`) — required
    /// `["task_name", "message"]`; matches
    /// [`SpawnAgentArgs`](crate::subagents::SpawnAgentArgs) (the request type the
    /// handler deserializes), which is `deny_unknown_fields`.
    pub fn spawn_agent() -> ToolDefinition {
        spawn_agent_with_options(SpawnAgentDefinitionOptions::default())
    }

    pub fn spawn_agent_with_options(options: SpawnAgentDefinitionOptions) -> ToolDefinition {
        let mut properties = Map::new();
        properties.insert(
            "message".to_string(),
            json!({
                "type": "string",
                "description": "Initial plain-text task for the new agent."
            }),
        );
        properties.insert(
            "task_name".to_string(),
            json!({
                "type": "string",
                "description": "Task name for the new agent. Use lowercase letters, digits, and underscores."
            }),
        );
        properties.insert(
            "fork_turns".to_string(),
            json!({
                "type": "string",
                "description": "Optional number of turns to fork. Defaults to `all`. Use `none`, `all`, or a positive integer string such as `3` to fork only the most recent turns."
            }),
        );
        if !options.hide_agent_type_model_reasoning {
            properties.insert(
                "agent_type".to_string(),
                json!({
                    "type": "string",
                    "description": options.agent_type_description
                }),
            );
            properties.insert(
                "model".to_string(),
                json!({
                    "type": "string",
                    "description": SPAWN_AGENT_MODEL_OVERRIDE_DESCRIPTION
                }),
            );
            properties.insert(
                "reasoning_effort".to_string(),
                json!({
                    "type": "string",
                    "description": "Optional reasoning effort override for the new agent. Replaces the inherited reasoning effort."
                }),
            );
        }
        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: spawn_agent_description_v2(&options),
            input_schema: json!({
                "type": "object",
                "properties": Value::Object(properties),
                "required": ["task_name", "message"],
                "additionalProperties": false
            }),
            output_schema: Some(spawn_agent_output_schema_v2(
                options.hide_agent_type_model_reasoning,
            )),
            namespace: None,
            namespace_description: None,
        }
    }

    pub fn spawn_agent_v1_with_options(options: SpawnAgentDefinitionOptions) -> ToolDefinition {
        let mut properties = Map::new();
        properties.insert(
            "message".to_string(),
            json!({
                "type": "string",
                "description": "Initial plain-text task for the new agent. Use either message or items."
            }),
        );
        properties.insert("items".to_string(), collab_input_items_schema());
        if !options.hide_agent_type_model_reasoning {
            properties.insert(
                "agent_type".to_string(),
                json!({
                    "type": "string",
                    "description": options.agent_type_description
                }),
            );
            properties.insert(
                "fork_context".to_string(),
                json!({
                    "type": "boolean",
                    "description": "When true, fork the current thread history into the new agent before sending the initial prompt. This must be used when you want the new agent to have exactly the same context as you."
                }),
            );
            properties.insert(
                "model".to_string(),
                json!({
                    "type": "string",
                    "description": SPAWN_AGENT_MODEL_OVERRIDE_DESCRIPTION
                }),
            );
            properties.insert(
                "reasoning_effort".to_string(),
                json!({
                    "type": "string",
                    "description": "Optional reasoning effort override for the new agent. Replaces the inherited reasoning effort."
                }),
            );
            properties.insert(
                "service_tier".to_string(),
                json!({
                    "type": "string",
                    "description": SPAWN_AGENT_SERVICE_TIER_OVERRIDE_DESCRIPTION
                }),
            );
        }
        namespace_v1(ToolDefinition {
            name: "spawn_agent".to_string(),
            description: spawn_agent_tool_description_v1(&options),
            input_schema: json!({
                "type": "object",
                "properties": Value::Object(properties),
                "additionalProperties": false
            }),
            output_schema: Some(spawn_agent_output_schema_v1()),
            namespace: None,
            namespace_description: None,
        })
    }

    /// `wait_agent`: EVENT-NOTIFY wait for a child to report news. Parity: codex
    /// `multi_agents_v2/wait.rs`: targetless mailbox wait.
    pub fn wait_agent() -> ToolDefinition {
        wait_agent_with_timeouts(WaitAgentDefinitionOptions {
            default_timeout_ms: 300_000,
            min_timeout_ms: 1,
            max_timeout_ms: 3_600_000,
        })
    }

    pub fn wait_agent_with_timeouts(options: WaitAgentDefinitionOptions) -> ToolDefinition {
        let max_timeout_ms = options.max_timeout_ms.max(1);
        let min_timeout_ms = options.min_timeout_ms.clamp(1, max_timeout_ms);
        ToolDefinition {
            name: "wait_agent".to_string(),
            description: "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. Does not return the content; returns either a summary of which agents have updates (if any), or a timeout summary if no mailbox update arrives before the deadline. Use this after spawning agents when the user expects their findings, comparison, or final synthesis; after it completes, consume the mailbox update in the next model step and continue waiting if more spawned agents still need to report."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "timeout_ms": {
                        "type": "number",
                        "description": format!(
                            "Optional timeout in milliseconds. Defaults to {}, min {}, max {}.",
                            options.default_timeout_ms,
                            min_timeout_ms,
                            max_timeout_ms
                        )
                    }
                },
                "additionalProperties": false
            }),
            output_schema: Some(wait_output_schema_v2()),
            namespace: None,
            namespace_description: None,
        }
    }

    pub fn wait_agent_v1_with_timeouts(options: WaitAgentDefinitionOptions) -> ToolDefinition {
        namespace_v1(ToolDefinition {
            name: "wait_agent".to_string(),
            description: "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "targets": {
                        "type": "array",
                        "description": "Agent ids to wait on. Pass multiple ids to wait for whichever finishes first.",
                        "items": { "type": "string" }
                    },
                    "timeout_ms": {
                        "type": "number",
                        "description": format!(
                            "Optional timeout in milliseconds. Defaults to {}, min {}, max {}. Prefer longer waits (minutes) to avoid busy polling.",
                            options.default_timeout_ms,
                            options.min_timeout_ms,
                            options.max_timeout_ms
                        )
                    }
                },
                "required": ["targets"],
                "additionalProperties": false
            }),
            output_schema: Some(wait_output_schema_v1()),
            namespace: None,
            namespace_description: None,
        })
    }

    /// `send_input`: deliver a message to a running child agent (codex
    /// `enqueue_mailbox_communication`).
    pub fn send_input() -> ToolDefinition {
        namespace_v1(ToolDefinition {
            name: "send_input".to_string(),
            description: "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Agent id to message (from spawn_agent)."
                    },
                    "message": {
                        "type": "string",
                        "description": "Legacy plain-text message to send to the agent. Use either message or items."
                    },
                    "items": collab_input_items_schema(),
                    "interrupt": {
                        "type": "boolean",
                        "description": "When true, stop the agent's current task and handle this immediately. When false (default), queue this message."
                    },
                },
                "required": ["target"],
                "additionalProperties": false
            }),
            output_schema: Some(send_input_output_schema()),
            namespace: None,
            namespace_description: None,
        })
    }

    pub fn resume_agent() -> ToolDefinition {
        namespace_v1(ToolDefinition {
            name: "resume_agent".to_string(),
            description:
                "Resume a previously closed agent by id so it can receive send_input and wait_agent calls."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Agent id to resume."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            output_schema: Some(resume_agent_output_schema()),
            namespace: None,
            namespace_description: None,
        })
    }

    /// `send_message`: queue a message on a running child without triggering a
    /// fresh turn (codex MultiAgentV2).
    pub fn send_message() -> ToolDefinition {
        ToolDefinition {
            name: "send_message".to_string(),
            description: "Send a message to an existing agent. The message will be delivered promptly. Does not trigger a new turn."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Relative or canonical task name to message (from spawn_agent)."
                    },
                    "message": {
                        "type": "string",
                        "description": "Message text to queue on the target agent."
                    }
                },
                "required": ["target", "message"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `followup_task`: send a message and trigger the target's next turn (codex
    /// MultiAgentV2).
    pub fn followup_task() -> ToolDefinition {
        ToolDefinition {
            name: "followup_task".to_string(),
            description: "Send a message to an existing non-root target agent and trigger a turn in that target. If the target is currently mid-turn, the message is queued and will be used to start the target's next turn, after the current turn completes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Agent id or canonical task name to message (from spawn_agent)."
                    },
                    "message": {
                        "type": "string",
                        "description": "Message text to send to the target agent."
                    }
                },
                "required": ["target", "message"],
                "additionalProperties": false
            }),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// `list_agents`: a read-only snapshot of the live sub-agent registry (codex
    /// `live_agents`).
    pub fn list_agents() -> ToolDefinition {
        ToolDefinition {
            name: "list_agents".to_string(),
            description:
                "List live agents in the current root thread tree. Optionally filter by task-path prefix."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional task-path prefix (not ending with trailing slash). Accepts the same relative or absolute task-path syntax."
                    }
                },
                "additionalProperties": false
            }),
            output_schema: Some(list_agents_output_schema()),
            namespace: None,
            namespace_description: None,
        }
    }

    /// `close_agent`: close a spawned child agent and descendants.
    pub fn close_agent() -> ToolDefinition {
        ToolDefinition {
            name: "close_agent".to_string(),
            description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Agent id or canonical task name to close (from spawn_agent)."
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
            output_schema: Some(close_agent_output_schema()),
            namespace: None,
            namespace_description: None,
        }
    }

    pub fn close_agent_v1() -> ToolDefinition {
        namespace_v1(ToolDefinition {
            name: "close_agent".to_string(),
            description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Agent id to close (from spawn_agent)."
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
            output_schema: Some(close_agent_output_schema()),
            namespace: None,
            namespace_description: None,
        })
    }
}

/// Build a [`ToolRegistry`] preloaded with the default callable handlers, each carrying its
/// parity-grounded [`ToolDefinition`] and static `parallel_safe` flag.
///
/// This is the single place the dispatch loop wires the full tool set. Most
/// tools register directly; `browser` and `mcp` register via
/// [`register_with_wire`](ToolRegistry::register_with_wire) over their
/// `WireArgs` types. The browser/python/mcp handlers need an injected backend
/// (they would otherwise reach the OS), so those are supplied by the caller.
///
/// `parallel_safe` per tool: `exec_command` / `tool_search` / `web_search` /
/// `search` = `true`; `shell` / `apply_patch` / `view_image` / `browser` /
/// `python` / `update_plan` / `done` = `false` (serial). `mcp` is registered
/// `false` here
/// (a serial default); its per-request read-only hint still drives the handler's
/// own [`ToolRuntime::parallel_safe`](crate::tools::ToolRuntime::parallel_safe).
#[allow(clippy::too_many_arguments)]
pub fn default_registry<S, A>(
    shell: crate::tools::handlers::shell::ShellTool,
    apply_patch: crate::tools::handlers::apply_patch::ApplyPatchTool,
    view_image: crate::tools::handlers::view_image::ViewImageTool,
    browser: crate::tools::handlers::browser::BrowserTool,
    python: crate::tools::handlers::python::PythonTool,
    mcp: crate::tools::handlers::mcp::McpTool,
    update_plan: crate::tools::handlers::update_plan::UpdatePlanTool,
    tool_search: crate::tools::handlers::tool_search::ToolSearchTool,
    web_search: crate::tools::handlers::web_search::WebSearchTool,
    search: crate::tools::handlers::search::SearchTool,
    done: crate::tools::handlers::done::DoneTool,
) -> ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    use crate::tools::handlers::apply_patch::ApplyPatchRequest;
    use crate::tools::handlers::browser::BrowserRequest;
    use crate::tools::handlers::done::DoneRequest;
    use crate::tools::handlers::mcp::McpToolCallRequest;
    use crate::tools::handlers::python::PythonRequest;
    use crate::tools::handlers::search::SearchRequest;
    use crate::tools::handlers::shell::{
        ExecCommandRequest, ExecCommandTool, ShellRequest, WriteStdinRequest, WriteStdinTool,
    };
    use crate::tools::handlers::tool_search::ToolSearchRequest;
    use crate::tools::handlers::update_plan::UpdatePlanRequest;
    use crate::tools::handlers::view_image::ViewImageRequest;
    use crate::tools::handlers::web_search::WebSearchRequest;

    let mut reg = ToolRegistry::new();

    reg.register::<_, ShellRequest>("shell", definitions::shell(), false, shell);
    let unified_exec = crate::tools::unified_exec::UnifiedExecManager::default();
    reg.register::<_, ExecCommandRequest>(
        "exec_command",
        definitions::exec_command(),
        true,
        ExecCommandTool::new(unified_exec.clone()),
    );
    reg.register::<_, WriteStdinRequest>(
        "write_stdin",
        definitions::write_stdin(),
        false,
        WriteStdinTool::new(unified_exec),
    );
    reg.register::<_, ApplyPatchRequest>(
        "apply_patch",
        definitions::apply_patch(),
        false,
        apply_patch,
    );
    reg.register::<_, ViewImageRequest>("view_image", definitions::view_image(), false, view_image);
    reg.register::<_, PythonRequest>("python", definitions::python(), false, python);
    // `browser` / `mcp` carry a parsed / namespaced `Req`; each deserializes
    // THROUGH its `WireArgs` via `#[serde(from = "…WireArgs")]`, so the plain
    // `register` path works (the registry deserializes the model object straight
    // into the `Req`).
    reg.register::<_, BrowserRequest>("browser", definitions::browser(), false, browser);
    reg.register::<_, McpToolCallRequest>("mcp", definitions::mcp(), false, mcp);
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        definitions::update_plan(),
        false,
        update_plan,
    );
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        definitions::tool_search(),
        true,
        tool_search,
    );
    reg.register::<_, WebSearchRequest>("web_search", definitions::web_search(), true, web_search);
    // `search`: locally-executed DuckDuckGo search. Read-only HTTP GET +
    // pure parse, so parallel-safe like `web_search` / `tool_search`.
    reg.register::<_, SearchRequest>("search", definitions::search(), true, search);
    // `done`: the completion tool. Serial (terminal; must not be reordered).
    reg.register::<_, DoneRequest>("done", definitions::done(), false, done);

    reg
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
