use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_agent::config_model::{
    configured_model_provider_id_for_cwd_with_options, default_model_for_cwd_with_options,
    model_catalog_for_cwd_with_options,
};
use browser_use_agent::config_overrides::{
    apply_child_request_runtime_config, apply_runtime_config_overrides,
    load_mcp_servers_for_profile, parse_config_overrides, resolve_agent_roles_for_profile,
    resolve_approval_policy_for_profile, resolve_collab_for_profile, resolve_guardian_for_profile,
    resolve_multi_agent_v2_for_profile, AgentRunOptions, ChildAgentRunCompletion,
    ChildAgentRunRequest, ChildAgentRunner, ConfigOverrides, ProviderBackend, ProviderRunConfig,
    RunConfigValueSource,
};
use browser_use_agent::context::{
    append_user_shell_command_context_event, typed_user_input_payload_from_items_for_cwd,
    typed_user_input_payload_from_text_for_cwd,
};
use browser_use_agent::entrypoint::cleanup_unified_exec_manager_for_session_id;
use browser_use_agent::entrypoint::RuntimeTurnDriver;
use browser_use_agent::infra::{
    capture_async, capture_blocking, install_process_crypto_provider,
    record_browser_script_response_events, record_python_response_final_event,
    record_python_worker_event, review_prompt_base_branch, review_prompt_commit,
    review_prompt_custom, review_prompt_uncommitted_changes, start_review_session,
    UnifiedExecShutdownCleanup,
};
use browser_use_agent::live_executor::{
    ensure_agent_attached as ensure_runtime_agent_attached, RuntimeAgentExecutor,
    RuntimeAgentExecutorConfig, RuntimeAgentRunRequest,
};
use browser_use_agent::prompts::CollaborationModeKind;
use browser_use_agent::rollout::fork_events_by_turn;
use browser_use_agent::session::SharedStore;
use browser_use_agent::session::{
    provider_messages_from_events_for_fork, resume::provider_messages_to_fork_response_items,
    ForkMode,
};
use browser_use_agent::subagents::{
    canonical_agent_path_from_task_name, canonical_agent_reference,
    cleanup_agent_runtime_state_for_agent_subtree, display_agent_path_for_session,
    last_task_message_for_agent, local_agent_status_value, session_was_interrupted,
    store_collect_agent_tree as collect_agent_tree,
    store_resolve_agent_reference_in_tree as resolve_agent_reference_in_tree,
    store_root_session_id as root_session_id, ResolvedAgentReference,
};
use browser_use_agent::tools::AskForApproval;
use browser_use_protocol::{
    browser_summary_from_events, failure_from_events, sanitized_agent_context_from_events,
    session_result_from_events, task_from_events,
};
use browser_use_providers::{
    claude_code_oauth_authorize_url, claude_code_oauth_pkce,
    exchange_claude_code_authorization_code, load_codex_auth, load_codex_auth_file,
    load_codex_managed_auth, load_codex_managed_auth_file, parse_claude_code_authorization_input,
    ClaudeCodeOAuthCredential, CodexAuth, CodexManagedAuth, CLAUDE_CODE_CALLBACK_HOST,
    CLAUDE_CODE_CALLBACK_PATH, CLAUDE_CODE_CALLBACK_PORT,
};
use browser_use_python_worker::PythonWorker;
use browser_use_runtime::{
    send_local_runtime_request, AgentId, BrowserConfig, BrowserId, BrowserUseRuntime,
    CompleteAgentRequest, CreateRootAgentRequest, Durability as RuntimeDurability,
    FailAgentRequest, LiveThreadPersistence, LocalRuntimeRequest, LocalRuntimeWaitTarget,
    MailboxDeliveryPhase as RuntimeMailboxDeliveryPhase, MailboxItemKind as RuntimeMailboxItemKind,
    MemoryJournal, RunAgentRequest, RunId as RuntimeRunId, RuntimeHandle, RuntimeProjectionState,
    SessionId, SpawnChildRequest, SqliteJournal, StateIndex, SubmitInputRequest,
};
#[cfg(test)]
use browser_use_runtime::{AttachChildAgentRequest, AttachRootAgentRequest};
use browser_use_store::{now_ms, resolve_state_dir, Store};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MESSAGE_KIND_FOLLOWUP: &str = "followup";
const APPROX_CHARS_PER_TOKEN: usize = 4;
const DATASET_BROWSER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Parser)]
#[command(name = "browser-use-terminal", bin_name = "browser-use-terminal")]
#[command(about = "Rust browser-use task control")]
#[command(version)]
struct Args {
    #[arg(long, default_value = "~/.browser-use-terminal")]
    state_dir: PathBuf,
    /// Layer $BROWSER_USE_TERMINAL_HOME/<name>.config.toml on top of the base user config.
    #[arg(long = "profile", short = 'p', global = true)]
    config_profile: Option<String>,
    /// Override a configuration value. Use a dotted path and TOML value.
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = clap::ArgAction::Append,
        global = true
    )]
    config_overrides: Vec<String>,
    #[arg(long = "collaboration-mode", value_enum, default_value_t = CollaborationModeArg::Default, global = true)]
    collaboration_mode: CollaborationModeArg,
    #[arg(long = "approval-policy", value_enum, global = true)]
    approval_policy: Option<ApprovalPolicyArg>,
    #[arg(long = "guardian", global = true)]
    guardian: bool,
    /// Load additional MCP server definitions from a TOML config file.
    #[arg(long = "mcp-config", value_name = "PATH", action = clap::ArgAction::Append, global = true)]
    mcp_config: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CollaborationModeArg {
    Default,
}

impl From<CollaborationModeArg> for CollaborationModeKind {
    fn from(value: CollaborationModeArg) -> Self {
        match value {
            CollaborationModeArg::Default => CollaborationModeKind::Default,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ApprovalPolicyArg {
    Never,
    OnFailure,
    OnRequest,
    UnlessTrusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SdkTransportArg {
    Stdio,
}

impl From<ApprovalPolicyArg> for AskForApproval {
    fn from(value: ApprovalPolicyArg) -> Self {
        match value {
            ApprovalPolicyArg::Never => AskForApproval::Never,
            ApprovalPolicyArg::OnFailure => AskForApproval::OnFailure,
            ApprovalPolicyArg::OnRequest => AskForApproval::OnRequest,
            ApprovalPolicyArg::UnlessTrusted => AskForApproval::UnlessTrusted,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CliRuntimeOptions {
    approval_policy: Option<AskForApproval>,
    use_guardian: Option<bool>,
    mcp_config_paths: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start {
        text: String,
    },
    RunFake {
        text: String,
        #[arg(long)]
        python_code: Option<String>,
    },
    RunOpenai {
        text: String,
        #[arg(long)]
        model: Option<String>,
    },
    RunAnthropic {
        text: String,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
    },
    RunOpenrouter {
        text: String,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
    },
    RunDeepseek {
        text: String,
        #[arg(long, default_value = "deepseek-v4-pro")]
        model: String,
    },
    /// Run a task against the codex (chatgpt.com) backend via the Codex CLI login.
    ///
    /// Credentials resolve env-first (`CODEX_ACCESS_TOKEN` + `CODEX_ACCOUNT_ID`),
    /// then the credential store (`auth login codex` / `auth import-codex`), then
    /// `~/.codex/auth.json`.
    RunCodex {
        text: String,
        #[arg(long, default_value = "gpt-5.1-codex")]
        model: String,
    },
    RunCodexSession {
        task_id: String,
        #[arg(long, default_value = "gpt-5.1-codex")]
        model: String,
    },
    RunOpenaiSession {
        task_id: String,
        #[arg(long)]
        model: Option<String>,
    },
    RunAnthropicSession {
        task_id: String,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
    },
    RunOpenrouterSession {
        task_id: String,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
    },
    RunDeepseekSession {
        task_id: String,
        #[arg(long, default_value = "deepseek-v4-pro")]
        model: String,
    },
    Followup {
        task_id: String,
        text: String,
    },
    Finish {
        task_id: String,
        #[arg(long)]
        result: String,
    },
    Fail {
        task_id: String,
        #[arg(long)]
        error: String,
    },
    Cancel {
        task_id: String,
        #[arg(long, default_value = "user requested cancellation")]
        reason: String,
    },
    #[command(alias = "session")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    History,
    Show {
        task_id: String,
    },
    Events {
        task_id: String,
    },
    Python {
        task_id: String,
        code: String,
    },
    #[command(alias = "browser_script")]
    BrowserScript {
        task_id: String,
        code: String,
    },
    SyncCookies {
        #[arg(value_name = "LOCAL_PROFILE")]
        profile: Option<String>,
        #[arg(long = "local-profile")]
        local_profile: Option<String>,
        #[arg(long)]
        all_cookies: bool,
        #[arg(long = "domain", action = clap::ArgAction::Append)]
        domains: Vec<String>,
        #[arg(long = "exclude-domain", action = clap::ArgAction::Append)]
        exclude_domains: Vec<String>,
        #[arg(long)]
        cloud_profile_id: Option<String>,
        #[arg(long)]
        cloud_profile_name: Option<String>,
        #[arg(long)]
        new_cloud_profile_name: Option<String>,
    },
    UserShell {
        task_id: String,
        command: String,
    },
    Review {
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        commit: Option<String>,
        #[arg(long)]
        custom: Option<String>,
    },
    Export {
        task_id: String,
        output_dir: PathBuf,
    },
    Import {
        input: PathBuf,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Diagnostics,
    SdkServer {
        #[arg(long, value_enum, default_value_t = SdkTransportArg::Stdio)]
        transport: SdkTransportArg,
    },
    Trace {
        task_id: String,
        output: Option<PathBuf>,
    },
    SpawnAgent {
        parent_id: String,
        message: String,
        #[arg(long)]
        task_name: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        nickname: Option<String>,
        #[arg(long)]
        role: Option<String>,
    },
    ListAgents {
        parent_id: String,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        json: bool,
    },
    CloseAgent {
        target: String,
        #[arg(long)]
        current_id: Option<String>,
        #[arg(long, default_value = "closed by user")]
        reason: String,
    },
    ResumeAgent {
        child_id: String,
    },
    SendAgentMessage {
        author_id: String,
        target_id: String,
        message: String,
        #[arg(long)]
        trigger_turn: bool,
    },
    WaitAgent {
        target_id: String,
        #[arg(long = "target")]
        targets: Vec<String>,
        #[arg(long, default_value_t = 30000)]
        timeout_ms: u64,
    },
    Update {
        #[arg(long, default_value = "latest")]
        release: String,
        #[arg(long)]
        check: bool,
        #[arg(long)]
        install_script: Option<String>,
    },
    DatasetList,
    DatasetSample {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    DatasetReport {
        run_id_or_path: String,
    },
    DatasetRunFake {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 1)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunOpenai {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunCodex {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "gpt-5.1-codex")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunAnthropic {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunOpenrouter {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Init,
    Show,
    Set { key: String, value: String },
    Reset { target: ConfigResetTarget },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ConfigResetTarget {
    Onboarding,
    Profile,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Status,
    Login {
        account: AuthAccount,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        account_id: Option<String>,
        #[arg(long)]
        code: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    ImportCodex {
        #[arg(long = "from")]
        input: Option<PathBuf>,
    },
    Logout {
        account: AuthAccount,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    List,
    Show {
        task_id: String,
    },
    Cancel {
        task_id: String,
        #[arg(long, default_value = "user requested cancellation")]
        reason: String,
    },
    Trace {
        task_id: String,
        output: Option<PathBuf>,
    },
    Export {
        task_id: String,
        output_dir: PathBuf,
    },
    Import {
        input: PathBuf,
    },
    Events {
        task_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AuthAccount {
    Codex,
    ClaudeCode,
    BrowserUseCloud,
    Openai,
    Anthropic,
    Openrouter,
    Deepseek,
}

#[derive(Clone, Debug, Serialize)]
struct DatasetCase {
    dataset: String,
    path: String,
    task_id: String,
    confirmed_task: String,
    raw: Value,
}

#[derive(Clone, Debug)]
struct DatasetRunOptions {
    count: usize,
    task_ids: Vec<String>,
    all: bool,
    run_id: Option<String>,
    resume: bool,
    skip_failed: bool,
    stop_on_failure: bool,
    max_attempts: usize,
    concurrency: usize,
    browser_mode: Option<String>,
}

#[derive(Clone, Debug)]
struct DatasetProviderConfig {
    provider: String,
    model: String,
    browser_mode: String,
    max_turns: usize,
    python_timeout_seconds: u64,
}

trait DatasetRunner: Clone + Send + Sync + 'static {
    fn run_dataset_session(
        &self,
        store: &Store,
        session_id: &str,
        options: AgentRunOptions,
    ) -> Result<()>;
}

#[derive(Clone)]
struct ConfigDatasetRunner {
    config: ProviderRunConfig,
}

impl DatasetRunner for ConfigDatasetRunner {
    fn run_dataset_session(
        &self,
        store: &Store,
        session_id: &str,
        options: AgentRunOptions,
    ) -> Result<()> {
        let mut config = self.config.clone();
        let mut merged_options = options;
        merged_options.config_profile = config.options.config_profile.clone();
        merged_options.config_overrides = config.options.config_overrides.clone();
        merged_options.model_provider_id = config.options.model_provider_id.clone();
        merged_options.model_provider_id_source = config.options.model_provider_id_source;
        merged_options.collaboration_mode = config.options.collaboration_mode;
        merged_options.child_agent_runner = config.options.child_agent_runner.clone();
        merged_options.mcp_servers = config.options.mcp_servers.clone();
        merged_options.approval_policy = config.options.approval_policy;
        merged_options.use_guardian = config.options.use_guardian;
        config.options = merged_options;
        run_existing_session_from_config_and_notify(store, session_id, config, None)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
struct DatasetTaskPaths {
    root: PathBuf,
    cwd: PathBuf,
    artifacts: PathBuf,
    agent_workspace: PathBuf,
    logs: PathBuf,
    runtime: PathBuf,
    tmp: PathBuf,
}

fn main() -> Result<()> {
    install_process_crypto_provider();
    let _unified_exec_cleanup = UnifiedExecShutdownCleanup::new();
    load_dotenv()?;
    let mut args = Args::parse();
    if let Command::SdkServer { transport } = args.command {
        return sdk_server(transport);
    }
    args.state_dir = resolve_state_dir(&args.state_dir);
    let store = Store::open(&args.state_dir)?;
    capture_async(
        &store,
        "bu:cli command ran",
        serde_json::json!({ "command": command_name(&args.command), "surface": "cli" }),
    );
    let config_profile = args.config_profile.clone();
    let config_overrides = args.config_overrides.clone();
    let collaboration_mode = args.collaboration_mode.into();
    let runtime_options = CliRuntimeOptions {
        approval_policy: args.approval_policy.map(Into::into),
        use_guardian: args.guardian.then_some(true),
        mcp_config_paths: args.mcp_config.clone(),
    };
    match args.command {
        Command::Start { text } => start(&store, text),
        Command::RunFake { text, python_code } => run_fake(&store, text, python_code),
        Command::RunOpenai { text, model } => run_openai(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunAnthropic { text, model } => run_anthropic(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunOpenrouter { text, model } => run_openrouter(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunDeepseek { text, model } => run_deepseek(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunCodex { text, model } => run_codex(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunCodexSession { task_id, model } => run_codex_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunOpenaiSession { task_id, model } => run_openai_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunAnthropicSession { task_id, model } => run_anthropic_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunOpenrouterSession { task_id, model } => run_openrouter_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::RunDeepseekSession { task_id, model } => run_deepseek_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            &runtime_options,
        ),
        Command::Followup { task_id, text } => followup(&store, &task_id, text),
        Command::Finish { task_id, result } => finish(&store, &task_id, result),
        Command::Fail { task_id, error } => fail(&store, &task_id, error),
        Command::Cancel { task_id, reason } => cancel(&store, &task_id, &reason),
        Command::Sessions { command } => sessions(&store, command),
        Command::History => history(&store),
        Command::Show { task_id } => show(&store, &task_id),
        Command::Events { task_id } => events(&store, &task_id),
        Command::Python { task_id, code } => python(&store, &task_id, code),
        Command::BrowserScript { task_id, code } => browser_script(&store, &task_id, code),
        Command::SyncCookies {
            profile,
            local_profile,
            all_cookies,
            domains,
            exclude_domains,
            cloud_profile_id,
            cloud_profile_name,
            new_cloud_profile_name,
        } => sync_cookies(
            &store,
            SyncCookiesArgs {
                profile,
                local_profile,
                all_cookies,
                domains,
                exclude_domains,
                cloud_profile_id,
                cloud_profile_name,
                new_cloud_profile_name,
            },
        ),
        Command::UserShell { task_id, command } => user_shell(&store, &task_id, command),
        Command::Review {
            base,
            commit,
            custom,
        } => review(&store, base, commit, custom),
        Command::Export {
            task_id,
            output_dir,
        } => export(&store, &task_id, output_dir),
        Command::Import { input } => import(&store, input),
        Command::Config { command } => config(
            &store,
            command,
            config_profile.as_deref(),
            &config_overrides,
        ),
        Command::Auth { command } => auth(&store, command),
        Command::Diagnostics => diagnostics(&store),
        Command::SdkServer { .. } => unreachable!("sdk-server is handled before Store bootstrap"),
        Command::Trace { task_id, output } => trace(&store, &task_id, output),
        Command::SpawnAgent {
            parent_id,
            message,
            task_name,
            path,
            nickname,
            role,
        } => spawn_agent(&store, &parent_id, message, task_name, path, nickname, role),
        Command::ListAgents {
            parent_id,
            path_prefix,
            json,
        } => list_agents(&store, &parent_id, path_prefix.as_deref(), json),
        Command::CloseAgent {
            target,
            current_id,
            reason,
        } => close_agent(&store, current_id.as_deref(), &target, &reason),
        Command::ResumeAgent { child_id } => resume_agent(&store, &child_id),
        Command::SendAgentMessage {
            author_id,
            target_id,
            message,
            trigger_turn,
        } => send_agent_message(&store, &author_id, &target_id, &message, trigger_turn),
        Command::WaitAgent {
            target_id,
            targets,
            timeout_ms,
        } => wait_agent(&store, &target_id, targets, timeout_ms),
        Command::Update {
            release,
            check,
            install_script,
        } => update(&store, release, check, install_script),
        Command::DatasetList => dataset_list(),
        Command::DatasetSample {
            dataset,
            count,
            task_ids,
            all,
        } => dataset_sample(&dataset, count, task_ids, all),
        Command::DatasetReport { run_id_or_path } => dataset_report(&store, &run_id_or_path),
        Command::DatasetRunFake {
            dataset,
            count,
            task_ids,
            all,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_fake(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
        ),
        Command::DatasetRunOpenai {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_openai(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
            config_profile.as_deref(),
            &config_overrides,
            &runtime_options,
        ),
        Command::DatasetRunCodex {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_codex(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
            config_profile.as_deref(),
            &config_overrides,
            &runtime_options,
        ),
        Command::DatasetRunAnthropic {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_anthropic(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
            &runtime_options,
        ),
        Command::DatasetRunOpenrouter {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_openrouter(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
            &runtime_options,
        ),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Start { .. } => "start",
        Command::RunFake { .. } => "run_fake",
        Command::RunOpenai { .. } => "run_openai",
        Command::RunAnthropic { .. } => "run_anthropic",
        Command::RunOpenrouter { .. } => "run_openrouter",
        Command::RunDeepseek { .. } => "run_deepseek",
        Command::RunCodex { .. } => "run_codex",
        Command::RunCodexSession { .. } => "run_codex_session",
        Command::RunOpenaiSession { .. } => "run_openai_session",
        Command::RunAnthropicSession { .. } => "run_anthropic_session",
        Command::RunOpenrouterSession { .. } => "run_openrouter_session",
        Command::RunDeepseekSession { .. } => "run_deepseek_session",
        Command::Followup { .. } => "followup",
        Command::Finish { .. } => "finish",
        Command::Fail { .. } => "fail",
        Command::Cancel { .. } => "cancel",
        Command::Sessions { .. } => "sessions",
        Command::History => "history",
        Command::Show { .. } => "show",
        Command::Events { .. } => "events",
        Command::Python { .. } => "python",
        Command::BrowserScript { .. } => "browser_script",
        Command::SyncCookies { .. } => "sync_cookies",
        Command::UserShell { .. } => "user_shell",
        Command::Review { .. } => "review",
        Command::Export { .. } => "export",
        Command::Import { .. } => "import",
        Command::Config { .. } => "config",
        Command::Auth { .. } => "auth",
        Command::Diagnostics => "diagnostics",
        Command::SdkServer { .. } => "sdk_server",
        Command::Trace { .. } => "trace",
        Command::SpawnAgent { .. } => "spawn_agent",
        Command::ListAgents { .. } => "list_agents",
        Command::CloseAgent { .. } => "close_agent",
        Command::ResumeAgent { .. } => "resume_agent",
        Command::SendAgentMessage { .. } => "send_agent_message",
        Command::WaitAgent { .. } => "wait_agent",
        Command::Update { .. } => "update",
        Command::DatasetList => "dataset_list",
        Command::DatasetSample { .. } => "dataset_sample",
        Command::DatasetReport { .. } => "dataset_report",
        Command::DatasetRunFake { .. } => "dataset_run_fake",
        Command::DatasetRunOpenai { .. } => "dataset_run_openai",
        Command::DatasetRunCodex { .. } => "dataset_run_codex",
        Command::DatasetRunAnthropic { .. } => "dataset_run_anthropic",
        Command::DatasetRunOpenrouter { .. } => "dataset_run_openrouter",
    }
}

fn load_dotenv() -> Result<()> {
    let path = Path::new(".env");
    if !path.exists() {
        return Ok(());
    }
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = unquote_env_value(value.trim());
        unsafe {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

const DEFAULT_RELEASE_REPO: &str = "browser-use/terminal";
const INSTALL_SCRIPT_BRANCH: &str = "main";

fn update(
    store: &Store,
    release: String,
    check: bool,
    install_script: Option<String>,
) -> Result<()> {
    if check {
        let latest = if release == "latest" {
            latest_release_version()?
        } else {
            normalize_release_version(&release)
        };
        let current = env!("CARGO_PKG_VERSION");
        let update_status = if latest == current {
            "up_to_date"
        } else {
            "available"
        };
        capture_blocking(
            store,
            "bu:cli update checked",
            serde_json::json!({
                "surface": "cli",
                "status": update_status,
                "release": release.as_str(),
            }),
        );
        if latest == current {
            println!("browser-use terminal is up to date ({current}).");
        } else {
            println!("browser-use terminal update available: {current} -> {latest}");
            println!("Run `browser-use-terminal update` to install it.");
        }
        return Ok(());
    }

    let script = resolve_install_script(install_script)?;
    capture_blocking(
        store,
        "bu:cli update started",
        serde_json::json!({ "surface": "cli", "release": release.as_str() }),
    );
    let status = std::process::Command::new("sh")
        .arg(&script)
        .arg("--release")
        .arg(&release)
        .arg("--no-launch")
        .status()
        .with_context(|| format!("run installer script {}", script.display()))?;
    if !status.success() {
        capture_blocking(
            store,
            "bu:cli update failed",
            serde_json::json!({ "surface": "cli", "release": release.as_str() }),
        );
        bail!("installer exited with status {status}");
    }
    capture_blocking(
        store,
        "bu:cli update completed",
        serde_json::json!({ "surface": "cli", "release": release.as_str() }),
    );
    Ok(())
}

fn latest_release_version() -> Result<String> {
    let repo = release_repo();
    let url = format!("https://github.com/{repo}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build GitHub release client")?;
    let response = client
        .get(url)
        .header("User-Agent", "browser-use-terminal-updater")
        .send()
        .context("fetch latest GitHub release")?
        .error_for_status()
        .context("latest GitHub release returned an error")?;
    let final_url = response.url().clone();
    let segments = final_url
        .path_segments()
        .context("latest GitHub release URL has no path")?
        .collect::<Vec<_>>();
    let tag = segments
        .windows(3)
        .find_map(|window| {
            if window[0] == "releases" && window[1] == "tag" {
                Some(window[2])
            } else {
                None
            }
        })
        .context("latest GitHub release redirect missing tag")?;
    Ok(normalize_release_version(tag))
}

fn normalize_release_version(raw: &str) -> String {
    raw.trim()
        .strip_prefix("browser-use-terminal-v")
        .or_else(|| raw.trim().strip_prefix('v'))
        .unwrap_or(raw.trim())
        .to_string()
}

fn release_repo() -> String {
    std::env::var("BUT_RELEASE_REPO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string())
}

fn resolve_install_script(explicit: Option<String>) -> Result<PathBuf> {
    let source = explicit
        .or_else(|| std::env::var("BUT_INSTALL_SCRIPT").ok())
        .filter(|value| !value.trim().is_empty());
    match source {
        Some(source) if source.starts_with("https://") || source.starts_with("http://") => {
            download_install_script(&source)
        }
        Some(source) => Ok(PathBuf::from(source)),
        None => {
            let local_script = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .map(|root| root.join("scripts/install/install.sh"))
                .filter(|path| path.is_file());
            if let Some(path) = local_script {
                return Ok(path);
            }
            let repo = release_repo();
            let url = format!(
                "https://raw.githubusercontent.com/{repo}/{INSTALL_SCRIPT_BRANCH}/scripts/install/install.sh"
            );
            download_install_script(&url)
        }
    }
}

fn download_install_script(url: &str) -> Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build installer download client")?;
    let script = client
        .get(url)
        .header("User-Agent", "browser-use-terminal-updater")
        .send()
        .with_context(|| format!("download installer script from {url}"))?
        .error_for_status()
        .with_context(|| format!("installer script request failed for {url}"))?
        .text()
        .with_context(|| format!("read installer script from {url}"))?;
    let path = std::env::temp_dir().join(format!(
        "browser-use-terminal-update-{}-install.sh",
        std::process::id()
    ));
    fs::write(&path, script).context("write temporary installer script")?;
    Ok(path)
}

fn sessions(store: &Store, command: SessionsCommand) -> Result<()> {
    match command {
        SessionsCommand::List => history(store),
        SessionsCommand::Show { task_id } => show(store, &task_id),
        SessionsCommand::Cancel { task_id, reason } => cancel(store, &task_id, &reason),
        SessionsCommand::Trace { task_id, output } => trace(store, &task_id, output),
        SessionsCommand::Export {
            task_id,
            output_dir,
        } => export(store, &task_id, output_dir),
        SessionsCommand::Import { input } => import(store, input),
        SessionsCommand::Events { task_id } => events(store, &task_id),
    }
}

fn start(store: &Store, text: String) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let task = store.create_session(None, &cwd)?;
    store.append_event(
        &task.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&text, &cwd)?,
    )?;
    maybe_append_message_history(&task.id, &text, &cwd, &AgentRunOptions::default());
    println!("{}", task.id);
    Ok(())
}

fn run_new_session_from_config(
    store: &Store,
    text: String,
    config: ProviderRunConfig,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let session = store.create_session(None, &cwd)?;
    store.append_event(
        &session.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&text, &cwd)?,
    )?;
    maybe_append_message_history(&session.id, &text, &cwd, &config.options);
    let session_id = run_session_via_engine(store, &session.id, config)?;
    println!("{session_id}");
    Ok(())
}

fn maybe_append_message_history(
    session_id: &str,
    text: &str,
    cwd: &Path,
    options: &AgentRunOptions,
) {
    let _ = options;
    #[cfg(not(test))]
    {
        // The new engine's message-history layer does not yet re-resolve the
        // AGENTS.md-derived `MessageHistorySettings` from `AgentRunOptions`
        // (documented Phase-E seam in `browser-use-agent::history`); the CLI
        // therefore persists with the default settings (SaveAll), matching the
        // default run behavior.
        let _ = browser_use_agent::history::append_message_history_entry_for_cwd(
            text,
            session_id,
            cwd,
            browser_use_agent::history::MessageHistorySettings::default(),
        );
    }
    #[cfg(test)]
    {
        let _ = (session_id, text, cwd);
    }
}

/// Drive a session through the live runtime executor.
///
/// The CLI is synchronous, while the agent engine is async. This bridge now
/// creates a `RuntimeAgentExecutor` over the CLI runtime handle, so cancellation,
/// child runs, mailboxes, and session resources use the same live authority as
/// the TUI/SDK instead of a one-off engine runtime.
///
/// Replaces the legacy `run_existing_session_from_config` /
/// `run_agent_from_config` / `run_existing_session_with_provider` /
/// `run_fake_agent` engine entrypoints.
fn run_session_via_engine(
    store: &Store,
    session_id: &str,
    config: ProviderRunConfig,
) -> Result<String> {
    let runtime_handle = cli_runtime_handle(store)?;
    run_session_via_engine_with_runtime(store, session_id, config, runtime_handle)
}

fn run_session_via_engine_with_runtime(
    store: &Store,
    session_id: &str,
    config: ProviderRunConfig,
    runtime_handle: RuntimeHandle,
) -> Result<String> {
    run_session_via_engine_with_runtime_and_cancel(
        store,
        session_id,
        config,
        runtime_handle,
        tokio_util::sync::CancellationToken::new(),
        None,
    )
}

fn run_session_via_engine_with_runtime_and_cancel(
    store: &Store,
    session_id: &str,
    mut config: ProviderRunConfig,
    runtime_handle: RuntimeHandle,
    cancellation_token: tokio_util::sync::CancellationToken,
    browser_id: Option<BrowserId>,
) -> Result<String> {
    let _local_runtime_server = CliLocalRuntimeServer::ensure(store, &runtime_handle)?;
    let executor = cli_runtime_agent_executor(store, runtime_handle)?;
    attach_cli_child_agent_runner(store, executor.clone(), &mut config);
    let mut request = RuntimeAgentRunRequest::new(session_id.to_string(), config)
        .with_cancellation_token(cancellation_token);
    let root_cancel = request
        .cancellation_token
        .clone()
        .unwrap_or_else(tokio_util::sync::CancellationToken::new);
    if let Some(browser_id) = browser_id {
        request = request.with_browser_id(browser_id);
    }
    let resolved = executor.run_blocking(request)?;
    if root_cancel.is_cancelled() {
        let _ = executor.wait_for_background_idle(Duration::from_secs(30));
    }
    Ok(resolved.session_id)
}

struct CliLocalRuntimeServer {
    owned_socket_path: Option<PathBuf>,
}

impl CliLocalRuntimeServer {
    fn ensure(store: &Store, runtime: &RuntimeHandle) -> Result<Self> {
        Self::ensure_for_state_dir(store.state_dir(), runtime)
    }

    #[cfg(unix)]
    fn ensure_for_state_dir(state_dir: &Path, runtime: &RuntimeHandle) -> Result<Self> {
        let existing_live_server = send_local_runtime_request(
            state_dir,
            &LocalRuntimeRequest::Ping,
            Duration::from_millis(100),
        )?
        .is_some_and(|response| response.ok);
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(state_dir, runtime.clone())?;
        Ok(Self {
            owned_socket_path: (!existing_live_server).then_some(socket_path),
        })
    }

    #[cfg(not(unix))]
    fn ensure_for_state_dir(_state_dir: &Path, _runtime: &RuntimeHandle) -> Result<Self> {
        Ok(Self {
            owned_socket_path: None,
        })
    }
}

impl Drop for CliLocalRuntimeServer {
    fn drop(&mut self) {
        if let Some(socket_path) = self.owned_socket_path.take() {
            let _ = fs::remove_file(socket_path);
        }
    }
}

fn cli_runtime_handle(store: &Store) -> Result<RuntimeHandle> {
    let journal = std::sync::Arc::new(SqliteJournal::from_store(Store::open(store.state_dir())?));
    let persistence: std::sync::Arc<dyn LiveThreadPersistence> = journal.clone();
    let state_index: std::sync::Arc<dyn StateIndex> = journal;
    Ok(BrowserUseRuntime::new(persistence, state_index).handle())
}

fn cli_runtime_agent_executor(
    store: &Store,
    runtime: RuntimeHandle,
) -> Result<RuntimeAgentExecutor> {
    RuntimeAgentExecutor::new(
        RuntimeAgentExecutorConfig::new(store.state_dir().to_path_buf(), runtime)
            .with_worker_threads(2),
    )
}

fn ensure_cli_agent_attached(
    runtime: &RuntimeHandle,
    store: &Store,
    session_id: &str,
    max_concurrent_threads_per_session: usize,
) -> Result<()> {
    ensure_runtime_agent_attached(
        runtime,
        store,
        session_id,
        max_concurrent_threads_per_session,
    )
}

fn attach_cli_child_agent_runner(
    store: &Store,
    executor: RuntimeAgentExecutor,
    config: &mut ProviderRunConfig,
) {
    let state_dir = store.state_dir().to_path_buf();
    let base_config_slot: std::sync::Arc<std::sync::Mutex<Option<ProviderRunConfig>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = std::sync::Arc::clone(&base_config_slot);
    let runner = ChildAgentRunner::new(move |request| {
        let base_config = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .context("child agent runner base config not initialized")?;
        spawn_runtime_cli_child_agent(executor.clone(), state_dir.clone(), base_config, request)
    });
    config.options = config.options.clone().with_child_agent_runner(runner);
    *base_config_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(config.clone());
}

fn spawn_runtime_cli_child_agent(
    executor: RuntimeAgentExecutor,
    state_dir: PathBuf,
    mut base_config: ProviderRunConfig,
    request: ChildAgentRunRequest,
) -> Result<()> {
    let runtime_handle = executor.runtime_handle();
    let store = Store::open(&state_dir)?;
    ensure_cli_agent_attached(
        &runtime_handle,
        &store,
        &request.parent_session_id,
        base_config
            .options
            .multi_agent_v2
            .max_concurrent_threads_per_session,
    )?;
    let child = create_agent_child_session_from_request(&runtime_handle, &store, &request)?;
    let child_id = child.id.clone();
    record_child_run_marker_from_request(&store, &child_id, &request)?;
    apply_cli_child_request_to_config(&mut base_config, &request)?;
    let mut run_request = RuntimeAgentRunRequest::new(child_id.clone(), base_config);
    if let Some(run_id) = request.run_id.as_ref() {
        run_request = run_request.with_run_id(RuntimeRunId::from_string(run_id.clone())?);
    }
    executor.spawn_background(
        format!("browser-use-child-{child_id}"),
        run_request,
        move |completion| {
            let events = Store::open(&state_dir)
                .and_then(|store| store.events_for_session(&child_id))
                .ok();
            if let Some(child_completion) = cli_child_completion_from_background(
                completion.is_success(),
                completion.error_message(),
                events.as_deref(),
            ) {
                if let Err(error) = notify_cli_runtime_child_completion(
                    &runtime_handle,
                    &child_id,
                    &child_completion,
                ) {
                    eprintln!("child agent completion runtime update failed: {error:#}");
                }
            }
        },
    )?;
    Ok(())
}

fn apply_cli_child_request_to_config(
    config: &mut ProviderRunConfig,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    if let Some(model) = request.model.as_deref().filter(|value| !value.is_empty()) {
        config.model = model.to_string();
        config.model_source = RunConfigValueSource::Explicit;
    }
    if let Some(provider_id) = child_request_provider_id(request) {
        if let Some(backend) = ProviderBackend::from_provider_id(&provider_id) {
            config.backend = backend;
        }
        config.options.model_provider_id = Some(provider_id);
        config.options.model_provider_id_source = RunConfigValueSource::Explicit;
    }
    if !request.config_overrides.is_empty() {
        config
            .options
            .config_overrides
            .extend(request.config_overrides.clone());
        apply_child_request_runtime_config(config, request)?;
    }
    if let Some(reasoning) = request.reasoning_effort.clone() {
        config.options.config_overrides.push((
            "reasoning_effort".to_string(),
            toml::Value::String(reasoning),
        ));
    }
    if let Some(service_tier) = request.service_tier.clone() {
        config.options.config_overrides.push((
            "service_tier".to_string(),
            toml::Value::String(service_tier),
        ));
    }
    Ok(())
}

fn notify_cli_runtime_child_completion(
    runtime_handle: &RuntimeHandle,
    child_id: &str,
    completion: &ChildAgentRunCompletion,
) -> Result<()> {
    let child_agent_id = AgentId::from_string(child_id.to_string())?;
    let runtime_result = if completion.success {
        runtime_handle.complete_agent(CompleteAgentRequest {
            child_agent_id,
            result: completion.summary.clone().unwrap_or_default(),
        })
    } else {
        runtime_handle.fail_agent(FailAgentRequest {
            child_agent_id,
            error: completion
                .summary
                .clone()
                .unwrap_or_else(|| "child agent failed".to_string()),
        })
    };
    runtime_result
}

fn cli_child_completion_from_background(
    success: bool,
    error: Option<String>,
    events: Option<&[browser_use_protocol::EventRecord]>,
) -> Option<ChildAgentRunCompletion> {
    if success {
        if events.is_some_and(child_run_should_skip_success_completion) {
            return None;
        }
        let summary = events.and_then(session_result_from_events);
        Some(ChildAgentRunCompletion::success(summary))
    } else {
        Some(ChildAgentRunCompletion::failure(
            error.unwrap_or_else(|| "child agent failed".to_string()),
        ))
    }
}

fn child_run_should_skip_success_completion(events: &[browser_use_protocol::EventRecord]) -> bool {
    child_run_was_interrupted_from_events(events) || child_run_latest_terminal_is_cancelled(events)
}

fn child_run_latest_terminal_is_cancelled(events: &[browser_use_protocol::EventRecord]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.event_type.as_str(),
                "session.cancelled"
                    | "session.interrupted"
                    | "session.input"
                    | "session.followup"
                    | "session.done"
                    | "session.failed"
            )
        })
        .is_some_and(|event| event.event_type == "session.cancelled")
}

fn child_request_provider_id(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| {
            matches!(
                key.as_str(),
                "model_provider" | "model_provider_id" | "provider"
            )
        })
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn create_agent_child_session_from_request(
    runtime_handle: &RuntimeHandle,
    store: &Store,
    request: &ChildAgentRunRequest,
) -> Result<browser_use_protocol::SessionMeta> {
    if let Some(existing) = store.load_session(&request.child_session_id)? {
        return Ok(existing);
    }
    ensure_task_exists(store, &request.parent_session_id)?;
    runtime_handle.spawn_child(SpawnChildRequest {
        parent_agent_id: AgentId::from_string(request.parent_session_id.clone())?,
        child_agent_id: Some(AgentId::from_string(request.child_session_id.clone())?),
        child_session_id: Some(SessionId::from_string(request.child_session_id.clone())?),
        task_name: task_name_from_agent_path(request.agent_path.as_deref())
            .unwrap_or_else(|| request.child_session_id.clone()),
        message: request.message.clone(),
        nickname: request.nickname.clone(),
        role: request.role.clone(),
    })?;
    let child = store
        .load_session(&request.child_session_id)?
        .with_context(|| {
            format!(
                "runtime did not create child session {}",
                request.child_session_id
            )
        })?;
    let parent_events = store.events_for_session(&request.parent_session_id)?;
    store.append_event(
        &child.id,
        "agent.context",
        child_request_agent_context_payload(&parent_events, request)?,
    )?;
    seed_environment_context_event(store, &child.id, &child.cwd)?;
    seed_child_permissions_context_event(store, &child.id, request)?;
    append_child_initial_input_from_request(store, &child.id, &child.cwd, request)?;
    store.append_event(
        &request.parent_session_id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id.clone(),
            "agent_path": request.agent_path.clone(),
            "nickname": request.nickname.clone(),
            "role": request.role.clone(),
        }),
    )?;
    Ok(child)
}

fn task_name_from_agent_path(agent_path: Option<&str>) -> Option<String> {
    agent_path
        .and_then(|path| path.rsplit('/').find(|segment| !segment.trim().is_empty()))
        .map(ToOwned::to_owned)
}

fn record_child_run_marker_from_request(
    store: &Store,
    child_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(run_id) = request.run_id.as_deref() else {
        return Ok(());
    };
    let config_overrides = request
        .config_overrides
        .iter()
        .map(|(key, value)| {
            serde_json::json!({
                "key": key,
                "value": value,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        child_id,
        "agent.run.started",
        serde_json::json!({
            "run_id": run_id,
            "parent_session_id": request.parent_session_id.as_str(),
            "child_session_id": child_id,
            "agent_path": request.agent_path.as_deref(),
            "model": request.model.as_deref(),
            "reasoning_effort": request.reasoning_effort.as_deref(),
            "service_tier": request.service_tier.as_deref(),
            "config_overrides": config_overrides,
        }),
    )?;
    Ok(())
}

fn append_child_initial_input_from_request(
    store: &Store,
    child_id: &str,
    child_cwd: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    if request.input_is_inter_agent_communication {
        let author_path = display_agent_path_for_session(store, &request.parent_session_id)
            .unwrap_or_else(|_| "/root".to_string());
        let recipient_path = request.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(store, child_id).unwrap_or_else(|_| child_id.to_string())
        });
        store.append_event(
            child_id,
            "agent.mailbox_input",
            serde_json::json!({
                "id": browser_use_store::new_thread_id(),
                "author_session_id": request.parent_session_id,
                "target_session_id": child_id,
                "author_path": author_path,
                "recipient_path": recipient_path,
                "content": request.message,
                "trigger_turn": true,
            }),
        )?;
    } else {
        let payload = if let Some(items) = request.input_items.as_ref() {
            typed_user_input_payload_from_items_for_cwd(items, child_cwd)?
        } else {
            typed_user_input_payload_from_text_for_cwd(&request.message, child_cwd)?
        };
        store.append_event(child_id, "session.input", payload)?;
    }
    Ok(())
}

fn child_request_agent_context_payload(
    parent_events: &[browser_use_protocol::EventRecord],
    request: &ChildAgentRunRequest,
) -> Result<serde_json::Value> {
    let mode = child_request_fork_mode(request.fork_turns.as_deref())?;
    let forked = fork_events_by_turn(parent_events, &mode);
    let history = provider_messages_from_events_for_fork(&forked.carried);
    let response_items = provider_messages_to_fork_response_items(&history);
    let raw_mode = request
        .fork_turns
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    let mut payload = serde_json::json!({
        "from_session_id": request.parent_session_id.clone(),
        "fork_mode": raw_mode,
        "agent_path": request.agent_path.clone(),
        "nickname": request.nickname.clone(),
        "role": request.role.clone(),
    });
    if matches!(mode, ForkMode::None) {
        payload["history_mode"] = serde_json::json!("none");
    } else {
        payload["history_mode"] = serde_json::json!("fork_response_items");
        payload["fork_response_items"] = serde_json::Value::Array(response_items);
    }
    Ok(payload)
}

fn child_request_fork_mode(raw: Option<&str>) -> Result<ForkMode> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    if value.eq_ignore_ascii_case("none") {
        return Ok(ForkMode::None);
    }
    if value.eq_ignore_ascii_case("all") {
        return Ok(ForkMode::All);
    }
    let turns = value
        .parse::<usize>()
        .with_context(|| "fork_turns must be `none`, `all`, or a positive integer string")?;
    if turns == 0 {
        bail!("fork_turns must be `none`, `all`, or a positive integer string");
    }
    Ok(ForkMode::LastN(turns))
}

fn run_fake(store: &Store, text: String, python_code: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let session = store.create_session(None, &cwd)?;
    store.append_event(
        &session.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&text, &cwd)?,
    )?;
    let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake")
        .with_fake_result(fake_agent_result_text(&text, python_code.as_deref()));
    let session_id = run_session_via_engine(store, &session.id, config)?;
    println!("{session_id}");
    Ok(())
}

/// The fake-backend reply text for a `run-fake` invocation.
///
/// Parity with the legacy `run_fake_agent` scripted provider (browser-use-core
/// `lib.rs:869`): without `python_code` it replays `Fake result for: {text}`;
/// with `python_code` the legacy fake ran a python tool then `done`. The new
/// engine's `Fake` backend has no tool dispatch, so the python branch is carried
/// as a stable completion string (the FakeAgentOptions/python tool path is a
/// documented engine seam — see report).
fn fake_agent_result_text(text: &str, python_code: Option<&str>) -> String {
    match python_code {
        Some(_) => "Python tool completed.".to_string(),
        None => format!("Fake result for: {text}"),
    }
}

fn cli_agent_options(
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<AgentRunOptions> {
    let mut options = AgentRunOptions::default()
        .with_collaboration_mode(collaboration_mode)
        .with_browser_mode(cli_browser_mode())
        .with_model_compaction(true)
        .with_analytics_source("cli");
    if let Some(profile) = config_profile {
        options = options.with_config_profile(profile.to_string());
    }
    let config_overrides = parse_cli_config_overrides(raw_config_overrides)?;
    if let Some(policy) = resolve_approval_policy_for_profile(
        config_profile,
        &config_overrides,
        runtime_options.approval_policy,
    )? {
        options = options.with_approval_policy(policy);
    }
    if let Some(use_guardian) = resolve_guardian_for_profile(
        config_profile,
        &config_overrides,
        runtime_options.use_guardian,
    )? {
        options = options.with_guardian(use_guardian);
    }
    options = options.with_multi_agent_v2(resolve_multi_agent_v2_for_profile(
        config_profile,
        &config_overrides,
    )?);
    options = options.with_collab_enabled(resolve_collab_for_profile(
        config_profile,
        &config_overrides,
    )?);
    options = options.with_agent_roles(resolve_agent_roles_for_profile(
        config_profile,
        &config_overrides,
    )?);
    let mcp_servers =
        load_mcp_servers_for_profile(config_profile, &runtime_options.mcp_config_paths)?;
    if !mcp_servers.is_empty() {
        options = options.with_mcp_servers(mcp_servers);
    }
    if !config_overrides.is_empty() {
        apply_runtime_config_overrides(&mut options, &config_overrides)?;
        options = options.with_config_overrides(config_overrides);
    }
    Ok(options)
}

fn resolve_cli_model_with_source(
    backend: ProviderBackend,
    explicit_model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
) -> Result<(String, RunConfigValueSource)> {
    if let Some(model) = explicit_model {
        return Ok((model, RunConfigValueSource::Explicit));
    }
    let config_overrides = parse_cli_config_overrides(raw_config_overrides)?;
    let model_source = if config_overrides.iter().any(|(key, _)| key == "model") {
        RunConfigValueSource::Explicit
    } else {
        RunConfigValueSource::Default
    };
    Ok((
        default_cli_model_for_backend_with_overrides(backend, config_profile, &config_overrides)?,
        model_source,
    ))
}

fn default_cli_model_for_backend_with_overrides(
    backend: ProviderBackend,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<String> {
    let cwd = std::env::current_dir()?;
    match backend {
        ProviderBackend::Openai => {
            default_model_for_cwd_with_options(cwd, config_profile, config_overrides, false)
        }
        ProviderBackend::Anthropic => Ok("claude-sonnet-4-6".to_string()),
        ProviderBackend::Openrouter => Ok("openai/gpt-5.5".to_string()),
        ProviderBackend::Deepseek => Ok("deepseek-v4-pro".to_string()),
        ProviderBackend::Codex | ProviderBackend::Fake | ProviderBackend::None => {
            Ok("fake".to_string())
        }
    }
}

fn default_provider_id_for_backend(backend: ProviderBackend) -> &'static str {
    match backend {
        ProviderBackend::Openai => "openai",
        ProviderBackend::Anthropic => "anthropic",
        ProviderBackend::Openrouter => "openrouter",
        ProviderBackend::Deepseek => "deepseek",
        ProviderBackend::Fake => "fake",
        ProviderBackend::Codex | ProviderBackend::None => "none",
    }
}

fn resolved_cli_provider_id_for_backend_with_overrides(
    backend: ProviderBackend,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<String> {
    Ok(configured_model_provider_id_for_cwd_with_options(
        std::env::current_dir()?,
        config_profile,
        config_overrides,
    )?
    .unwrap_or_else(|| default_provider_id_for_backend(backend).to_string()))
}

fn cli_provider_id_source(config_overrides: &[(String, toml::Value)]) -> RunConfigValueSource {
    if config_overrides
        .iter()
        .any(|(key, _)| key == "model_provider")
    {
        RunConfigValueSource::Explicit
    } else {
        RunConfigValueSource::Default
    }
}

fn parse_cli_config_overrides(raw_config_overrides: &[String]) -> Result<ConfigOverrides> {
    parse_config_overrides(raw_config_overrides)
}

fn cli_browser_mode() -> String {
    std::env::var("LLM_BROWSER_BROWSER_MODE")
        .ok()
        .filter(|mode| !mode.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

fn dataset_browser_mode(options: &DatasetRunOptions) -> String {
    options
        .browser_mode
        .as_deref()
        .filter(|mode| !mode.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(cli_browser_mode)
        .to_ascii_lowercase()
        .replace(['_', ' '], "-")
}

fn run_openai(
    store: &Store,
    text: String,
    model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Openai,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config = ProviderRunConfig::new(ProviderBackend::Openai, model)
        .with_model_source(model_source)
        .with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    run_new_session_from_config(store, text, config)
}

fn run_anthropic(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    run_new_session_from_config(store, text, config)
}

fn run_openrouter(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Openrouter, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    run_new_session_from_config(store, text, config)
}

fn run_deepseek(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Deepseek, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    run_new_session_from_config(store, text, config)
}

/// Run a task against the codex (chatgpt.com) backend.
///
/// The codex OAuth credentials are resolved inside the engine
/// ([`browser_use_agent::entrypoint::provider`]) env-first
/// (`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID`), then from the Store settings the
/// `auth login codex` / `auth import-codex` commands write
/// (`auth.codex.access_token` / `auth.codex.account_id`), then `~/.codex/auth.json`.
fn run_codex(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Codex, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    run_new_session_from_config(store, text, config)
}

fn run_codex_session(
    store: &Store,
    task_id: &str,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Codex, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config, None)?;
    println!("{session_id}");
    Ok(())
}

fn run_openai_session(
    store: &Store,
    task_id: &str,
    model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Openai,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config = ProviderRunConfig::new(ProviderBackend::Openai, model)
        .with_model_source(model_source)
        .with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config, None)?;
    println!("{session_id}");
    Ok(())
}

fn run_anthropic_session(
    store: &Store,
    task_id: &str,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config, None)?;
    println!("{session_id}");
    Ok(())
}

fn run_openrouter_session(
    store: &Store,
    task_id: &str,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Openrouter, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config, None)?;
    println!("{session_id}");
    Ok(())
}

fn run_deepseek_session(
    store: &Store,
    task_id: &str,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Deepseek, model).with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
            runtime_options,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config, None)?;
    println!("{session_id}");
    Ok(())
}

fn run_existing_session_from_config_and_notify(
    store: &Store,
    task_id: &str,
    config: ProviderRunConfig,
    expected_run_id: Option<String>,
) -> Result<String> {
    let result = run_session_via_engine(store, task_id, config);
    let run_error = result.as_ref().err().map(|error| format!("{error:#}"));
    let child_id = result.as_deref().unwrap_or(task_id);
    notify_parent_after_cli_child_run(store, child_id, run_error, expected_run_id.as_deref())?;
    result
}

fn notify_parent_after_cli_child_run(
    store: &Store,
    child_id: &str,
    run_error: Option<String>,
    expected_run_id: Option<&str>,
) -> Result<()> {
    let Some(child) = store.load_session(child_id)? else {
        return Ok(());
    };
    let Some(parent_id) = child.parent_id.as_deref() else {
        return Ok(());
    };
    update_parent_from_child_run(store, parent_id, child_id, run_error, expected_run_id)?;
    Ok(())
}

/// Store-based parent-link projection run after a child agent terminates.
///
/// Writes the parent's terminal `agent.{completed,failed,cancelled,updated}`
/// event and flips the child edge status. It deliberately does not enqueue
/// Store-backed `agent_messages`; live completion delivery belongs to the
/// runtime mailbox.
fn update_parent_from_child_run(
    store: &Store,
    parent_id: &str,
    child_id: &str,
    run_error: Option<String>,
    expected_run_id: Option<&str>,
) -> Result<Value> {
    let child = store
        .load_session(child_id)?
        .with_context(|| format!("unknown child session id: {child_id}"))?;
    let child_events = store.events_for_session(child_id)?;
    let latest_run_id = latest_child_run_id_from_events(&child_events);
    let run_id = expected_run_id.map(ToOwned::to_owned).or(latest_run_id);
    let child_run_events = if let Some(expected_run_id) = expected_run_id {
        let Some(current_events) = current_child_run_events(&child_events, expected_run_id) else {
            return Ok(serde_json::json!({
                "child_session_id": child_id,
                "run_id": run_id.as_deref(),
                "status": "stale",
                "result": null,
                "failure": null,
            }));
        };
        current_events
    } else {
        child_events.as_slice()
    };
    if store
        .agent_summary_for_child(child_id)?
        .is_some_and(|summary| summary.status == "closed")
    {
        return Ok(serde_json::json!({
            "child_session_id": child_id,
            "run_id": run_id.as_deref(),
            "status": "closed",
            "result": null,
            "failure": null,
        }));
    }
    if parent_has_child_terminal_event_for_run(store, parent_id, child_id, run_id.as_deref())? {
        return Ok(serde_json::json!({
            "child_session_id": child_id,
            "run_id": run_id.as_deref(),
            "status": "duplicate",
            "result": null,
            "failure": null,
        }));
    }
    let terminal = latest_child_terminal_from_events(child_run_events);
    let result = terminal
        .as_ref()
        .and_then(|terminal| terminal.result.clone())
        .or_else(|| session_result_from_events(child_run_events));
    let failure = terminal
        .as_ref()
        .and_then(|terminal| terminal.failure.clone())
        .or_else(|| run_error.clone())
        .or_else(|| failure_from_events(child_run_events));
    let status = child.status.as_str().to_string();
    if status == "cancelled" && child_run_was_interrupted_from_events(child_run_events) {
        store.set_child_agent_status(child_id, "open")?;
        let payload = serde_json::json!({
            "child_session_id": child_id,
            "run_id": run_id.as_deref(),
            "status": "interrupted",
            "result": result,
            "failure": failure,
        });
        store.append_event(
            parent_id,
            "agent.updated",
            serde_json::json!({
                "child_session_id": child_id,
                "run_id": run_id.as_deref(),
                "status": "interrupted",
                "payload": payload,
            }),
        )?;
        return Ok(payload);
    }
    let event_type = match status.as_str() {
        "done" => "agent.completed",
        "failed" => "agent.failed",
        "cancelled" => "agent.cancelled",
        _ => "agent.updated",
    };
    let edge_status = match status.as_str() {
        "done" | "failed" | "cancelled" => status.as_str(),
        _ => "open",
    };
    store.set_child_agent_status(child_id, edge_status)?;
    let payload = serde_json::json!({
        "child_session_id": child_id,
        "run_id": run_id.as_deref(),
        "status": status,
        "result": result,
        "failure": failure,
    });
    store.append_event(
        parent_id,
        event_type,
        serde_json::json!({
            "child_session_id": child_id,
            "run_id": run_id.as_deref(),
            "status": status,
            "payload": payload,
        }),
    )?;
    Ok(payload)
}

struct ChildTerminal {
    result: Option<String>,
    failure: Option<String>,
}

fn latest_child_terminal_from_events(
    events: &[browser_use_protocol::EventRecord],
) -> Option<ChildTerminal> {
    events
        .iter()
        .rev()
        .find(|event| matches!(event.event_type.as_str(), "session.done" | "session.failed"))
        .map(|event| match event.event_type.as_str() {
            "session.done" => ChildTerminal {
                result: session_result_from_events(std::slice::from_ref(event)),
                failure: None,
            },
            "session.failed" => ChildTerminal {
                result: None,
                failure: failure_from_events(std::slice::from_ref(event))
                    .or_else(|| Some("failed".to_string())),
            },
            _ => ChildTerminal {
                result: None,
                failure: None,
            },
        })
}

fn latest_child_run_id_from_events(events: &[browser_use_protocol::EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        (event.event_type == "agent.run.started")
            .then(|| event.payload.get("run_id").and_then(Value::as_str))
            .flatten()
            .map(ToOwned::to_owned)
    })
}

fn current_child_run_events<'a>(
    events: &'a [browser_use_protocol::EventRecord],
    expected_run_id: &str,
) -> Option<&'a [browser_use_protocol::EventRecord]> {
    let marker_idx = events
        .iter()
        .rposition(|event| event.event_type == "agent.run.started")?;
    let marker = &events[marker_idx];
    let marker_run_id = marker.payload.get("run_id").and_then(Value::as_str)?;
    (marker_run_id == expected_run_id).then_some(&events[marker_idx + 1..])
}

fn parent_has_child_terminal_event_for_run(
    store: &Store,
    parent_id: &str,
    child_id: &str,
    run_id: Option<&str>,
) -> Result<bool> {
    Ok(store.events_for_session(parent_id)?.iter().any(|event| {
        if !matches!(
            event.event_type.as_str(),
            "agent.completed" | "agent.failed" | "agent.cancelled"
        ) {
            return false;
        }
        if event
            .payload
            .get("child_session_id")
            .or_else(|| event.payload.pointer("/payload/child_session_id"))
            .and_then(Value::as_str)
            != Some(child_id)
        {
            return false;
        }
        if event
            .payload
            .get("runtime_owned")
            .or_else(|| event.payload.pointer("/payload/runtime_owned"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return true;
        }
        match run_id {
            Some(run_id) => {
                event
                    .payload
                    .get("run_id")
                    .or_else(|| event.payload.pointer("/payload/run_id"))
                    .and_then(Value::as_str)
                    == Some(run_id)
            }
            None => true,
        }
    }))
}

fn child_run_was_interrupted_from_events(events: &[browser_use_protocol::EventRecord]) -> bool {
    session_was_interrupted(events)
}

#[allow(clippy::too_many_arguments)]
fn capture_user_message(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_subagent: bool,
    kind: &str,
    seq: i64,
    text: &str,
) {
    let trimmed = text.trim();
    let char_count = trimmed.chars().count();
    let word_count = if trimmed.is_empty() {
        0
    } else {
        trimmed.split_whitespace().count()
    };
    let approx_tokens = char_count.div_ceil(APPROX_CHARS_PER_TOKEN);
    capture_async(
        store,
        "bu:tui user_message",
        serde_json::json!({
            "surface": surface,
            "session_id": session_id,
            "is_subagent": is_subagent,
            "kind": kind,
            "seq": seq,
            "char_count": char_count,
            "word_count": word_count,
            "approx_tokens": approx_tokens,
        }),
    );
}

fn followup(store: &Store, task_id: &str, text: String) -> Result<()> {
    let session = ensure_task_exists(store, task_id)?;
    if let Some(seq) = followup_via_live_runtime(store, &session, &text)? {
        capture_user_message(
            store,
            "cli",
            task_id,
            session.parent_id.is_some(),
            MESSAGE_KIND_FOLLOWUP,
            seq,
            &text,
        );
        maybe_append_message_history(
            task_id,
            &text,
            Path::new(&session.cwd),
            &AgentRunOptions::default(),
        );
        println!("followup {task_id}");
        return Ok(());
    }
    let followup_record = store.append_event(
        task_id,
        "session.followup",
        typed_user_input_payload_from_text_for_cwd(&text, &session.cwd)?,
    )?;
    capture_user_message(
        store,
        "cli",
        task_id,
        session.parent_id.is_some(),
        MESSAGE_KIND_FOLLOWUP,
        followup_record.seq,
        &text,
    );
    maybe_append_message_history(
        task_id,
        &text,
        Path::new(&session.cwd),
        &AgentRunOptions::default(),
    );
    println!("followup {task_id}");
    Ok(())
}

fn followup_via_live_runtime(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    text: &str,
) -> Result<Option<i64>> {
    let payload = typed_user_input_payload_from_text_for_cwd(text, &session.cwd)?;
    let response = match send_local_runtime_request(
        store.state_dir(),
        &LocalRuntimeRequest::SubmitUserInput {
            session_id: session.id.clone(),
            content: text.to_string(),
            trigger_turn: true,
            delivery_phase: RuntimeMailboxDeliveryPhase::CurrentTurn,
            input_items: payload.get("items").cloned(),
            payload: serde_json::json!({ "source": "cli" }),
        },
        Duration::from_millis(500),
    )? {
        Some(response) if response.ok => response,
        _ => return Ok(None),
    };
    let mailbox_item = response
        .result
        .get("mailbox_item")
        .cloned()
        .unwrap_or(Value::Null);
    let record = store.append_event(
        &session.id,
        "session.followup.runtime_queued",
        serde_json::json!({
            "source": "cli",
            "runtime_mailbox_id": mailbox_item.get("id").and_then(Value::as_str),
            "runtime_mailbox_seq": mailbox_item.get("seq").and_then(Value::as_u64),
        }),
    )?;
    Ok(Some(record.seq))
}

fn finish(store: &Store, task_id: &str, result: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.done",
        serde_json::json!({ "result": result.clone() }),
    )?;
    notify_parent_agent_done(store, &task)?;
    println!("done {task_id}");
    Ok(())
}

fn cancel(store: &Store, task_id: &str, reason: &str) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    let live_cancelled = cancel_via_live_runtime(store, task_id)?;
    if live_cancelled {
        store.append_event(
            task_id,
            "runtime.cancel.forwarded",
            serde_json::json!({ "source": "cli", "reason": reason }),
        )?;
        println!("cancelled {task_id}");
        return Ok(());
    }
    store.request_cancel(task_id, reason)?;
    cleanup_agent_runtime_state_for_agent_subtree(store, task_id, |_| 0)?;
    notify_parent_agent_done(store, &task)?;
    println!("cancelled {task_id}");
    Ok(())
}

fn cancel_via_live_runtime(store: &Store, task_id: &str) -> Result<bool> {
    let request = LocalRuntimeRequest::CancelRun {
        session_id: task_id.to_string(),
    };
    let Some(response) =
        send_local_runtime_request(store.state_dir(), &request, Duration::from_secs(5))?
    else {
        return Ok(false);
    };
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| "local runtime cancel failed".to_string());
        if error.contains("unknown agent") {
            return Ok(false);
        }
        bail!(error);
    }
    Ok(response
        .result
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

fn fail(store: &Store, task_id: &str, error: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.failed",
        serde_json::json!({ "error": error.clone() }),
    )?;
    notify_parent_agent_done(store, &task)?;
    println!("failed {task_id}");
    Ok(())
}

fn history(store: &Store) -> Result<()> {
    let tasks = store.list_sessions()?;
    if tasks.is_empty() {
        println!("No previous work yet.");
        return Ok(());
    }
    for task in tasks {
        let events = store.events_for_session(&task.id)?;
        let title = task_from_events(&events).unwrap_or_else(|| "untitled task".to_string());
        println!("{}  {:<9}  {}", task.id, task.status.as_str(), title);
    }
    Ok(())
}

fn show(store: &Store, task_id: &str) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    let events = store.events_for_session(task_id)?;
    let title = task_from_events(&events).unwrap_or_else(|| "untitled task".to_string());
    let browser = browser_summary_from_events(&events, "local chrome");
    println!("Task: {title}");
    println!("Status: {}", task.status.as_str());
    if let Some(url) = browser.url {
        println!("Browser: {url}");
    }
    if let Some(terminal) = latest_child_terminal_from_events(&events) {
        if let Some(result) = terminal.result {
            println!();
            println!("Result");
            println!("{result}");
        }
        if let Some(error) = terminal.failure {
            println!();
            println!("Failure");
            println!("{error}");
        }
    }
    Ok(())
}

fn events(store: &Store, task_id: &str) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    for event in store.events_for_session(task_id)? {
        println!("{}", serde_json::to_string(&event)?);
    }
    Ok(())
}

fn python(store: &Store, task_id: &str, code: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "tool.started",
        serde_json::json!({
            "name": "python",
            "arguments": { "code": code.clone() },
        }),
    )?;
    let browser_mode = cli_browser_mode();
    let agent_workspace = store
        .state_dir()
        .join("agent-workspace")
        .display()
        .to_string();
    let worker_env = [("BH_AGENT_WORKSPACE", agent_workspace.as_str())];
    let mut worker =
        PythonWorker::start_with_browser_mode_and_env(Some(&browser_mode), worker_env)?;
    let mut stream_error = None;
    let response =
        worker.run_with_events(task_id, &task.cwd, &task.artifact_root, &code, |event| {
            if stream_error.is_none() {
                if let Err(err) = record_python_worker_event(store, task_id, &event) {
                    stream_error = Some(err);
                }
            }
        })?;
    if let Some(err) = stream_error {
        return Err(err);
    }
    record_python_response_final_event(store, task_id, &response)?;
    if response.ok {
        store.append_event(
            task_id,
            "tool.finished",
            serde_json::json!({ "name": "python" }),
        )?;
        print!("{}", response.text);
        return Ok(());
    }
    store.append_event(
        task_id,
        "tool.failed",
        serde_json::json!({
            "name": "python",
            "error": response.error,
        }),
    )?;
    bail!(
        "{}",
        response
            .error
            .unwrap_or_else(|| "python worker failed".to_string())
    )
}

fn browser_script(store: &Store, task_id: &str, code: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    let tool_call_id = format!("browser_script-cli-{task_id}");
    if let Some(connect_command) = remote_cdp_connect_command_from_env() {
        let connect = browser_use_browser::run_browser_command(
            task_id,
            &task.cwd,
            &task.artifact_root,
            &connect_command,
        )?;
        if connect.content.get("status").and_then(Value::as_str) != Some("connected") {
            bail!("browser connect remote-cdp failed: {}", connect.content);
        }
    }
    store.append_event(
        task_id,
        "tool.started",
        serde_json::json!({
            "name": "browser_script",
            "tool_call_id": tool_call_id,
            "arguments": { "code": code.clone() },
        }),
    )?;
    let response = browser_use_browser::run_browser_script(
        task_id,
        &task.cwd,
        &task.artifact_root,
        &code,
        30,
    )?;
    record_browser_script_response_events(store, task_id, &tool_call_id, &response)?;
    if response.ok {
        store.append_event(
            task_id,
            "tool.finished",
            serde_json::json!({ "name": "browser_script", "tool_call_id": tool_call_id }),
        )?;
        print!("{}", response.text);
        return Ok(());
    }
    store.append_event(
        task_id,
        "tool.failed",
        serde_json::json!({
            "name": "browser_script",
            "tool_call_id": tool_call_id,
            "error": response.error,
        }),
    )?;
    bail!(
        "{}",
        response
            .error
            .unwrap_or_else(|| "browser_script failed".to_string())
    )
}

fn remote_cdp_connect_command_from_env() -> Option<String> {
    std::env::var("BU_CDP_WS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("BU_CDP_URL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .map(|endpoint| {
            let flag = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
                "--ws"
            } else {
                "--url"
            };
            format!(
                "browser connect remote-cdp {flag} {}",
                shell_quote_arg(&endpoint)
            )
        })
}

#[derive(Clone, Debug)]
struct SyncCookiesArgs {
    profile: Option<String>,
    local_profile: Option<String>,
    all_cookies: bool,
    domains: Vec<String>,
    exclude_domains: Vec<String>,
    cloud_profile_id: Option<String>,
    cloud_profile_name: Option<String>,
    new_cloud_profile_name: Option<String>,
}

fn run_cookie_sync_browser_command(store: &Store, args: &[String]) -> Result<Value> {
    let browser_use_api_key = browser_use_api_key_from_store_or_env(store)?;
    let cwd = std::env::current_dir()?;
    let artifact_root = cli_browser_artifact_root(store)?;
    let options = browser_use_browser::BrowserCommandOptions {
        browser_use_api_key,
    };
    Ok(browser_use_browser::run_browser_command_with_options(
        "cli-browser",
        &cwd,
        &artifact_root,
        &browser_command_from_args(args),
        options,
    )?
    .content)
}

fn sync_cookies(store: &Store, args: SyncCookiesArgs) -> Result<()> {
    if args.all_cookies && !args.domains.is_empty() {
        bail!("pass --all-cookies or --domain filters, not both");
    }
    let profile = args.local_profile.or(args.profile);
    let mut browser_args = vec!["profile".to_string(), "sync".to_string()];
    if let Some(profile) = profile {
        browser_args.extend(["--profile".to_string(), profile]);
    }
    if args.all_cookies || args.domains.is_empty() {
        browser_args.push("--all-cookies".to_string());
    }
    for domain in args.domains {
        browser_args.extend(["--domain".to_string(), domain]);
    }
    for domain in args.exclude_domains {
        browser_args.extend(["--exclude-domain".to_string(), domain]);
    }
    if let Some(profile_id) = args.cloud_profile_id {
        browser_args.extend(["--cloud-profile-id".to_string(), profile_id]);
    }
    if let Some(profile_name) = args.cloud_profile_name {
        browser_args.extend(["--cloud-profile-name".to_string(), profile_name]);
    }
    if let Some(profile_name) = args.new_cloud_profile_name {
        browser_args.extend(["--new-cloud-profile-name".to_string(), profile_name]);
    }
    let output = run_cookie_sync_browser_command(store, &browser_args)?;
    print_json_value(&output)
}

fn browser_use_api_key_from_store_or_env(store: &Store) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV) {
        if !value.trim().is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(store
        .get_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?
        .filter(|value| !value.trim().is_empty()))
}

fn cli_browser_artifact_root(store: &Store) -> Result<PathBuf> {
    let root = store.state_dir().join("cli-browser-artifacts");
    fs::create_dir_all(&root)?;
    Ok(root)
}

fn browser_command_from_args(args: &[String]) -> String {
    let mut command = String::from("browser");
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote_arg(arg));
    }
    command
}

fn shell_quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',')
        })
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r#"'\''"#))
}

fn print_json_value(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

fn user_shell(store: &Store, task_id: &str, command: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let started = Instant::now();
    let mut process = std::process::Command::new(shell);
    process
        .arg("-lc")
        .arg(&command)
        .current_dir(&task.cwd)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("LANG", "C.UTF-8")
        .env("LC_CTYPE", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("COLORTERM", "")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GH_PAGER", "cat")
        .env("CODEX_CI", "1")
        .env("CODEX_THREAD_ID", task_id);
    let output = process
        .output()
        .with_context(|| format!("run user shell command for session {task_id}"))?;
    let duration = started.elapsed();
    let exit_code = output.status.code().unwrap_or(-1);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let shared: SharedStore =
        std::sync::Arc::new(std::sync::Mutex::new(Store::open(store.state_dir())?));
    let runtime =
        tokio::runtime::Runtime::new().context("build tokio runtime for user shell context")?;
    runtime.block_on(append_user_shell_command_context_event(
        shared, task_id, &command, exit_code, duration, &combined,
    ))?;
    print!("{combined}");
    if output.status.success() {
        Ok(())
    } else {
        bail!("user shell command exited with status {}", output.status)
    }
}

fn review(
    store: &Store,
    base: Option<String>,
    commit: Option<String>,
    custom: Option<String>,
) -> Result<()> {
    let selected = base.is_some() as u8 + commit.is_some() as u8 + custom.is_some() as u8;
    if selected > 1 {
        bail!("review accepts only one of --base, --commit, or --custom");
    }
    let cwd = std::env::current_dir()?;
    let prompt = if let Some(custom) = custom {
        review_prompt_custom(&custom)?
    } else if let Some(commit) = commit {
        review_prompt_commit(&cwd, &commit)
    } else if let Some(base) = base {
        review_prompt_base_branch(&cwd, &base)
    } else {
        review_prompt_uncommitted_changes()
    };
    let session_id = start_review_session(store, &prompt, &cwd)?;
    println!("{session_id}");
    Ok(())
}

fn export(store: &Store, task_id: &str, output_dir: PathBuf) -> Result<()> {
    store.export_legacy_session(task_id, &output_dir)?;
    println!("{}", output_dir.display());
    Ok(())
}

fn import(store: &Store, input: PathBuf) -> Result<()> {
    let session = store.import_legacy_session(input)?;
    println!("{}", session.id);
    Ok(())
}

fn config(
    store: &Store,
    command: ConfigCommand,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
) -> Result<()> {
    let config_overrides = parse_cli_config_overrides(raw_config_overrides)?;
    match command {
        ConfigCommand::Init => {
            for (key, value) in default_settings(config_profile, &config_overrides)? {
                if store.get_setting(&key)?.is_none() {
                    store.set_setting(&key, &value)?;
                }
            }
            println!(
                "initialized {}",
                store.state_dir().join("state.db").display()
            );
            Ok(())
        }
        ConfigCommand::Show => {
            let mut settings = default_settings(config_profile, &config_overrides)?
                .into_iter()
                .map(|(key, value)| (key, value, true))
                .collect::<Vec<_>>();
            for (key, value) in store.list_settings()? {
                if let Some(existing) = settings.iter_mut().find(|(name, _, _)| name == &key) {
                    existing.1 = value;
                    existing.2 = false;
                } else {
                    settings.push((key, value, false));
                }
            }
            for (key, value, is_default) in settings {
                let suffix = if is_default { " (default)" } else { "" };
                let shown = if is_secret_setting(&key) {
                    "<stored>"
                } else {
                    value.as_str()
                };
                println!("{key}={shown}{suffix}");
            }
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            store.set_setting(&key, &value)?;
            println!("{key}={value}");
            Ok(())
        }
        ConfigCommand::Reset { target } => {
            match target {
                ConfigResetTarget::Onboarding => {
                    store.set_setting("setup.complete", "0")?;
                    println!("reset onboarding");
                }
                ConfigResetTarget::Profile => {
                    store.delete_setting("browser.preference.profile")?;
                    store.delete_setting("browser.preference.profile_label")?;
                    println!("reset profile");
                }
            }
            Ok(())
        }
    }
}

fn default_settings(
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<Vec<(String, String)>> {
    let provider_model = default_cli_model_for_backend_with_overrides(
        ProviderBackend::Openrouter,
        config_profile,
        config_overrides,
    )?;
    let display_model =
        display_model_for_provider_model(&provider_model, config_profile, config_overrides)?;
    let provider_id = resolved_cli_provider_id_for_backend_with_overrides(
        ProviderBackend::Openrouter,
        config_profile,
        config_overrides,
    )?;
    Ok(vec![
        ("account".to_string(), "OpenRouter API key".to_string()),
        ("model".to_string(), display_model),
        ("provider.model".to_string(), provider_model),
        ("provider.id".to_string(), provider_id),
        ("browser".to_string(), "Local Chrome".to_string()),
        ("agent.backend".to_string(), "openrouter".to_string()),
        ("setup.complete".to_string(), "0".to_string()),
    ])
}

fn display_model_for_provider_model(
    model: &str,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<String> {
    if model == "openai/gpt-5.5" {
        return Ok("GPT-5.5".to_string());
    }
    let cwd = std::env::current_dir()?;
    let catalog = model_catalog_for_cwd_with_options(cwd, config_profile, config_overrides)?;
    Ok(catalog
        .models
        .iter()
        .find(|entry| entry.slug == model)
        .map(|entry| entry.display_name.clone())
        .unwrap_or_else(|| model.to_string()))
}

fn is_secret_setting(key: &str) -> bool {
    key.starts_with("auth.")
        && (key.ends_with(".api_key")
            || key.ends_with(".access_token")
            || key.ends_with(".refresh_token")
            || key.ends_with(".auth_token"))
}

const BROWSER_USE_CLOUD_API_KEY_SETTING: &str = "auth.browser_use_cloud.api_key";
const BROWSER_USE_CLOUD_API_KEY_ENV: &str = "BROWSER_USE_API_KEY";

fn auth(store: &Store, command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Status => {
            print_api_key_status(
                store,
                "Browser Use Cloud key",
                BROWSER_USE_CLOUD_API_KEY_SETTING,
                &["BROWSER_USE_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "OpenAI API key",
                "auth.openai.api_key",
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
            )?;
            print_codex_status(store)?;
            print_api_key_status(
                store,
                "Anthropic API key",
                "auth.anthropic.api_key",
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "OpenRouter API key",
                "auth.openrouter.api_key",
                &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "DeepSeek API key",
                "auth.deepseek.api_key",
                &["LLM_BROWSER_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            )?;
            print_claude_code_status(store)?;
            Ok(())
        }
        AuthCommand::Login {
            account,
            api_key,
            access_token,
            account_id,
            code,
            no_browser,
        } => auth_login(
            store,
            account,
            api_key,
            access_token,
            account_id,
            code,
            no_browser,
        ),
        AuthCommand::ImportCodex { input } => {
            let auth = if let Some(input) = input {
                let managed_auth = load_codex_managed_auth_file(input)?;
                let auth = managed_auth.current_auth()?;
                store_codex_managed_auth(store, &managed_auth)?;
                auth
            } else {
                match load_codex_managed_auth() {
                    Ok(managed_auth) => {
                        let auth = managed_auth.current_auth()?;
                        store_codex_managed_auth(store, &managed_auth)?;
                        auth
                    }
                    Err(_) => {
                        let auth =
                            load_codex_auth().context("load external Codex auth for import")?;
                        store_codex_auth(store, &auth)?;
                        auth
                    }
                }
            };
            println!("Codex login: imported account {}", auth.account_id);
            Ok(())
        }
        AuthCommand::Logout { account } => {
            auth_logout(store, account)?;
            println!("{}: logged out", auth_account_label(account));
            Ok(())
        }
    }
}

fn env_any(names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

fn print_auth_line(label: &str, connected: bool) {
    let status = if connected {
        "connected"
    } else {
        "not connected"
    };
    println!("{label}: {status}");
}

fn auth_login(
    store: &Store,
    account: AuthAccount,
    api_key: Option<String>,
    access_token: Option<String>,
    account_id: Option<String>,
    code: Option<String>,
    no_browser: bool,
) -> Result<()> {
    match account {
        AuthAccount::BrowserUseCloud => {
            let api_key =
                read_required_secret(api_key, &format!("{} API key", auth_account_label(account)))?;
            let key = api_key_setting(account).context("account does not use an API key")?;
            store.set_setting(key, api_key.trim())?;
            store.set_setting("browser", "Browser Use Cloud")?;
            println!("{}: connected (stored)", auth_account_label(account));
            Ok(())
        }
        AuthAccount::Openai
        | AuthAccount::Anthropic
        | AuthAccount::Openrouter
        | AuthAccount::Deepseek => {
            let api_key =
                read_required_secret(api_key, &format!("{} API key", auth_account_label(account)))?;
            let key = api_key_setting(account).context("account does not use an API key")?;
            store.set_setting(key, api_key.trim())?;
            store.set_setting("account", auth_account_label(account))?;
            println!("{}: connected (stored)", auth_account_label(account));
            Ok(())
        }
        AuthAccount::Codex => {
            let auth = if access_token.is_some() || account_id.is_some() {
                CodexAuth {
                    access_token: access_token
                        .context("auth login codex requires --access-token with --account-id")?,
                    account_id: account_id
                        .context("auth login codex requires --account-id with --access-token")?,
                }
            } else {
                match load_codex_managed_auth() {
                    Ok(managed_auth) => {
                        let auth = managed_auth.current_auth()?;
                        store_codex_managed_auth(store, &managed_auth)?;
                        store.set_setting("account", "Codex login")?;
                        println!("Codex login: connected account {}", auth.account_id);
                        return Ok(());
                    }
                    Err(_) => load_codex_auth().context("load external Codex auth for login")?,
                }
            };
            store_codex_auth(store, &auth)?;
            store.set_setting("account", "Codex login")?;
            println!("Codex login: connected account {}", auth.account_id);
            Ok(())
        }
        AuthAccount::ClaudeCode => {
            let credential = claude_code_login(access_token, code, !no_browser)?;
            store_claude_code_oauth(store, &credential)?;
            store.set_setting("account", "Claude Code login")?;
            println!("Claude Code login: connected (stored OAuth credential)");
            Ok(())
        }
    }
}

fn auth_logout(store: &Store, account: AuthAccount) -> Result<()> {
    match account {
        AuthAccount::Codex => {
            store.delete_setting("auth.codex.access_token")?;
            store.delete_setting("auth.codex.account_id")?;
            store.delete_setting("auth.codex.id_token")?;
            store.delete_setting("auth.codex.refresh_token")?;
            store.delete_setting("auth.codex.source_path")?;
            store.delete_setting("auth.codex.last_refresh")?;
        }
        AuthAccount::Openai
        | AuthAccount::Anthropic
        | AuthAccount::Openrouter
        | AuthAccount::Deepseek => {
            if let Some(key) = api_key_setting(account) {
                store.delete_setting(key)?;
            }
        }
        AuthAccount::BrowserUseCloud => {
            store.delete_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?;
        }
        AuthAccount::ClaudeCode => {
            store.delete_setting("auth.claude_code.access_token")?;
            store.delete_setting("auth.claude_code.refresh_token")?;
            store.delete_setting("auth.claude_code.expires_ms")?;
            store.delete_setting("auth.claude_code.auth_token")?;
        }
    }
    Ok(())
}

fn read_required_secret(value: Option<String>, prompt: &str) -> Result<String> {
    if let Some(value) = value {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            bail!("{prompt} cannot be empty");
        }
        return Ok(trimmed);
    }
    eprint!("{prompt}: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        bail!("{prompt} cannot be empty");
    }
    Ok(trimmed)
}

fn claude_code_login(
    access_token: Option<String>,
    code: Option<String>,
    open_browser: bool,
) -> Result<ClaudeCodeOAuthCredential> {
    let (verifier, challenge) = claude_code_oauth_pkce();
    if let Some(access_token) = access_token {
        let access_token = access_token.trim().to_string();
        if access_token.is_empty() {
            bail!("Claude Code OAuth token cannot be empty");
        }
        return Ok(ClaudeCodeOAuthCredential {
            access_token,
            refresh_token: String::new(),
            expires_ms: 0,
        });
    }

    if let Some(input) = code {
        let parsed = parse_claude_code_authorization_input(&input);
        let auth_code = parsed
            .code
            .context("Claude Code authorization code was missing")?;
        let state = parsed.state.unwrap_or_else(|| verifier.clone());
        return exchange_claude_code_authorization_code(&auth_code, &state, &verifier);
    }

    let (tx, rx) = mpsc::channel();
    let _callback = start_claude_code_callback_server(verifier.clone(), tx)?;
    let url = claude_code_oauth_authorize_url(&verifier, &challenge);
    println!("Open this URL to login with Anthropic Claude Code:\n");
    println!("{url}");
    println!("\nWaiting for browser callback on http://localhost:{CLAUDE_CODE_CALLBACK_PORT}{CLAUDE_CODE_CALLBACK_PATH} ...");
    if open_browser {
        if let Err(error) = open::that(&url) {
            eprintln!("Could not open browser automatically: {error}");
        }
    }
    let parsed = rx
        .recv_timeout(Duration::from_secs(900))
        .context("timed out waiting for Anthropic browser callback")??;
    let auth_code = parsed
        .code
        .context("Claude Code authorization code was missing")?;
    let state = parsed.state.unwrap_or_default();
    if state != verifier {
        bail!("Claude Code OAuth state mismatch");
    }
    exchange_claude_code_authorization_code(&auth_code, &state, &verifier)
}

struct CallbackServerHandle {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CallbackServerHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_claude_code_callback_server(
    expected_state: String,
    sender: mpsc::Sender<Result<browser_use_providers::ClaudeCodeAuthorization>>,
) -> Result<CallbackServerHandle> {
    let listener = TcpListener::bind((CLAUDE_CODE_CALLBACK_HOST, CLAUDE_CODE_CALLBACK_PORT))
        .with_context(|| {
            format!(
                "bind Claude Code OAuth callback on {CLAUDE_CODE_CALLBACK_HOST}:{CLAUDE_CODE_CALLBACK_PORT}"
            )
        })?;
    listener
        .set_nonblocking(true)
        .context("configure Claude Code OAuth callback listener")?;
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            if stop_rx.try_recv().is_ok() || Instant::now() >= deadline {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let result = handle_claude_code_callback(&mut stream, &expected_state);
                    let _ = sender.send(result);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    let _ = sender.send(Err(error).context("accept Claude Code OAuth callback"));
                    break;
                }
            }
        }
    });
    Ok(CallbackServerHandle {
        stop: stop_tx,
        thread: Some(thread),
    })
}

fn handle_claude_code_callback(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<browser_use_providers::ClaudeCodeAuthorization> {
    let mut request = [0_u8; 4096];
    let read = stream
        .read(&mut request)
        .context("read Claude Code OAuth callback")?;
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("parse Claude Code OAuth callback request")?;
    let parsed = parse_claude_code_authorization_input(path);
    let status = if !path.starts_with(CLAUDE_CODE_CALLBACK_PATH) {
        404
    } else if parsed.code.is_none() || parsed.state.as_deref() != Some(expected_state) {
        400
    } else {
        200
    };
    let text = match status {
        200 => "Anthropic authentication completed. You can close this window.",
        400 => "Anthropic authentication failed: missing code or state mismatch.",
        _ => "Anthropic callback route not found.",
    };
    let body = format!("<html><body><p>{text}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
    if status == 200 {
        Ok(parsed)
    } else {
        bail!("{text}")
    }
}

fn store_codex_auth(store: &Store, auth: &CodexAuth) -> Result<()> {
    store.set_setting("auth.codex.access_token", auth.access_token.trim())?;
    store.set_setting("auth.codex.account_id", auth.account_id.trim())?;
    store.delete_setting("auth.codex.id_token")?;
    store.delete_setting("auth.codex.refresh_token")?;
    store.delete_setting("auth.codex.source_path")?;
    store.delete_setting("auth.codex.last_refresh")?;
    Ok(())
}

fn store_codex_managed_auth(store: &Store, auth: &CodexManagedAuth) -> Result<()> {
    let snapshot = auth.current_snapshot()?;
    store.set_setting("auth.codex.access_token", snapshot.access_token.trim())?;
    store.set_setting("auth.codex.account_id", snapshot.account_id.trim())?;
    if let Some(id_token) = snapshot
        .id_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        store.set_setting("auth.codex.id_token", id_token.trim())?;
    } else {
        store.delete_setting("auth.codex.id_token")?;
    }
    if let Some(refresh_token) = snapshot
        .refresh_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        store.set_setting("auth.codex.refresh_token", refresh_token.trim())?;
    } else {
        store.delete_setting("auth.codex.refresh_token")?;
    }
    if let Some(source_path) = snapshot.source_path.as_ref() {
        store.set_setting(
            "auth.codex.source_path",
            source_path.to_string_lossy().as_ref(),
        )?;
    } else {
        store.delete_setting("auth.codex.source_path")?;
    }
    if let Some(last_refresh) = snapshot.last_refresh {
        store.set_setting("auth.codex.last_refresh", &last_refresh.to_rfc3339())?;
    } else {
        store.delete_setting("auth.codex.last_refresh")?;
    }
    Ok(())
}

fn store_claude_code_oauth(store: &Store, credential: &ClaudeCodeOAuthCredential) -> Result<()> {
    store.set_setting(
        "auth.claude_code.access_token",
        credential.access_token.trim(),
    )?;
    if credential.refresh_token.trim().is_empty() {
        store.delete_setting("auth.claude_code.refresh_token")?;
    } else {
        store.set_setting(
            "auth.claude_code.refresh_token",
            credential.refresh_token.trim(),
        )?;
    }
    if credential.expires_ms > 0 {
        store.set_setting(
            "auth.claude_code.expires_ms",
            &credential.expires_ms.to_string(),
        )?;
    } else {
        store.delete_setting("auth.claude_code.expires_ms")?;
    }
    store.delete_setting("auth.claude_code.auth_token")?;
    Ok(())
}

fn print_api_key_status(
    store: &Store,
    label: &str,
    setting_key: &str,
    env_names: &[&str],
) -> Result<()> {
    if store
        .get_setting(setting_key)?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("{label}: connected (stored)");
    } else if env_any(env_names) {
        println!("{label}: connected (environment)");
    } else {
        print_auth_line(label, false);
    }
    Ok(())
}

fn print_codex_status(store: &Store) -> Result<()> {
    if let Some(auth) = stored_codex_auth(store)? {
        println!(
            "Codex login: connected account {} (stored)",
            auth.account_id
        );
        return Ok(());
    }
    if let Some(auth) = codex_auth_from_explicit_env() {
        println!(
            "Codex login: connected account {} (environment)",
            auth.account_id
        );
    } else {
        print_auth_line("Codex login", false);
    }
    Ok(())
}

fn print_claude_code_status(store: &Store) -> Result<()> {
    if store
        .get_setting("auth.claude_code.access_token")?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("Claude Code login: connected (stored OAuth credential)");
        return Ok(());
    }
    if store
        .get_setting("auth.claude_code.auth_token")?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("Claude Code login: connected (stored legacy OAuth token)");
        return Ok(());
    }
    if env_any(&[
        "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
        "ANTHROPIC_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
    ]) {
        println!("Claude Code login: connected (environment OAuth token)");
        return Ok(());
    }
    match claude_code_cli_status() {
        Ok(Some(summary)) => println!("Claude Code CLI: connected ({summary})"),
        Ok(None) => print_auth_line("Claude Code login", false),
        Err(error) => println!("Claude Code login: not connected ({error})"),
    }
    Ok(())
}

fn claude_code_cli_status() -> Result<Option<String>> {
    let output = std::process::Command::new("claude")
        .args(["auth", "status", "--json"])
        .output()
        .context("run `claude auth status --json`")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse Claude Code auth status")?;
    if value
        .get("loggedIn")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let email = value
            .get("email")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown email");
        let subscription = value
            .get("subscriptionType")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown plan");
        return Ok(Some(format!("{email}, {subscription}")));
    }
    Ok(None)
}

fn stored_codex_auth(store: &Store) -> Result<Option<CodexAuth>> {
    let Some(access_token) = store.get_setting("auth.codex.access_token")? else {
        return Ok(None);
    };
    let Some(account_id) = store.get_setting("auth.codex.account_id")? else {
        return Ok(None);
    };
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(CodexAuth::new(access_token, account_id)))
}

fn codex_auth_from_explicit_env() -> Option<CodexAuth> {
    if let Ok(path) = std::env::var("LLM_BROWSER_CODEX_AUTH_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return load_codex_auth_file(path).ok();
        }
    }
    let access_token = std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN").ok()?;
    let account_id = std::env::var("LLM_BROWSER_CODEX_ACCOUNT_ID").ok()?;
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return None;
    }
    Some(CodexAuth {
        access_token,
        account_id,
    })
}

fn api_key_setting(account: AuthAccount) -> Option<&'static str> {
    match account {
        AuthAccount::Openai => Some("auth.openai.api_key"),
        AuthAccount::Anthropic => Some("auth.anthropic.api_key"),
        AuthAccount::Openrouter => Some("auth.openrouter.api_key"),
        AuthAccount::Deepseek => Some("auth.deepseek.api_key"),
        AuthAccount::BrowserUseCloud => Some(BROWSER_USE_CLOUD_API_KEY_SETTING),
        AuthAccount::Codex | AuthAccount::ClaudeCode => None,
    }
}

fn auth_account_label(account: AuthAccount) -> &'static str {
    match account {
        AuthAccount::Codex => "Codex login",
        AuthAccount::ClaudeCode => "Claude Code login",
        AuthAccount::BrowserUseCloud => "Browser Use Cloud",
        AuthAccount::Openai => "OpenAI API key",
        AuthAccount::Anthropic => "Anthropic API key",
        AuthAccount::Openrouter => "OpenRouter API key",
        AuthAccount::Deepseek => "DeepSeek API key",
    }
}

fn diagnostics(store: &Store) -> Result<()> {
    let sessions = store.list_sessions()?;
    let event_count = sessions.iter().try_fold(0usize, |count, session| {
        Ok::<usize, anyhow::Error>(count + store.events_for_session(&session.id)?.len())
    })?;
    println!("state_dir: {}", store.state_dir().display());
    println!("database: {}", store.state_dir().join("state.db").display());
    println!("sessions: {}", sessions.len());
    println!("events: {event_count}");
    println!("settings: {}", store.list_settings()?.len());

    let mut worker = PythonWorker::start()?;
    let artifact_dir = store.state_dir().join("artifacts").join("__diagnostics__");
    let response = worker.run(
        "__diagnostics__",
        std::env::current_dir()?,
        artifact_dir,
        "result = {'browser_harness_available': browser_harness_available, 'browser_harness_error': browser_harness_error}",
    )?;
    println!(
        "browser_harness: {}",
        if response.browser_harness_available {
            "available"
        } else {
            "not available"
        }
    );
    if let Some(error) = response.browser_harness_error {
        if !error.trim().is_empty() {
            println!("browser_harness_error: {error}");
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

struct SdkServerContext {
    journal: Arc<MemoryJournal>,
    runtime: RuntimeHandle,
    store: SharedStore,
    _ephemeral_state_dir: Arc<tempfile::TempDir>,
}

impl SdkServerContext {
    fn memory() -> Result<Self> {
        let (runtime, journal) = BrowserUseRuntime::memory();
        let ephemeral_state_dir = Arc::new(tempfile::Builder::new().prefix("but-sdk-").tempdir()?);
        let store = Store::open_in_memory(ephemeral_state_dir.path())?;
        Ok(Self {
            journal,
            runtime: runtime.handle(),
            store: Arc::new(Mutex::new(store)),
            _ephemeral_state_dir: ephemeral_state_dir,
        })
    }

    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            journal: Arc::clone(&self.journal),
            runtime: self.runtime.clone(),
            store: Arc::clone(&self.store),
            _ephemeral_state_dir: Arc::clone(&self._ephemeral_state_dir),
        })
    }
}

fn sdk_server(transport: SdkTransportArg) -> Result<()> {
    match transport {
        SdkTransportArg::Stdio => sdk_server_stdio(),
    }
}

fn sdk_server_stdio() -> Result<()> {
    let context = SdkServerContext::memory()?;
    let (response_tx, response_rx) = mpsc::channel::<Value>();
    let event_thread_stop = Arc::new(AtomicBool::new(false));
    let writer = thread::Builder::new()
        .name("browser-use-sdk-stdio-writer".to_string())
        .spawn(move || -> Result<()> {
            let mut stdout = io::BufWriter::new(io::stdout().lock());
            for response in response_rx {
                writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
                stdout.flush()?;
            }
            Ok(())
        })
        .context("spawn sdk stdio writer")?;

    let event_thread = {
        let response_tx = response_tx.clone();
        let runtime = context.runtime.clone();
        let stop = Arc::clone(&event_thread_stop);
        thread::Builder::new()
            .name("browser-use-sdk-event-forwarder".to_string())
            .spawn(move || -> Result<()> {
                let mut rx = runtime.events().subscribe();
                let mut projection = RuntimeProjectionState::new(runtime.snapshot());
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .context("build sdk event runtime")?;
                rt.block_on(async move {
                    while !stop.load(Ordering::Relaxed) {
                        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                            Ok(Ok(event)) => {
                                let projected = projection.apply_event(&event);
                                let session_id =
                                    event.session_id.as_ref().map(|id| id.as_str().to_string());
                                let run_id = event
                                    .run_id
                                    .as_ref()
                                    .map(|id| id.as_str().to_string())
                                    .or_else(|| session_id.clone());
                                let agent_id =
                                    event.agent_id.as_ref().map(|id| id.as_str().to_string());
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "agent.event",
                                    "params": {
                                        "run_id": run_id.clone(),
                                        "session_id": session_id.clone(),
                                        "agent_id": agent_id.clone(),
                                        "event": event,
                                    },
                                });
                                if response_tx.send(notification).is_err() {
                                    break;
                                }
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "agent.projected_event",
                                    "params": {
                                        "run_id": run_id,
                                        "session_id": session_id,
                                        "agent_id": agent_id,
                                        "event": projected,
                                    },
                                });
                                if response_tx.send(notification).is_err() {
                                    break;
                                }
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                                continue;
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                            Err(_) => continue,
                        }
                    }
                });
                Ok(())
            })
            .context("spawn sdk event forwarder")?
    };

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response_tx = response_tx.clone();
        let context = match context.try_clone() {
            Ok(context) => context,
            Err(error) => {
                let _ = response_tx.send(json_rpc_error(
                    None,
                    -32000,
                    format!("SDK context clone failed: {error:#}"),
                ));
                continue;
            }
        };
        thread::Builder::new()
            .name("browser-use-sdk-request".to_string())
            .spawn(move || {
                let response = handle_sdk_json_rpc_line(&context, &line);
                let _ = response_tx.send(response);
            })
            .context("spawn sdk request handler")?;
    }
    event_thread_stop.store(true, Ordering::Relaxed);
    drop(response_tx);
    event_thread.join().unwrap_or_else(|panic| {
        Err(anyhow::anyhow!(
            "sdk event forwarder panicked: {}",
            panic_payload_message(panic)
        ))
    })?;
    writer.join().unwrap_or_else(|panic| {
        Err(anyhow::anyhow!(
            "sdk writer panicked: {}",
            panic_payload_message(panic)
        ))
    })?;
    Ok(())
}

fn handle_sdk_json_rpc_line(context: &SdkServerContext, line: &str) -> Value {
    match serde_json::from_str::<JsonRpcRequest>(line) {
        Ok(request) => handle_sdk_json_rpc_request(context, request),
        Err(error) => json_rpc_error(None, -32700, format!("Parse error: {error}")),
    }
}

fn handle_sdk_json_rpc_request(context: &SdkServerContext, request: JsonRpcRequest) -> Value {
    if request.jsonrpc.as_deref() != Some("2.0") {
        return json_rpc_error(request.id, -32600, "Invalid Request");
    }
    let id = request.id;
    let result = match request.method.as_str() {
        "runtime.ping" => Ok(serde_json::json!({ "ok": true })),
        "runtime.snapshot" => sdk_runtime_snapshot(&context.runtime),
        "browser.create" => sdk_browser_create(&context.runtime, &request.params),
        "browser.stop" | "browser.close" => sdk_browser_close(&context.runtime, &request.params),
        "agent.create" => sdk_agent_create(context, &request.params),
        "agent.snapshot" => sdk_agent_snapshot(context, &request.params),
        "agent.run" => sdk_agent_run(context, &request.params),
        "agent.stop" => sdk_agent_stop(context, &request.params),
        "agent.close" => sdk_agent_close(context, &request.params),
        _ => Err(anyhow::anyhow!("Method not found")),
    };
    match result {
        Ok(result) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        Err(error) if error.to_string() == "Method not found" => {
            json_rpc_error(id, -32601, "Method not found")
        }
        Err(error) => json_rpc_error(id, -32000, error.to_string()),
    }
}

fn sdk_runtime_snapshot(runtime: &RuntimeHandle) -> Result<Value> {
    Ok(serde_json::to_value(runtime.snapshot())?)
}

fn sdk_browser_create(runtime: &RuntimeHandle, params: &Value) -> Result<Value> {
    let config = BrowserConfig {
        keep_alive: params
            .get("keep_alive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        headless: params.get("headless").and_then(Value::as_bool),
        profile_id: params
            .get("profile_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    };
    let browser_id = runtime.create_browser(config);
    Ok(serde_json::json!({ "browser_id": browser_id.as_str() }))
}

fn sdk_browser_close(runtime: &RuntimeHandle, params: &Value) -> Result<Value> {
    let browser_id = BrowserId::from_string(
        params
            .get("browser_id")
            .and_then(Value::as_str)
            .context("browser.close requires string param `browser_id`")?
            .to_string(),
    )?;
    runtime.close_browser(&browser_id)?;
    Ok(serde_json::json!({ "ok": true }))
}

fn sdk_agent_create(context: &SdkServerContext, params: &Value) -> Result<Value> {
    let task = params
        .get("task")
        .and_then(Value::as_str)
        .context("agent.create requires string param `task`")?
        .to_string();
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let max_concurrent_threads_per_session = params
        .get("max_concurrent_threads_per_session")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(
        browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        );
    let agent = context.runtime.create_root_agent(CreateRootAgentRequest {
        cwd: cwd.clone(),
        task: task.clone(),
        max_concurrent_threads_per_session,
    })?;
    let runtime_session = context
        .runtime
        .load_session(agent.session_id())?
        .with_context(|| format!("runtime did not create session {}", agent.session_id()))?;
    let input_payload = typed_user_input_payload_from_text_for_cwd(&task, &cwd)?;
    {
        let store = context.store.lock().expect("sdk store mutex poisoned");
        if store.load_session(agent.session_id().as_str())?.is_none() {
            store.create_session_with_id_and_artifact_root(
                None,
                Path::new(&runtime_session.cwd),
                Path::new(&runtime_session.artifact_root),
                agent.session_id().as_str().to_string(),
            )?;
        }
        store.append_event(
            agent.session_id().as_str(),
            "session.input",
            input_payload.clone(),
        )?;
    }
    context.runtime.append_observed_session_event(
        agent.session_id().clone(),
        "session.input",
        input_payload,
        RuntimeDurability::Barrier,
    )?;
    Ok(serde_json::json!({
        "agent_id": agent.agent_id().as_str(),
        "session_id": agent.session_id().as_str(),
    }))
}

fn sdk_agent_snapshot(context: &SdkServerContext, params: &Value) -> Result<Value> {
    if let Some(agent_id) = params.get("agent_id").and_then(Value::as_str) {
        let agent_id = AgentId::from_string(agent_id.to_string())?;
        return Ok(serde_json::to_value(
            context.runtime.snapshot_agent(&agent_id)?,
        )?);
    }
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .context("agent.snapshot requires string param `agent_id` or `session_id`")?;
    let agent_id = context
        .runtime
        .agent_id_for_session(&SessionId::from_string(session_id.to_string())?)?;
    Ok(serde_json::to_value(
        context.runtime.snapshot_agent(&agent_id)?,
    )?)
}

fn sdk_agent_run(context: &SdkServerContext, params: &Value) -> Result<Value> {
    let agent_id = AgentId::from_string(
        params
            .get("agent_id")
            .and_then(Value::as_str)
            .context("agent.run requires string param `agent_id`")?
            .to_string(),
    )?;
    let thread = context.runtime.agents().thread(&agent_id)?;
    let session_id = thread.session_id().clone();
    let session = context
        .runtime
        .load_session(&session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;

    for followup in params
        .get("followups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        let payload = typed_user_input_payload_from_text_for_cwd(followup, &session.cwd)?;
        let submitted = context.runtime.submit_followup(SubmitInputRequest {
            target_agent_id: agent_id.clone(),
            content: followup.to_string(),
            trigger_turn: true,
            delivery_phase: RuntimeMailboxDeliveryPhase::CurrentTurn,
            input_items: payload.get("items").cloned(),
            payload: serde_json::json!({ "source": "sdk" }),
        })?;
        let append = context.runtime.append_observed_session_event(
            session_id.clone(),
            "session.followup.runtime_queued",
            serde_json::json!({
                "source": "sdk",
                "runtime_mailbox_id": submitted.mailbox_item.id,
                "runtime_mailbox_seq": submitted.mailbox_item.seq,
            }),
            RuntimeDurability::Barrier,
        )?;
        let _ = append;
    }

    let browser_id = params
        .get("browser_id")
        .and_then(Value::as_str)
        .map(|browser_id| BrowserId::from_string(browser_id.to_string()))
        .transpose()?;

    let events_before_run = context.runtime.events_for_session(&session_id)?;
    let task = task_from_events(&events_before_run).unwrap_or_else(|| "task".to_string());
    let config = sdk_provider_run_config(params, Some(&task))?;
    sdk_run_agent_with_runtime(context, &agent_id, &session_id, browser_id, config)?;

    let events = context.runtime.events_for_session(&session_id)?;
    let output = session_result_from_events(&events);
    let error = failure_from_events(&events);
    let final_projected_event = sdk_final_projected_event(
        context,
        &agent_id,
        &session_id,
        &events,
        output.as_deref(),
        error.as_deref(),
    )?;
    let event_values = events
        .iter()
        .map(|event| {
            serde_json::json!({
                "seq": event.seq,
                "id": event.id,
                "event_type": event.event_type,
                "payload": event.payload,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "history": {
            "output": output,
            "success": error.is_none(),
            "done": true,
            "errors": error.into_iter().collect::<Vec<_>>(),
            "events": event_values,
        },
        "final_projected_event": final_projected_event,
    }))
}

fn sdk_run_agent_with_runtime(
    context: &SdkServerContext,
    agent_id: &AgentId,
    session_id: &SessionId,
    browser_id: Option<BrowserId>,
    config: ProviderRunConfig,
) -> Result<()> {
    let runtime = context.runtime.clone();
    let driver_runtime = runtime.clone();
    let store = Arc::clone(&context.store);
    let session_id_for_driver = session_id.as_str().to_string();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_for_driver = cancel.clone();
    let run_cwd = {
        let store = context.store.lock().expect("sdk store mutex poisoned");
        store
            .load_session(session_id.as_str())?
            .map(|session| PathBuf::from(session.cwd))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
    };
    let provider_config = serde_json::json!({
        "backend": format!("{:?}", config.backend),
        "model": config.model.clone(),
        "source": "sdk-memory",
    });
    let mut request = RunAgentRequest::new(session_id.clone())
        .with_agent_id(agent_id.clone())
        .with_provider_config(provider_config)
        .with_cwd(run_cwd)
        .with_input_source("sdk-memory")
        .with_cancellation_token(cancel);
    if let Some(browser_id) = browser_id {
        request = request.with_browser_id(browser_id);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("build sdk run runtime")?;
    rt.block_on(async move {
        runtime
            .run_agent(request, async move {
                RuntimeTurnDriver::new(
                    store,
                    session_id_for_driver,
                    config,
                    cancel_for_driver,
                    driver_runtime,
                )
                .run()
                .await
                .map(|resolved| resolved.as_str().to_string())
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

fn sdk_final_projected_event(
    context: &SdkServerContext,
    agent_id: &AgentId,
    session_id: &SessionId,
    events: &[browser_use_protocol::EventRecord],
    output: Option<&str>,
    error: Option<&str>,
) -> Result<Value> {
    let mut snapshot = context.runtime.snapshot();
    if let Some(agent) = snapshot
        .agents
        .iter_mut()
        .find(|agent| &agent.agent_id == agent_id || &agent.session_id == session_id)
    {
        for event in events {
            match event.event_type.as_str() {
                "model.stream_delta" => {
                    if let Some(text) = event
                        .payload
                        .get("text")
                        .or_else(|| event.payload.get("delta"))
                        .and_then(Value::as_str)
                    {
                        agent.live.last_model_delta = Some(text.to_string());
                    }
                }
                "model.thinking_delta" => {
                    if let Some(text) = event
                        .payload
                        .get("text")
                        .or_else(|| event.payload.get("delta"))
                        .and_then(Value::as_str)
                    {
                        agent.live.last_model_thinking_delta = Some(text.to_string());
                    }
                }
                "token_count" => {
                    if let Some(info) = event.payload.get("info") {
                        agent.live.last_token_usage = info.get("last_token_usage").cloned();
                        agent.live.total_token_usage = info.get("total_token_usage").cloned();
                        agent.live.model_context_window =
                            info.get("model_context_window").and_then(Value::as_i64);
                    }
                }
                "session.done" => {
                    agent.live.final_result = event
                        .payload
                        .get("result")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    agent.live.failure = None;
                    agent.live.active_items.clear();
                }
                "session.failed" | "stream_error" | "model.turn.error" => {
                    agent.live.failure = event
                        .payload
                        .get("error")
                        .or_else(|| event.payload.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    agent.live.active_items.clear();
                }
                _ => {}
            }
        }
        if agent.live.final_result.is_none() {
            agent.live.final_result = output.map(str::to_string);
        }
        if agent.live.failure.is_none() {
            agent.live.failure = error.map(str::to_string);
        }
    }
    let source_event_id = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.event_type.as_str(),
                "agent.turn.completed"
                    | "agent.turn.aborted"
                    | "session.done"
                    | "session.failed"
                    | "session.cancelled"
            )
        })
        .and_then(|event| {
            event
                .payload
                .get("runtime_event_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            events.iter().rev().find_map(|event| {
                event
                    .payload
                    .get("runtime_event_id")
                    .and_then(Value::as_str)
            })
        })
        .unwrap_or_else(|| session_id.as_str());
    Ok(serde_json::json!({
        "source_event_id": source_event_id,
        "kind": if error.is_some() { "thread_status_changed" } else { "turn_completed" },
        "session_id": session_id.as_str(),
        "payload": {
            "runtime_owned": true,
            "source": "agent.run.final_projection",
            "success": error.is_none(),
            "result": output,
            "error": error,
        },
        "snapshot": snapshot,
    }))
}

fn sdk_agent_stop(context: &SdkServerContext, params: &Value) -> Result<Value> {
    let session_id = params
        .get("session_id")
        .or_else(|| params.get("agent_id"))
        .or_else(|| params.get("run_id"))
        .and_then(Value::as_str)
        .context("agent.stop requires `session_id`, `agent_id`, or `run_id`")?;
    let cancelled = context
        .runtime
        .cancel_run(&SessionId::from_string(session_id.to_string())?);
    Ok(serde_json::json!({ "cancelled": cancelled }))
}

fn sdk_agent_close(context: &SdkServerContext, params: &Value) -> Result<Value> {
    let agent_id = AgentId::from_string(
        params
            .get("agent_id")
            .and_then(Value::as_str)
            .context("agent.close requires string param `agent_id`")?
            .to_string(),
    )?;
    context
        .runtime
        .close_agent(browser_use_runtime::CloseAgentRequest {
            agent_id,
            reason: "sdk close".to_string(),
        })?;
    Ok(serde_json::json!({ "ok": true }))
}

fn sdk_provider_run_config(params: &Value, task: Option<&str>) -> Result<ProviderRunConfig> {
    let llm = params.get("llm").unwrap_or(&Value::Null);
    let provider = llm
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("openai");
    let model = llm
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("gpt-5.5");
    let backend = sdk_provider_backend(provider, model)?;
    let provider_id = sdk_provider_id(provider, backend);
    let mut options = AgentRunOptions::default()
        .with_browser_mode(
            params
                .get("browser_mode")
                .and_then(Value::as_str)
                .unwrap_or("local"),
        )
        .with_model_compaction(true)
        .with_analytics_source("sdk")
        .with_model_provider_id(provider_id.clone());
    options.analytics_provider_kind = Some(provider_id);
    options.analytics_model = Some(model.to_string());
    if let Some(max_steps) = params
        .get("max_steps")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    {
        options.max_turns = max_steps;
    }
    if let Some(schema) = params.get("output_schema").filter(|value| !value.is_null()) {
        options = options.with_final_output_json_schema(schema.clone(), true);
    }
    if let Some(timeout) = llm.get("timeout").and_then(Value::as_u64) {
        options.python_tool_timeout_seconds = timeout;
    }

    let mut config = ProviderRunConfig::new(backend, model).with_options(options);
    if backend == ProviderBackend::Fake {
        config = config.with_fake_result(fake_agent_result_text(task.unwrap_or("task"), None));
    }
    Ok(config)
}

fn sdk_provider_backend(provider: &str, model: &str) -> Result<ProviderBackend> {
    if model.eq_ignore_ascii_case("fake") {
        return Ok(ProviderBackend::Fake);
    }
    let normalized = provider.trim().to_ascii_lowercase();
    if normalized == "browser-use" || normalized == "browser_use" {
        return Ok(ProviderBackend::Openai);
    }
    ProviderBackend::from_provider_id(&normalized)
        .filter(|backend| *backend != ProviderBackend::None)
        .with_context(|| format!("unsupported SDK provider: {provider}"))
}

fn sdk_provider_id(provider: &str, backend: ProviderBackend) -> String {
    let normalized = provider.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "openai" | "anthropic" | "openrouter" | "deepseek" | "codex" | "fake"
    ) {
        return normalized;
    }
    default_provider_id_for_backend(backend).to_string()
}

fn json_rpc_error(id: Option<Value>, code: i64, message: impl Into<String>) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

fn trace(store: &Store, task_id: &str, output: Option<PathBuf>) -> Result<()> {
    let session = ensure_task_exists(store, task_id)?;
    let events = store.events_for_session(task_id)?;
    let artifacts = store.artifacts_for_session(task_id)?;
    let bundle = serde_json::json!({
        "session": session,
        "events": events,
        "artifacts": artifacts,
    });
    if let Some(output) = output {
        if output.extension().is_some() {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(
                &output,
                format!("{}\n", serde_json::to_string_pretty(&bundle)?),
            )
            .with_context(|| format!("write {}", output.display()))?;
            println!("{}", output.display());
        } else {
            std::fs::create_dir_all(&output)
                .with_context(|| format!("create {}", output.display()))?;
            let path = output.join("trace.json");
            std::fs::write(
                &path,
                format!("{}\n", serde_json::to_string_pretty(&bundle)?),
            )
            .with_context(|| format!("write {}", path.display()))?;
            println!("{}", path.display());
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    }
    Ok(())
}

/// Seed the child session's `environment_context` workspace-context event.
///
/// The legacy CLI called `append_workspace_context_event(store, &child)`, which
/// assembled a rich `<environment_context>` block (AGENTS.md / permissions /
/// collaboration-mode etc.). That assembly is a not-yet-ported engine seam; the
/// new engine seeds a minimal `<environment_context><cwd>…</cwd></…>` block on
/// run (`entrypoint::environment_context_content`). This mirrors that minimal
/// block synchronously at spawn time so the freshly-created child carries the
/// same workspace-context event the engine would (de-dup) re-emit on its run.
fn seed_environment_context_event(store: &Store, session_id: &str, cwd: &str) -> Result<()> {
    let content = format!("<environment_context>\n<cwd>{cwd}</cwd>\n</environment_context>");
    store.append_event(
        session_id,
        "workspace.context",
        serde_json::json!({
            "kind": "environment_context",
            "content": content,
        }),
    )?;
    Ok(())
}

fn seed_child_permissions_context_event(
    store: &Store,
    session_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(content) = child_request_developer_instructions(request) else {
        return Ok(());
    };
    store.append_event(
        session_id,
        "workspace.context",
        serde_json::json!({
            "kind": "permissions",
            "content": content,
        }),
    )?;
    Ok(())
}

fn child_request_developer_instructions(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "developer_instructions")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn spawn_agent(
    store: &Store,
    parent_id: &str,
    message: String,
    task_name: Option<String>,
    path: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
) -> Result<()> {
    ensure_task_exists(store, parent_id)?;
    if task_name.is_some() && path.is_some() {
        bail!("spawn-agent accepts either --task-name or --path, not both");
    }
    let requested_agent_path = match (task_name.as_deref(), path.as_deref()) {
        (Some(task_name), None) => {
            let parent_agent_path = display_agent_path_for_session(store, parent_id)?;
            Some(
                canonical_agent_path_from_task_name(task_name, &parent_agent_path)
                    .map_err(anyhow::Error::msg)?,
            )
        }
        (None, Some(path)) => Some(path.to_string()),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("checked above"),
    };
    let parent_events = store.events_for_session(parent_id)?;
    let inherited_context = sanitized_agent_context_from_events(&parent_events);
    let Some(runtime_child) = spawn_agent_via_live_runtime(
        store,
        parent_id,
        &message,
        task_name.as_deref(),
        requested_agent_path.as_deref(),
        nickname.as_deref(),
        role.as_deref(),
    )?
    else {
        bail!("spawn-agent requires a live runtime socket; Store-backed spawn is replay-only");
    };
    let child_session_id = runtime_child
        .get("session_id")
        .and_then(Value::as_str)
        .context("local runtime spawn_child response missing session_id")?;
    let agent_path = runtime_child
        .get("agent_path")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or(requested_agent_path);
    let child = store
        .load_session(child_session_id)?
        .with_context(|| format!("runtime did not create child session {child_session_id}"))?;
    store.append_event(
        &child.id,
        "agent.context",
        serde_json::json!({
            "from_session_id": parent_id,
            "fork_mode": "none",
            "history_mode": "compact_context",
            "agent_path": agent_path.clone(),
            "nickname": nickname.clone(),
            "role": role.clone(),
            "context": inherited_context,
        }),
    )?;
    seed_environment_context_event(store, &child.id, &child.cwd)?;
    store.append_event(
        &child.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&message, &child.cwd)?,
    )?;
    store.append_event(
        parent_id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id,
            "agent_path": agent_path,
            "nickname": nickname,
            "role": role,
        }),
    )?;
    println!("{}", child.id);
    Ok(())
}

fn spawn_agent_via_live_runtime(
    store: &Store,
    parent_id: &str,
    message: &str,
    task_name: Option<&str>,
    requested_agent_path: Option<&str>,
    nickname: Option<&str>,
    role: Option<&str>,
) -> Result<Option<Value>> {
    let child_id = browser_use_store::new_thread_id();
    let runtime_task_name = task_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            requested_agent_path
                .and_then(|path| path.rsplit('/').find(|segment| !segment.trim().is_empty()))
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "agent".to_string());
    let request = LocalRuntimeRequest::SpawnChild {
        parent_agent_id: parent_id.to_string(),
        child_agent_id: Some(child_id.clone()),
        child_session_id: Some(child_id),
        task_name: runtime_task_name,
        message: message.to_string(),
        nickname: nickname.map(ToOwned::to_owned),
        role: role.map(ToOwned::to_owned),
    };
    let Some(response) =
        send_local_runtime_request(store.state_dir(), &request, Duration::from_secs(5))?
    else {
        return Ok(None);
    };
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| "local runtime spawn_child failed".to_string());
        bail!(error);
    }
    let agent = response
        .result
        .get("agent")
        .cloned()
        .context("local runtime spawn_child response missing agent")?;
    Ok(Some(agent))
}

fn list_agents(
    store: &Store,
    parent_id: &str,
    path_prefix: Option<&str>,
    json_output: bool,
) -> Result<()> {
    ensure_task_exists(store, parent_id)?;
    let root_id = root_session_id(store, parent_id)?;
    let root = store
        .load_session(&root_id)?
        .with_context(|| format!("unknown root session id: {root_id}"))?;
    let path_prefix = path_prefix
        .map(|prefix| {
            display_agent_path_for_session(store, parent_id)
                .map(|current| canonical_agent_reference(prefix, &current))
        })
        .transpose()?;
    let mut agents = Vec::new();
    if path_prefix
        .as_deref()
        .is_none_or(|prefix| prefix == "/root" || "/root".starts_with(&format!("{prefix}/")))
    {
        agents.push(serde_json::json!({
            "agent_name": "/root",
            "agent_status": local_agent_status_value(store, &root, None)?,
            "last_task_message": "Main thread",
        }));
    }
    for agent in collect_agent_tree(store, &root_id)?
        .into_iter()
        .filter(|agent| agent.status != "closed")
    {
        let child = store
            .load_session(&agent.child_session_id)?
            .with_context(|| format!("unknown child session id: {}", agent.child_session_id))?;
        let agent_name = agent.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(store, &agent.child_session_id)
                .unwrap_or_else(|_| agent.child_session_id.clone())
        });
        if path_prefix.as_deref().is_some_and(|prefix| {
            agent_name != prefix && !agent_name.starts_with(&format!("{prefix}/"))
        }) {
            continue;
        }
        agents.push(serde_json::json!({
            "agent_name": agent_name,
            "agent_status": local_agent_status_value(store, &child, Some(&agent))?,
            "last_task_message": last_task_message_for_agent(store, &child.id)?,
        }));
    }
    agents.sort_by(|left, right| {
        left.get("agent_name")
            .and_then(Value::as_str)
            .cmp(&right.get("agent_name").and_then(Value::as_str))
    });
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "agents": agents }))?
        );
        return Ok(());
    }
    for agent in agents {
        let status = serde_json::to_string(&agent["agent_status"])?;
        let task = agent
            .get("last_task_message")
            .and_then(Value::as_str)
            .unwrap_or("");
        println!(
            "{:<32}  {:<24}  {}",
            agent["agent_name"].as_str().unwrap_or("-"),
            status,
            task
        );
    }
    Ok(())
}

fn close_agent(store: &Store, current_id: Option<&str>, target: &str, reason: &str) -> Result<()> {
    let child_id = resolve_close_agent_target(store, current_id, target)?;
    let child = store
        .load_session(&child_id)?
        .with_context(|| format!("unknown child session id: {child_id}"))?;
    if child.parent_id.is_none() {
        bail!("root is not a spawned agent");
    }
    let summary = store
        .agent_summary_for_child(&child_id)?
        .with_context(|| format!("unknown child agent edge for session id: {child_id}"))?;
    let previous_status = local_agent_status_value(store, &child, Some(&summary))?;
    if close_agent_via_live_runtime(store, &child_id, reason)? {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "previous_status": previous_status,
            }))?
        );
        return Ok(());
    }
    cleanup_agent_runtime_state_for_agent_subtree(store, &child_id, |session_id| {
        cleanup_unified_exec_manager_for_session_id(session_id)
    })?;
    store.close_child_agent(&child_id, reason)?;
    store.append_event(
        &summary.parent_session_id,
        "agent.cancelled",
        serde_json::json!({
            "child_session_id": child_id,
            "status": "cancelled",
            "payload": { "reason": reason },
        }),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "previous_status": previous_status,
        }))?
    );
    Ok(())
}

fn close_agent_via_live_runtime(store: &Store, child_id: &str, reason: &str) -> Result<bool> {
    let request = LocalRuntimeRequest::CloseAgent {
        agent_id: child_id.to_string(),
        reason: reason.to_string(),
    };
    let Some(response) =
        send_local_runtime_request(store.state_dir(), &request, Duration::from_secs(5))?
    else {
        return Ok(false);
    };
    if response.ok {
        return Ok(true);
    }
    let error = response
        .error
        .unwrap_or_else(|| "local runtime close_agent failed".to_string());
    if error.contains("unknown agent") {
        return Ok(false);
    }
    bail!(error);
}

fn resolve_close_agent_target(
    store: &Store,
    current_id: Option<&str>,
    target: &str,
) -> Result<String> {
    if let Some(current_id) = current_id {
        let resolved = resolve_agent_reference_in_tree(store, current_id, target)?
            .with_context(|| format!("live agent path `{target}` not found"))?;
        if resolved.is_root {
            bail!("root is not a spawned agent");
        }
        return Ok(resolved.session_id);
    }
    if !is_local_agent_id(target) {
        bail!("close-agent requires --current-id when target is not an agent id");
    }
    Ok(target.to_string())
}

fn resume_agent(store: &Store, child_id: &str) -> Result<()> {
    let child = store.load_session(child_id)?.with_context(|| {
        if is_local_agent_id(child_id) {
            format!("agent with id `{child_id}` not found")
        } else {
            format!("invalid agent id `{child_id}`")
        }
    })?;
    if !is_local_agent_id(child_id) {
        bail!("invalid agent id `{child_id}`");
    }
    if child.parent_id.is_none() {
        bail!("root is not a spawned agent");
    }
    let summary = store
        .agent_summary_for_child(child_id)?
        .with_context(|| format!("unknown child agent edge for session id: {child_id}"))?;
    let needs_reopen = matches!(
        child.status,
        browser_use_protocol::SessionStatus::Done
            | browser_use_protocol::SessionStatus::Failed
            | browser_use_protocol::SessionStatus::Cancelled
    ) || matches!(summary.status.as_str(), "closed" | "done" | "failed");
    if needs_reopen {
        store.reopen_child_agent_subtree(child_id)?;
    }
    let child = store
        .load_session(child_id)?
        .with_context(|| format!("unknown session id after resume: {child_id}"))?;
    let summary = store.agent_summary_for_child(child_id)?;
    let status = local_agent_status_value(store, &child, summary.as_ref())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": status,
        }))?
    );
    Ok(())
}

fn is_local_agent_id(value: &str) -> bool {
    browser_use_store::is_thread_id(value)
}

fn send_agent_message(
    store: &Store,
    author_id: &str,
    target_id: &str,
    message: &str,
    trigger_turn: bool,
) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        bail!("Empty message can't be sent to an agent");
    }
    let target = resolve_agent_reference_in_tree(store, author_id, target_id)?
        .with_context(|| format!("live agent path `{target_id}` not found"))?;
    if trigger_turn && target.is_root {
        bail!("Tasks can't be assigned to the root agent");
    }
    if let Some(message_id) =
        send_agent_message_via_live_runtime(store, author_id, &target, message, trigger_turn)?
    {
        println!("{message_id}");
        return Ok(());
    }
    bail!("send_agent_message requires a live runtime mailbox; Store-backed send is replay-only")
}

fn send_agent_message_via_live_runtime(
    store: &Store,
    author_id: &str,
    target: &ResolvedAgentReference,
    message: &str,
    trigger_turn: bool,
) -> Result<Option<String>> {
    let author_path = display_agent_path_for_session(store, author_id)?;
    let request = LocalRuntimeRequest::SendAgentMessage {
        author_agent_id: author_id.to_string(),
        target_agent_id: target.session_id.clone(),
        content: message.to_string(),
        trigger_turn,
        kind: if trigger_turn {
            RuntimeMailboxItemKind::Followup
        } else {
            RuntimeMailboxItemKind::Input
        },
        delivery_phase: RuntimeMailboxDeliveryPhase::NextTurn,
        payload: serde_json::json!({
            "source": "cli",
            "author_session_id": author_id,
            "target_session_id": target.session_id,
            "author_path": author_path,
            "target_path": target.agent_path,
        }),
    };
    let Some(response) =
        send_local_runtime_request(store.state_dir(), &request, Duration::from_secs(5))?
    else {
        return Ok(None);
    };
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| "local runtime send_agent_message failed".to_string());
        if error.contains("unknown agent") {
            return Ok(None);
        }
        bail!(error);
    }
    let message_id = response
        .result
        .get("mailbox_item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .context("local runtime send_agent_message response missing mailbox item id")?;
    Ok(Some(message_id))
}

fn wait_agent(store: &Store, target_id: &str, targets: Vec<String>, timeout_ms: u64) -> Result<()> {
    let session = ensure_task_exists(store, target_id)?;
    if !targets.is_empty() {
        if let Some(invalid) = targets.iter().find(|target| !is_local_agent_id(target)) {
            bail!("invalid agent id `{invalid}`");
        }
        if targets.len() > 1 {
            bail!("runtime-backed wait-agent accepts at most one target; omit targets to wait for any child");
        }
        if wait_agent_via_live_runtime(
            store,
            &session.id,
            Some(LocalRuntimeWaitTarget::AgentId(targets[0].clone())),
            timeout_ms,
        )? {
            return Ok(());
        }
        bail!("wait-agent requires a live runtime socket; Store-backed wait is replay-only");
    }
    if wait_agent_via_live_runtime(
        store,
        &session.id,
        Some(LocalRuntimeWaitTarget::Any),
        timeout_ms,
    )? {
        return Ok(());
    }
    bail!("wait-agent requires a live runtime socket; Store-backed wait is replay-only")
}

fn wait_agent_via_live_runtime(
    store: &Store,
    session_id: &str,
    target: Option<LocalRuntimeWaitTarget>,
    timeout_ms: u64,
) -> Result<bool> {
    let request = LocalRuntimeRequest::WaitAgent {
        parent_agent_id: session_id.to_string(),
        target,
        timeout_ms,
    };
    let Some(response) = send_local_runtime_request(
        store.state_dir(),
        &request,
        Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(5)),
    )?
    else {
        return Ok(false);
    };
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| "local runtime wait_agent failed".to_string());
        if error.contains("unknown agent") {
            return Ok(false);
        }
        bail!(error);
    }
    let timed_out = response
        .result
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = if timed_out {
        "Wait timed out."
    } else {
        "Wait completed."
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "message": message,
            "timed_out": timed_out,
            "mailbox_item": response.result.get("mailbox_item").cloned().unwrap_or(Value::Null),
        }))?
    );
    Ok(true)
}

fn dataset_list() -> Result<()> {
    let mut datasets = vec![
        serde_json::json!({
            "name": "real_v14_short",
            "path": "datasets/real_v14_short.json",
            "description": "10-task current smoke dataset",
        }),
        serde_json::json!({
            "name": "real_v14",
            "path": "datasets/real_v14_short.json",
            "description": "alias for real_v14_short in this repository",
        }),
        serde_json::json!({
            "name": "real_v8",
            "path": "datasets/real_v8.json",
            "description": "100-task baseline dataset",
        }),
    ];
    let dir = PathBuf::from("datasets");
    if dir.exists() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if datasets
                .iter()
                .any(|item| item.get("name").and_then(Value::as_str) == Some(name))
            {
                continue;
            }
            datasets.push(serde_json::json!({
                "name": name,
                "path": path.display().to_string(),
            }));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "datasets": datasets }))?
    );
    Ok(())
}

fn dataset_sample(dataset: &str, count: usize, task_ids: Vec<String>, all: bool) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    let selected = select_dataset_cases(
        cases,
        &DatasetRunOptions {
            count,
            task_ids,
            all,
            run_id: None,
            resume: false,
            skip_failed: false,
            stop_on_failure: false,
            max_attempts: 1,
            concurrency: 1,
            browser_mode: None,
        },
    )?;
    let sample = selected
        .iter()
        .map(dataset_case_manifest)
        .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}

fn dataset_report(store: &Store, run_id_or_path: &str) -> Result<()> {
    let manifest = load_dataset_manifest(store, run_id_or_path)?;
    let mut summary = summarize_dataset_manifest(&manifest);
    summary["artifact_salvage"] = dataset_artifact_salvage_report(store, &manifest)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn dataset_run_fake(store: &Store, dataset: &str, options: DatasetRunOptions) -> Result<()> {
    let browser_mode = dataset_browser_mode(&options);
    let run_config = ProviderRunConfig::new(ProviderBackend::Fake, "fake")
        .with_fake_result("Fake dataset case completed.");
    dataset_run_provider(
        store,
        dataset,
        options,
        ConfigDatasetRunner { config: run_config },
        DatasetProviderConfig {
            provider: "fake".to_string(),
            model: "fake".to_string(),
            browser_mode,
            max_turns: 80,
            python_timeout_seconds: 120,
        },
    )
}

fn create_dataset_session(
    store: &Store,
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
) -> Result<(String, DatasetTaskPaths)> {
    let paths = dataset_task_paths(store, run_id, case, attempt);
    create_dataset_task_dirs(&paths)?;
    let session = store.create_session_with_artifact_root(None, &paths.cwd, &paths.artifacts)?;
    let prompt = build_dataset_prompt(case);
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": prompt }),
    )?;
    store.append_event(
        &session.id,
        "dataset.case",
        serde_json::json!({
            "dataset": case.dataset,
            "path": case.path,
            "task_id": case.task_id,
            "attempt": attempt,
            "workspace": paths.cwd.display().to_string(),
            "task_root": paths.root.display().to_string(),
            "agent_workspace": paths.agent_workspace.display().to_string(),
            "runtime": paths.runtime.display().to_string(),
        }),
    )?;
    Ok((session.id, paths))
}

fn dataset_run_openai(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: Option<String>,
    max_turns: usize,
    python_timeout_seconds: u64,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Openai,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config_overrides = parse_cli_config_overrides(raw_config_overrides)?;
    let provider_id = resolved_cli_provider_id_for_backend_with_overrides(
        ProviderBackend::Openai,
        config_profile,
        &config_overrides,
    )?;
    let provider_id_source = cli_provider_id_source(&config_overrides);
    let browser_mode = dataset_browser_mode(&options);
    let mut agent_options = cli_agent_options(
        config_profile,
        raw_config_overrides,
        CollaborationModeKind::Default,
        runtime_options,
    )?;
    agent_options = if provider_id_source == RunConfigValueSource::Explicit {
        agent_options.with_model_provider_id(provider_id.clone())
    } else {
        agent_options.with_default_model_provider_id(provider_id.clone())
    };
    let run_config = ProviderRunConfig::new(ProviderBackend::Openai, model.clone())
        .with_model_source(model_source)
        .with_options(agent_options);
    dataset_run_provider(
        store,
        dataset,
        options,
        ConfigDatasetRunner { config: run_config },
        DatasetProviderConfig {
            provider: provider_id,
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_codex(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let browser_mode = dataset_browser_mode(&options);
    let run_config = ProviderRunConfig::new(ProviderBackend::Codex, model.clone()).with_options(
        cli_agent_options(
            config_profile,
            raw_config_overrides,
            CollaborationModeKind::Default,
            runtime_options,
        )?
        .with_default_model_provider_id("codex"),
    );
    dataset_run_provider(
        store,
        dataset,
        options,
        ConfigDatasetRunner { config: run_config },
        DatasetProviderConfig {
            provider: "codex".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_anthropic(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let browser_mode = dataset_browser_mode(&options);
    let run_config = ProviderRunConfig::new(ProviderBackend::Anthropic, model.clone())
        .with_options(
            cli_agent_options(None, &[], CollaborationModeKind::Default, runtime_options)?
                .with_default_model_provider_id("anthropic"),
        );
    dataset_run_provider(
        store,
        dataset,
        options,
        ConfigDatasetRunner { config: run_config },
        DatasetProviderConfig {
            provider: "anthropic".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_openrouter(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
    runtime_options: &CliRuntimeOptions,
) -> Result<()> {
    let browser_mode = dataset_browser_mode(&options);
    let run_config = ProviderRunConfig::new(ProviderBackend::Openrouter, model.clone())
        .with_options(
            cli_agent_options(None, &[], CollaborationModeKind::Default, runtime_options)?
                .with_default_model_provider_id("openrouter"),
        );
    dataset_run_provider(
        store,
        dataset,
        options,
        ConfigDatasetRunner { config: run_config },
        DatasetProviderConfig {
            provider: "openrouter".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_provider<R>(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    runner: R,
    config: DatasetProviderConfig,
) -> Result<()>
where
    R: DatasetRunner,
{
    let all_cases = load_dataset_cases(dataset)?;
    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| dataset_run_id(dataset));
    let manifest_path = dataset_manifest_path(store, &run_id);
    let resume_manifest = options.resume && manifest_path.exists();
    let selected = if resume_manifest {
        cases_from_manifest_selection(&all_cases, &load_dataset_manifest(store, &run_id)?)?
    } else {
        select_dataset_cases(all_cases, &options)?
    };
    if selected.is_empty() {
        println!("No dataset cases selected.");
        return Ok(());
    }

    let mut manifest = if resume_manifest {
        load_dataset_manifest(store, &run_id)?
    } else {
        new_dataset_manifest(&run_id, dataset, &selected, &options, &config)
    };
    let skip_ids = if options.resume {
        resume_skip_ids(&manifest, options.skip_failed)
    } else {
        HashSet::new()
    };
    write_dataset_manifest(store, &run_id, &manifest)?;

    let selected = selected
        .into_iter()
        .filter(|case| {
            if skip_ids.contains(&case.task_id) {
                println!("{}  skipped", case.task_id);
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    let mut pending = VecDeque::from(selected);
    let mut active = 0_usize;
    let mut stop_launching = false;
    let concurrency = options.concurrency.max(1);
    let max_attempts = options.max_attempts.max(1);
    let state_dir = store.state_dir().to_path_buf();
    let (tx, rx) = mpsc::channel::<(String, Result<Value>)>();

    while active > 0 || (!pending.is_empty() && !stop_launching) {
        while active < concurrency && !pending.is_empty() && !stop_launching {
            let case = pending.pop_front().expect("pending checked");
            let task_id = case.task_id.clone();
            let run_id = run_id.clone();
            let config = config.clone();
            let runner = runner.clone();
            let state_dir = state_dir.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Value> {
                        let store = Store::open(&state_dir)?;
                        run_dataset_case_with_attempts(
                            &store,
                            &runner,
                            &run_id,
                            &case,
                            config,
                            max_attempts,
                        )
                    }))
                    .unwrap_or_else(|panic| {
                        Err(anyhow::anyhow!(
                            "dataset worker panicked: {}",
                            panic_payload_message(panic)
                        ))
                    });
                let _ = tx.send((task_id, result));
            });
            active += 1;
        }
        if active == 0 {
            break;
        }
        let (task_id, result) = rx.recv().context("dataset worker channel closed")?;
        active -= 1;
        let result = match result {
            Ok(result) => result,
            Err(error) => serde_json::json!({
                "task_id": task_id,
                "ok": false,
                "error_type": "runner",
                "error": format!("{error:#}"),
            }),
        };
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        manifest_sessions_mut(&mut manifest)?.push(result);
        manifest["summary"] = summarize_dataset_manifest(&manifest);
        write_dataset_manifest(store, &run_id, &manifest)?;
        if options.stop_on_failure && !ok {
            stop_launching = true;
            pending.clear();
        }
    }

    for case in pending {
        if skip_ids.contains(&case.task_id) {
            println!("{}  skipped", case.task_id);
            continue;
        }
        println!("{}  pending", case.task_id);
    }

    manifest["summary"] = summarize_dataset_manifest(&manifest);
    write_dataset_manifest(store, &run_id, &manifest)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    if !dataset_manifest_exit_ok(&manifest) {
        bail!("dataset run has failures or pending tasks");
    }
    Ok(())
}

fn run_dataset_case_with_attempts<R: DatasetRunner>(
    store: &Store,
    runner: &R,
    run_id: &str,
    case: &DatasetCase,
    config: DatasetProviderConfig,
    max_attempts: usize,
) -> Result<Value> {
    let mut retry_history = Vec::new();
    for attempt in 1..=max_attempts {
        let mut result =
            run_dataset_case_with_provider(store, runner, run_id, case, config.clone(), attempt)?;
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok {
            if !retry_history.is_empty() {
                result["retry_history"] = Value::Array(retry_history);
            }
            return Ok(result);
        }
        let should_retry = attempt < max_attempts && is_transient_provider_failure(&result);
        if !should_retry {
            if is_permanent_provider_failure(&result) {
                result["retry_classification"] = Value::String("permanent".to_string());
            } else if attempt < max_attempts {
                result["retry_classification"] = Value::String("not_transient".to_string());
            }
            if !retry_history.is_empty() {
                result["retry_history"] = Value::Array(retry_history);
            }
            return Ok(result);
        }
        retry_history.push(result);
    }
    bail!("unreachable dataset retry loop")
}

fn run_dataset_case_with_provider<R: DatasetRunner>(
    store: &Store,
    runner: &R,
    run_id: &str,
    case: &DatasetCase,
    config: DatasetProviderConfig,
    attempt: usize,
) -> Result<Value> {
    let (session_id, paths) = create_dataset_session(store, run_id, case, attempt)?;
    println!("{}  {}", case.task_id, session_id);
    io::stdout().flush()?;
    let agent_options = AgentRunOptions {
        max_turns: config.max_turns,
        max_context_chars: AgentRunOptions::default().max_context_chars,
        browser_mode: Some(config.browser_mode.clone()),
        dynamic_browser_mode_from_store: AgentRunOptions::default().dynamic_browser_mode_from_store,
        collaboration_mode: AgentRunOptions::default().collaboration_mode,
        include_environment_context: true,
        include_permissions_instructions: true,
        environment_context_environments: Vec::new(),
        environment_context_network: None,
        config_profile: None,
        config_overrides: Vec::new(),
        session_thread_config: None,
        base_instructions: None,
        developer_instructions: None,
        compact_prompt: None,
        model_provider_id: Some(config.provider.clone()),
        model_provider_id_source: RunConfigValueSource::Explicit,
        python_tool_timeout_seconds: config.python_timeout_seconds,
        python_env: dataset_python_env(run_id, case, attempt, &paths, &config),
        child_agent_runner: None,
        final_output_json_schema: None,
        final_output_json_schema_strict: true,
        model_compaction_enabled: true,
        model_auto_compact_token_limit: None,
        model_auto_compact_token_limit_scope: AgentRunOptions::default()
            .model_auto_compact_token_limit_scope,
        analytics_source: Some("cli".to_string()),
        analytics_provider_kind: Some(config.provider.clone()),
        analytics_model: Some(config.model.clone()),
        // Provider-level runtime options are merged by ConfigDatasetRunner; this
        // per-case layer carries dataset-specific browser/python limits.
        mcp_servers: std::collections::HashMap::new(),
        approval_policy: AgentRunOptions::default().approval_policy,
        use_guardian: AgentRunOptions::default().use_guardian,
        multi_agent_v2: AgentRunOptions::default().multi_agent_v2,
        collab_enabled: AgentRunOptions::default().collab_enabled,
        agent_roles: AgentRunOptions::default().agent_roles,
    };
    let run_error = runner
        .run_dataset_session(store, &session_id, agent_options)
        .err()
        .map(|error| format!("{error:#}"));
    let result = dataset_attempt_result(store, case, &session_id, config, attempt, run_error)?;
    spawn_dataset_browser_cleanup(store, &session_id);
    Ok(result)
}

#[cfg(test)]
fn cleanup_dataset_browser_session(store: &Store, session_id: &str) -> Result<usize> {
    let removed_sessions = browser_use_browser::cleanup_session(session_id);
    store.append_event(
        session_id,
        "browser.cleaned_up",
        serde_json::json!({
            "source": "dataset-run",
            "removed_sessions": removed_sessions,
        }),
    )?;
    Ok(removed_sessions)
}

fn spawn_dataset_browser_cleanup(store: &Store, session_id: &str) {
    let state_dir = store.state_dir().to_path_buf();
    let session_id = session_id.to_string();
    thread::spawn(move || {
        let (done_tx, done_rx) = mpsc::channel();
        let cleanup_state_dir = state_dir.clone();
        let cleanup_session_id = session_id.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                browser_use_browser::cleanup_session(&cleanup_session_id)
            }));
            if let Ok(cleanup_store) = Store::open(&cleanup_state_dir) {
                match cleanup_result {
                    Ok(removed_sessions) => {
                        let _ = cleanup_store.append_event(
                            &cleanup_session_id,
                            "browser.cleaned_up",
                            serde_json::json!({
                                "source": "dataset-run",
                                "removed_sessions": removed_sessions,
                                "duration_ms": started.elapsed().as_millis() as u64,
                                "async": true,
                            }),
                        );
                    }
                    Err(panic) => {
                        let _ = cleanup_store.append_event(
                            &cleanup_session_id,
                            "browser.cleanup_failed",
                            serde_json::json!({
                                "source": "dataset-run",
                                "error": format!("cleanup panicked: {}", panic_payload_message(panic)),
                                "duration_ms": started.elapsed().as_millis() as u64,
                                "async": true,
                            }),
                        );
                    }
                }
            }
            let _ = done_tx.send(());
        });
        if done_rx
            .recv_timeout(DATASET_BROWSER_CLEANUP_TIMEOUT)
            .is_err()
        {
            if let Ok(timeout_store) = Store::open(&state_dir) {
                let _ = timeout_store.append_event(
                    &session_id,
                    "browser.cleanup_timed_out",
                    serde_json::json!({
                        "source": "dataset-run",
                        "timeout_ms": DATASET_BROWSER_CLEANUP_TIMEOUT.as_millis() as u64,
                        "async": true,
                    }),
                );
            }
        }
    });
}

fn dataset_attempt_result(
    store: &Store,
    case: &DatasetCase,
    session_id: &str,
    config: DatasetProviderConfig,
    attempt: usize,
    run_error: Option<String>,
) -> Result<Value> {
    let session = ensure_task_exists(store, session_id)?;
    let events = store.events_for_session(session_id)?;
    let final_result = session_result_from_events(&events);
    let final_result_chars = final_result.as_deref().map(str::len).unwrap_or(0);
    let usage = usage_summary_from_events(&events);
    let session_failure = failure_from_events(&events);
    let artifacts =
        dataset_artifacts_for_paths(Path::new(&session.cwd), Path::new(&session.artifact_root))?;
    let error = run_error.clone().or(session_failure.clone());
    let error_type = if run_error.is_some() {
        Value::String("provider".to_string())
    } else if session_failure.is_some() {
        Value::String("session".to_string())
    } else {
        Value::Null
    };
    let ok = run_error.is_none()
        && session.status.as_str() == "done"
        && error.is_none()
        && final_result.is_some();
    Ok(serde_json::json!({
        "task_id": case.task_id,
        "dataset": case.dataset,
        "path": case.path,
        "ok": ok,
        "attempt_number": attempt,
        "provider": config.provider,
        "model": config.model,
        "usage": usage,
        "final_result": final_result,
        "final_result_chars": final_result_chars,
        "artifacts": artifacts,
        "error_type": error_type,
        "error": error,
        "session": {
            "id": session.id,
            "status": session.status.as_str(),
            "cwd": session.cwd,
            "artifact_root": session.artifact_root,
        },
    }))
}

fn load_dataset_cases(dataset: &str) -> Result<Vec<DatasetCase>> {
    let path = resolve_dataset_path(dataset);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let raw: Value =
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
    let array = raw
        .as_array()
        .with_context(|| format!("{} must contain a JSON array", path.display()))?;
    let dataset_name = dataset_name_for_path(dataset, &path);
    array
        .iter()
        .enumerate()
        .map(|(idx, value)| parse_dataset_case(&dataset_name, &path, idx, value.clone()))
        .collect()
}

fn resolve_dataset_path(dataset: &str) -> PathBuf {
    let direct = PathBuf::from(dataset);
    if direct.exists() {
        return direct;
    }
    match dataset {
        "real_v14" | "real_v14_short" => return PathBuf::from("datasets/real_v14_short.json"),
        "real_v8" => return PathBuf::from("datasets/real_v8.json"),
        _ => {}
    }
    let with_ext = PathBuf::from("datasets").join(format!("{dataset}.json"));
    if with_ext.exists() {
        return with_ext;
    }
    direct
}

fn parse_dataset_case(
    dataset: &str,
    path: &std::path::Path,
    idx: usize,
    raw: Value,
) -> Result<DatasetCase> {
    raw.as_object()
        .with_context(|| format!("dataset row {} must be an object", idx + 1))?;
    let task_id = raw
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| (idx + 1).to_string());
    let confirmed_task = ["confirmed_task", "task", "text", "prompt"]
        .iter()
        .find_map(|key| raw.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .with_context(|| format!("dataset row {task_id} has no task text"))?;
    Ok(DatasetCase {
        dataset: dataset.to_string(),
        path: path.display().to_string(),
        task_id,
        confirmed_task,
        raw,
    })
}

fn dataset_name_for_path(dataset: &str, path: &std::path::Path) -> String {
    match dataset {
        "real_v14" | "real_v14_short" => "real_v14_short".to_string(),
        "real_v8" => "real_v8".to_string(),
        _ => path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(dataset)
            .to_string(),
    }
}

fn select_dataset_cases(
    cases: Vec<DatasetCase>,
    options: &DatasetRunOptions,
) -> Result<Vec<DatasetCase>> {
    if !options.task_ids.is_empty() {
        let requested = options
            .task_ids
            .iter()
            .cloned()
            .collect::<HashSet<String>>();
        let selected = cases
            .into_iter()
            .filter(|case| requested.contains(&case.task_id))
            .collect::<Vec<_>>();
        let found = selected
            .iter()
            .map(|case| case.task_id.clone())
            .collect::<HashSet<_>>();
        let missing = requested
            .difference(&found)
            .cloned()
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            bail!("dataset task id(s) not found: {}", missing.join(", "));
        }
        return Ok(selected);
    }
    if options.all {
        return Ok(cases);
    }
    Ok(cases.into_iter().take(options.count).collect())
}

fn cases_from_manifest_selection(
    cases: &[DatasetCase],
    manifest: &Value,
) -> Result<Vec<DatasetCase>> {
    let empty = Vec::new();
    let ids = manifest
        .get("selection")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter_map(|case| case.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        bail!("resume manifest has no selection");
    }
    let by_id = cases
        .iter()
        .map(|case| (case.task_id.clone(), case.clone()))
        .collect::<HashMap<_, _>>();
    ids.into_iter()
        .map(|id| {
            by_id
                .get(&id)
                .cloned()
                .with_context(|| format!("resume task id {id} no longer exists in dataset"))
        })
        .collect()
}

fn build_dataset_prompt(case: &DatasetCase) -> String {
    include_str!("../../../prompts/dataset-case-user.md")
        .trim()
        .replace("{{dataset}}", &case.dataset)
        .replace("{{task_id}}", &case.task_id)
        .replace("{{task}}", &case.confirmed_task)
}

fn dataset_case_manifest(case: &DatasetCase) -> Value {
    serde_json::json!({
        "dataset": case.dataset,
        "path": case.path,
        "task_id": case.task_id,
        "confirmed_task": case.confirmed_task,
        "raw": case.raw,
    })
}

fn new_dataset_manifest(
    run_id: &str,
    dataset: &str,
    cases: &[DatasetCase],
    options: &DatasetRunOptions,
    config: &DatasetProviderConfig,
) -> Value {
    let mut datasets = HashMap::<String, usize>::new();
    for case in cases {
        *datasets.entry(case.dataset.clone()).or_default() += 1;
    }
    serde_json::json!({
        "run_id": run_id,
        "dataset": dataset,
        "created_ms": now_ms(),
        "provider": config.provider,
        "model": config.model,
        "concurrency": options.concurrency.max(1),
        "max_attempts": options.max_attempts.max(1),
        "max_turns": config.max_turns,
        "python_timeout_seconds": config.python_timeout_seconds,
        "headless": config.browser_mode != "cloud",
        "browser": config.browser_mode,
        "selection": cases.iter().map(dataset_case_manifest).collect::<Vec<_>>(),
        "summary": {
            "count": cases.len(),
            "datasets": datasets,
            "passed": 0,
            "failed": 0,
            "pending": cases.len(),
            "usage": empty_usage_summary(),
        },
        "sessions": [],
    })
}

fn dataset_run_id(dataset: &str) -> String {
    let mut safe = dataset
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        safe.push_str("dataset");
    }
    format!("{safe}-{}", now_ms())
}

fn dataset_manifest_path(store: &Store, run_id: &str) -> PathBuf {
    store
        .state_dir()
        .join("dataset-runs")
        .join(format!("{run_id}.json"))
}

fn dataset_run_files_path(store: &Store, run_id: &str) -> PathBuf {
    store
        .state_dir()
        .join("dataset-run-files")
        .join(safe_path_segment(run_id))
}

fn dataset_task_paths(
    store: &Store,
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
) -> DatasetTaskPaths {
    let root = dataset_run_files_path(store, run_id).join(format!(
        "task-{}-attempt-{}",
        safe_path_segment(&case.task_id),
        attempt
    ));
    let runtime_base = PathBuf::from("/tmp")
        .join("lbe")
        .join(stable_short_hash(run_id, 12))
        .join(format!(
            "t{}a{}",
            stable_short_hash(&case.task_id, 10),
            attempt
        ));
    DatasetTaskPaths {
        cwd: root.join("cwd"),
        artifacts: root.join("artifacts"),
        agent_workspace: root.join("agent-workspace"),
        logs: root.join("logs"),
        runtime: runtime_base.join("r"),
        tmp: root.join("tmp"),
        root,
    }
}

fn create_dataset_task_dirs(paths: &DatasetTaskPaths) -> Result<()> {
    for path in [
        &paths.root,
        &paths.cwd,
        &paths.artifacts,
        &paths.agent_workspace,
        &paths.logs,
        &paths.runtime,
        &paths.tmp,
    ] {
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    }
    let helper = paths.agent_workspace.join("agent_helpers.py");
    if !helper.exists() {
        std::fs::write(&helper, "").with_context(|| format!("write {}", helper.display()))?;
    }
    Ok(())
}

fn dataset_python_env(
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
    paths: &DatasetTaskPaths,
    config: &DatasetProviderConfig,
) -> Vec<(String, String)> {
    let task_name = format!(
        "bu{}{}a{}",
        stable_short_hash(run_id, 8),
        stable_short_hash(&case.task_id, 8),
        attempt
    );
    let mut env = vec![
        ("BU_NAME".to_string(), task_name),
        (
            "BH_RUNTIME_DIR".to_string(),
            paths.runtime.display().to_string(),
        ),
        ("BH_TMP_DIR".to_string(), paths.tmp.display().to_string()),
        (
            "BH_AGENT_WORKSPACE".to_string(),
            paths.agent_workspace.display().to_string(),
        ),
        (
            "LLM_BROWSER_BROWSER_MODE".to_string(),
            config.browser_mode.clone(),
        ),
        (
            "LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW".to_string(),
            "0".to_string(),
        ),
    ];
    if config.browser_mode == "cloud" {
        env.push(("LLM_BROWSER_AUTO_CHROME".to_string(), "0".to_string()));
        env.push(("BU_CDP_URL".to_string(), "".to_string()));
        env.push(("BU_CDP_WS".to_string(), "".to_string()));
        env.push(("BU_BROWSER_ID".to_string(), "".to_string()));
    }
    env
}

fn safe_path_segment(value: &str) -> String {
    let mut safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if safe.is_empty() {
        safe.push_str("case");
    }
    safe
}

fn stable_short_hash(value: &str, len: usize) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let hex = format!("{hash:016x}");
    hex.chars().take(len.min(hex.len())).collect()
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

fn load_dataset_manifest(store: &Store, run_id_or_path: &str) -> Result<Value> {
    let direct = PathBuf::from(run_id_or_path);
    let path = if direct.exists() {
        direct
    } else {
        dataset_manifest_path(store, run_id_or_path)
    };
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn write_dataset_manifest(store: &Store, run_id: &str, manifest: &Value) -> Result<()> {
    let path = dataset_manifest_path(store, run_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(manifest)?),
    )
    .with_context(|| format!("write {}", path.display()))
}

fn manifest_sessions_mut(manifest: &mut Value) -> Result<&mut Vec<Value>> {
    if !manifest.get("sessions").is_some_and(Value::is_array) {
        manifest["sessions"] = Value::Array(Vec::new());
    }
    manifest
        .get_mut("sessions")
        .and_then(Value::as_array_mut)
        .context("manifest sessions must be an array")
}

fn summarize_dataset_manifest(manifest: &Value) -> Value {
    let selection = manifest
        .get("selection")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected_ids = selection
        .iter()
        .filter_map(|case| case.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut datasets = HashMap::<String, usize>::new();
    for case in &selection {
        if let Some(dataset) = case.get("dataset").and_then(Value::as_str) {
            *datasets.entry(dataset.to_string()).or_default() += 1;
        }
    }
    let mut latest = HashMap::<String, Value>::new();
    let mut attempts = HashMap::<String, usize>::new();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(task_id) = session.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        *attempts.entry(task_id.to_string()).or_default() += 1;
        latest.insert(task_id.to_string(), session.clone());
    }
    let mut passed_ids = Vec::new();
    let mut failed_ids = Vec::new();
    let mut pending_ids = Vec::new();
    for task_id in &selected_ids {
        match latest.get(task_id) {
            Some(session) if session.get("ok").and_then(Value::as_bool).unwrap_or(false) => {
                passed_ids.push(task_id.clone());
            }
            Some(_) => failed_ids.push(task_id.clone()),
            None => pending_ids.push(task_id.clone()),
        }
    }
    serde_json::json!({
        "run_id": manifest.get("run_id").cloned().unwrap_or(Value::Null),
        "dataset": manifest.get("dataset").cloned().unwrap_or(Value::Null),
        "provider": manifest.get("provider").cloned().unwrap_or(Value::Null),
        "model": manifest.get("model").cloned().unwrap_or(Value::Null),
        "count": selected_ids.len(),
        "datasets": datasets,
        "passed": passed_ids.len(),
        "failed": failed_ids.len(),
        "pending": pending_ids.len(),
        "passed_ids": passed_ids,
        "failed_ids": failed_ids,
        "pending_ids": pending_ids,
        "attempts_by_task": attempts,
        "usage": usage_summary_from_manifest(manifest),
    })
}

fn dataset_artifact_salvage_report(store: &Store, manifest: &Value) -> Result<Value> {
    let summary = summarize_dataset_manifest(manifest);
    let sessions = manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let sessions_with_artifacts = sessions
        .iter()
        .filter(|session| {
            session
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("found"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    let failed_with_artifacts = sessions
        .iter()
        .filter(|session| !session.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .filter(|session| {
            session
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("found"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|session| session.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();

    let run_id = manifest.get("run_id").and_then(Value::as_str);
    let mut pending_with_artifacts = Vec::new();
    if let Some(run_id) = run_id {
        for task_id in summary
            .get("pending_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let attempts = pending_artifact_attempts(store, run_id, task_id)?;
            if !attempts.is_empty() {
                pending_with_artifacts.push(serde_json::json!({
                    "task_id": task_id,
                    "attempts": attempts,
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "note": "Artifact presence is reported for manual review only; it does not mark a task successful.",
        "sessions_with_artifacts": sessions_with_artifacts,
        "failed_with_artifacts": failed_with_artifacts,
        "pending_with_artifacts": pending_with_artifacts,
    }))
}

fn pending_artifact_attempts(store: &Store, run_id: &str, task_id: &str) -> Result<Vec<Value>> {
    let base = dataset_run_files_path(store, run_id);
    if !base.exists() {
        return Ok(Vec::new());
    }
    let prefix = format!("task-{}-attempt-", safe_path_segment(task_id));
    let mut attempts = Vec::new();
    for entry in std::fs::read_dir(&base).with_context(|| format!("read {}", base.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(attempt) = name.strip_prefix(&prefix) else {
            continue;
        };
        let artifacts = dataset_artifacts_for_paths(&path.join("cwd"), &path.join("artifacts"))?;
        if artifacts
            .get("found")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            attempts.push(serde_json::json!({
                "attempt": attempt,
                "task_root": path.display().to_string(),
                "artifacts": artifacts,
            }));
        }
    }
    attempts.sort_by(|left, right| {
        left.get("attempt")
            .and_then(Value::as_str)
            .cmp(&right.get("attempt").and_then(Value::as_str))
    });
    Ok(attempts)
}

fn dataset_artifacts_for_paths(cwd: &Path, artifact_root: &Path) -> Result<Value> {
    let final_answer = artifact_root.join(".final_answer.json");

    let final_answer_summary = if final_answer.exists() {
        summarize_artifact_file(&final_answer)
    } else {
        Value::Null
    };
    let output_summaries = summarize_output_dir(cwd)?;
    let found = !final_answer_summary.is_null() || !output_summaries.is_empty();

    Ok(serde_json::json!({
        "found": found,
        "final_answer": final_answer_summary,
        "outputs": output_summaries,
    }))
}

fn summarize_output_dir(outputs: &Path) -> Result<Vec<Value>> {
    if !outputs.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(outputs).with_context(|| format!("read {}", outputs.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files
        .into_iter()
        .take(20)
        .map(|path| summarize_artifact_file(&path))
        .collect())
}

fn summarize_artifact_file(path: &Path) -> Value {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return serde_json::json!({
                "path": path.display().to_string(),
                "error": format!("{error:#}"),
            });
        }
    };
    let mut summary = serde_json::json!({
        "path": path.display().to_string(),
        "bytes": metadata.len(),
    });
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "json" && metadata.len() <= 10_000_000 {
        match std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|content| {
                serde_json::from_str::<Value>(&content).map_err(anyhow::Error::from)
            }) {
            Ok(value) => {
                summary["json"] = summarize_json_artifact(&value);
            }
            Err(error) => {
                summary["json_error"] = Value::String(format!("{error:#}"));
            }
        }
    } else if extension == "csv" {
        summary["kind"] = Value::String("csv".to_string());
    } else {
        summary["kind"] = Value::String(if extension.is_empty() {
            "file".to_string()
        } else {
            extension
        });
    }
    summary
}

fn summarize_json_artifact(value: &Value) -> Value {
    match value {
        Value::Array(items) => serde_json::json!({
            "kind": "array",
            "length": items.len(),
        }),
        Value::Object(object) => {
            let keys = object.keys().take(20).cloned().collect::<Vec<_>>();
            let array_lengths = object
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .as_array()
                        .map(|items| (key.clone(), Value::from(items.len())))
                })
                .collect::<serde_json::Map<_, _>>();
            serde_json::json!({
                "kind": "object",
                "keys": keys,
                "array_lengths": array_lengths,
            })
        }
        _ => serde_json::json!({
            "kind": "scalar",
        }),
    }
}

fn usage_summary_from_events(events: &[browser_use_protocol::EventRecord]) -> Value {
    let mut input_tokens = 0_i64;
    let mut input_cached_tokens = 0_i64;
    let mut input_cache_creation_tokens = 0_i64;
    let mut output_tokens = 0_i64;
    let mut reasoning_output_tokens = 0_i64;
    let mut total_tokens = 0_i64;
    let mut input_cost_usd = 0.0_f64;
    let mut input_cached_cost_usd = 0.0_f64;
    let mut input_cache_creation_cost_usd = 0.0_f64;
    let mut output_cost_usd = 0.0_f64;
    let mut cost_usd = 0.0_f64;
    let mut invocation_count = 0_i64;
    let mut cost_known_invocation_count = 0_i64;
    let mut cost_estimated_invocation_count = 0_i64;
    let mut cost_missing_invocation_count = 0_i64;

    for event in events {
        if event.event_type != "model.usage" {
            continue;
        }
        invocation_count += 1;
        if event
            .payload
            .get("cost_usd")
            .and_then(Value::as_f64)
            .is_some()
        {
            cost_known_invocation_count += 1;
            if event.payload.get("cost_source").and_then(Value::as_str) == Some("estimated") {
                cost_estimated_invocation_count += 1;
            }
        } else {
            cost_missing_invocation_count += 1;
        }
        input_tokens += json_i64(&event.payload, "input_tokens");
        input_cached_tokens += json_i64(&event.payload, "input_cached_tokens");
        input_cache_creation_tokens += json_i64(&event.payload, "input_cache_creation_tokens");
        output_tokens += json_i64(&event.payload, "output_tokens");
        reasoning_output_tokens += json_i64(&event.payload, "reasoning_output_tokens");
        total_tokens += json_i64(&event.payload, "total_tokens");
        input_cost_usd += json_f64(&event.payload, "input_cost_usd");
        input_cached_cost_usd += json_f64(&event.payload, "input_cached_cost_usd");
        input_cache_creation_cost_usd += json_f64(&event.payload, "input_cache_creation_cost_usd");
        output_cost_usd += json_f64(&event.payload, "output_cost_usd");
        cost_usd += json_f64(&event.payload, "cost_usd");
    }

    serde_json::json!({
        "input_tokens": input_tokens,
        "input_cached_tokens": input_cached_tokens,
        "input_cache_creation_tokens": input_cache_creation_tokens,
        "output_tokens": output_tokens,
        "reasoning_output_tokens": reasoning_output_tokens,
        "total_tokens": total_tokens,
        "input_cost_usd": input_cost_usd,
        "input_cached_cost_usd": input_cached_cost_usd,
        "input_cache_creation_cost_usd": input_cache_creation_cost_usd,
        "output_cost_usd": output_cost_usd,
        "cost_usd": cost_usd,
        "cost_known_invocation_count": cost_known_invocation_count,
        "cost_estimated_invocation_count": cost_estimated_invocation_count,
        "cost_missing_invocation_count": cost_missing_invocation_count,
        "cost_status": usage_cost_status(invocation_count, cost_known_invocation_count, cost_missing_invocation_count),
        "invocation_count": invocation_count,
    })
}

fn usage_summary_from_manifest(manifest: &Value) -> Value {
    let mut summary = empty_usage_summary();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let usage = session.get("usage").unwrap_or(&Value::Null);
        for key in [
            "input_tokens",
            "input_cached_tokens",
            "input_cache_creation_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "total_tokens",
            "invocation_count",
        ] {
            summary[key] = Value::from(json_i64(&summary, key) + json_i64(usage, key));
        }
        for key in [
            "input_cost_usd",
            "input_cached_cost_usd",
            "input_cache_creation_cost_usd",
            "output_cost_usd",
            "cost_usd",
        ] {
            summary[key] = Value::from(json_f64(&summary, key) + json_f64(usage, key));
        }
        for key in [
            "cost_known_invocation_count",
            "cost_estimated_invocation_count",
            "cost_missing_invocation_count",
        ] {
            summary[key] = Value::from(json_i64(&summary, key) + json_i64(usage, key));
        }
    }
    let invocation_count = json_i64(&summary, "invocation_count");
    let known_count = json_i64(&summary, "cost_known_invocation_count");
    let missing_count = json_i64(&summary, "cost_missing_invocation_count");
    summary["cost_status"] = Value::String(usage_cost_status(
        invocation_count,
        known_count,
        missing_count,
    ));
    summary
}

fn empty_usage_summary() -> Value {
    serde_json::json!({
        "input_tokens": 0,
        "input_cached_tokens": 0,
        "input_cache_creation_tokens": 0,
        "output_tokens": 0,
        "reasoning_output_tokens": 0,
        "total_tokens": 0,
        "input_cost_usd": 0.0,
        "input_cached_cost_usd": 0.0,
        "input_cache_creation_cost_usd": 0.0,
        "output_cost_usd": 0.0,
        "cost_usd": 0.0,
        "cost_known_invocation_count": 0,
        "cost_estimated_invocation_count": 0,
        "cost_missing_invocation_count": 0,
        "cost_status": "missing",
        "invocation_count": 0,
    })
}

fn usage_cost_status(invocation_count: i64, known_count: i64, missing_count: i64) -> String {
    if invocation_count <= 0 {
        return "missing".to_string();
    }
    if known_count == invocation_count {
        return "known".to_string();
    }
    if missing_count == invocation_count {
        return "missing".to_string();
    }
    "partial".to_string()
}

fn json_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn json_f64(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn resume_skip_ids(manifest: &Value, skip_failed: bool) -> HashSet<String> {
    let mut skip = HashSet::new();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(task_id) = session.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        let ok = session.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok || skip_failed {
            skip.insert(task_id.to_string());
        }
    }
    skip
}

fn dataset_manifest_exit_ok(manifest: &Value) -> bool {
    let summary = summarize_dataset_manifest(manifest);
    summary.get("failed").and_then(Value::as_u64).unwrap_or(1) == 0
        && summary.get("pending").and_then(Value::as_u64).unwrap_or(1) == 0
}

fn is_transient_provider_failure(result: &Value) -> bool {
    let Some(error) = result.get("error").and_then(Value::as_str) else {
        return false;
    };
    let error = error.to_ascii_lowercase();
    if is_permanent_provider_error(&error) {
        return false;
    }
    if [
        "incorrect api key",
        "401 unauthorized",
        "403 forbidden",
        "400 bad request",
        "content was flagged",
        "cybersecurity risk",
        "invalid_request_error",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return false;
    }
    [
        "read codex sse line",
        "stream error",
        "stream disconnected",
        "connection reset",
        "connection closed",
        "connection aborted",
        "operation timed out",
        "rate limit",
        "too many requests",
        "overloaded",
        "temporarily",
        "timeout",
        "timed out",
        "eof",
        "gateway",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn is_permanent_provider_failure(result: &Value) -> bool {
    result
        .get("error")
        .and_then(Value::as_str)
        .map(is_permanent_provider_error)
        .unwrap_or(false)
}

fn is_permanent_provider_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "no endpoints found that support image input",
        "context length exceeded",
        "maximum context length",
        "context_length_exceeded",
        "tool schema",
        "schema mismatch",
        "invalid_request_error",
        "incorrect api key",
        "401 unauthorized",
        "403 forbidden",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn ensure_task_exists(store: &Store, task_id: &str) -> Result<browser_use_protocol::SessionMeta> {
    store
        .load_session(task_id)?
        .with_context(|| format!("unknown task id: {task_id}"))
}

fn notify_parent_agent_done(store: &Store, task: &browser_use_protocol::SessionMeta) -> Result<()> {
    let Some(parent_id) = task.parent_id.as_deref() else {
        return Ok(());
    };
    update_parent_from_child_run(store, parent_id, &task.id, None, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_config_overrides_parse_toml_and_raw_strings_like_codex() -> Result<()> {
        let parsed = parse_cli_config_overrides(&[
            "project_doc_max_bytes=7".to_string(),
            "project_doc_fallback_filenames=[\"SESSION.md\"]".to_string(),
            "model=gpt-5.5".to_string(),
            "use_legacy_landlock=true".to_string(),
        ])?;

        assert_eq!(parsed[0].0, "project_doc_max_bytes");
        assert_eq!(parsed[0].1.as_integer(), Some(7));
        assert_eq!(parsed[1].0, "project_doc_fallback_filenames");
        assert_eq!(
            parsed[1].1.as_array().and_then(|items| items[0].as_str()),
            Some("SESSION.md")
        );
        assert_eq!(parsed[2].0, "model");
        assert_eq!(parsed[2].1.as_str(), Some("gpt-5.5"));
        assert_eq!(parsed[3].0, "features.use_legacy_landlock");
        assert_eq!(parsed[3].1.as_bool(), Some(true));
        Ok(())
    }

    #[test]
    fn cli_config_overrides_reject_missing_separator() {
        let error = parse_cli_config_overrides(&["model".to_string()])
            .expect_err("missing equals should fail");

        assert!(error.to_string().contains("missing '='"));
    }

    #[test]
    fn cli_collaboration_mode_defaults_to_core_default() -> Result<()> {
        let options = cli_agent_options(
            None,
            &[],
            CollaborationModeArg::Default.into(),
            &CliRuntimeOptions::default(),
        )?;

        assert_eq!(options.collaboration_mode, CollaborationModeKind::Default);
        Ok(())
    }

    #[test]
    fn cli_agent_options_apply_runtime_policy_guardian_and_mcp() -> Result<()> {
        let temp = unique_cli_test_dir("runtime-options")?;
        let mcp_config = temp.join("mcp.toml");
        std::fs::write(
            &mcp_config,
            r#"
[mcp_servers.local]
transport = "stdio"
command = "test-mcp"
"#,
        )?;
        let runtime_options = CliRuntimeOptions {
            approval_policy: Some(AskForApproval::UnlessTrusted),
            use_guardian: Some(true),
            mcp_config_paths: vec![mcp_config],
        };

        let options =
            cli_agent_options(None, &[], CollaborationModeKind::Default, &runtime_options)?;

        assert_eq!(options.approval_policy, AskForApproval::UnlessTrusted);
        assert!(options.use_guardian);
        assert!(options.mcp_servers.contains_key("local"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_completion_handler_skips_interrupted_child_runs() {
        let interrupted_events = vec![browser_use_protocol::EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "child".to_string(),
            ts_ms: 0,
            event_type: "session.cancelled".to_string(),
            payload: serde_json::json!({"reason": "interrupt requested"}),
        }];
        assert!(
            cli_child_completion_from_background(true, None, Some(&interrupted_events)).is_none()
        );

        let cancelled_events = vec![browser_use_protocol::EventRecord {
            seq: 1,
            id: "cancelled".to_string(),
            session_id: "child".to_string(),
            ts_ms: 0,
            event_type: "session.cancelled".to_string(),
            payload: serde_json::json!({"reason": "cancelled by user", "runtime_owned": true}),
        }];
        assert!(
            cli_child_completion_from_background(true, None, Some(&cancelled_events)).is_none(),
            "clean cancellation unwind must not notify the parent as child success"
        );

        let done_events = vec![browser_use_protocol::EventRecord {
            seq: 1,
            id: "e2".to_string(),
            session_id: "child".to_string(),
            ts_ms: 0,
            event_type: "session.done".to_string(),
            payload: serde_json::json!({"result": "finished"}),
        }];
        let completion = cli_child_completion_from_background(true, None, Some(&done_events))
            .expect("non-interrupted run should notify");
        assert!(completion.success);
        assert_eq!(completion.summary.as_deref(), Some("finished"));

        let resumed_done_events = vec![
            browser_use_protocol::EventRecord {
                seq: 1,
                id: "e3".to_string(),
                session_id: "child".to_string(),
                ts_ms: 0,
                event_type: "session.cancelled".to_string(),
                payload: serde_json::json!({"reason": "interrupted by send_input"}),
            },
            browser_use_protocol::EventRecord {
                seq: 2,
                id: "e4".to_string(),
                session_id: "child".to_string(),
                ts_ms: 1,
                event_type: "session.followup".to_string(),
                payload: serde_json::json!({"text": "continue"}),
            },
            browser_use_protocol::EventRecord {
                seq: 3,
                id: "e5".to_string(),
                session_id: "child".to_string(),
                ts_ms: 2,
                event_type: "session.done".to_string(),
                payload: serde_json::json!({"result": "finished after resume"}),
            },
        ];
        let completion =
            cli_child_completion_from_background(true, None, Some(&resumed_done_events))
                .expect("resumed completion should clear earlier interruption");
        assert!(completion.success);
        assert_eq!(completion.summary.as_deref(), Some("finished after resume"));

        let completion = cli_child_completion_from_background(
            false,
            Some("boom".to_string()),
            Some(&interrupted_events),
        )
        .expect("run errors should still notify failure");
        assert!(!completion.success);
        assert!(completion.summary.unwrap_or_default().contains("boom"));
    }

    #[test]
    fn cli_child_runner_request_creates_runtime_backed_store_child_session() -> Result<()> {
        let temp = unique_cli_test_dir("child-runner-session")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("cwd");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &cwd)?;
        let runtime = cli_runtime_handle(&store)?;
        ensure_cli_agent_attached(
            &runtime,
            &store,
            &parent.id,
            browser_use_agent::config_overrides::DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )?;
        let request = ChildAgentRunRequest {
            parent_session_id: parent.id.clone(),
            child_session_id: "00000000abcd".to_string(),
            run_id: Some("run-1".to_string()),
            message: "Investigate the failing case".to_string(),
            input_items: None,
            input_is_inter_agent_communication: false,
            agent_path: Some("/root/investigate_1".to_string()),
            nickname: Some("Analyst".to_string()),
            role: Some("explorer".to_string()),
            fork_turns: Some("all".to_string()),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("high".to_string()),
            service_tier: Some("priority".to_string()),
            config_overrides: vec![(
                "model_provider".to_string(),
                toml::Value::String("anthropic".to_string()),
            )],
            completion_handler: None,
        };

        let child = create_agent_child_session_from_request(&runtime, &store, &request)?;
        record_child_run_marker_from_request(&store, &child.id, &request)?;

        assert_eq!(child.id, "00000000abcd");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert!(runtime
            .agents()
            .thread(&AgentId::from_string(child.id.clone())?)
            .is_ok());
        let child_events = store.events_for_session(&child.id)?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"));
        let marker = child_events
            .iter()
            .find(|event| event.event_type == "agent.run.started")
            .expect("run marker");
        assert_eq!(marker.payload["model"], "gpt-test");
        assert_eq!(marker.payload["reasoning_effort"], "high");
        assert_eq!(marker.payload["service_tier"], "priority");
        let model_provider_override = marker.payload["config_overrides"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("key").and_then(serde_json::Value::as_str) == Some("model_provider")
                })
            })
            .expect("model_provider override");
        assert_eq!(model_provider_override["value"], "anthropic");
        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"
                && event.payload["child_session_id"] == child.id));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn run_codex_session_command_accepts_task_id_and_model() -> Result<()> {
        let parsed = Args::try_parse_from([
            "browser-use-terminal",
            "run-codex-session",
            "session-123",
            "--model",
            "gpt-test",
        ])?;

        match &parsed.command {
            Command::RunCodexSession { task_id, model } => {
                assert_eq!(task_id, "session-123");
                assert_eq!(model, "gpt-test");
                assert_eq!(command_name(&parsed.command), "run_codex_session");
            }
            other => panic!("expected run-codex-session command, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn sdk_json_rpc_ping_and_create_methods_use_runtime() -> Result<()> {
        let temp = unique_cli_test_dir("sdk-json-rpc")?;
        let context = SdkServerContext::memory()?;

        let ping = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":1,"method":"runtime.ping","params":{}}"#,
        );
        assert_eq!(ping["result"]["ok"], true);
        let snapshot = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":10,"method":"runtime.snapshot","params":{}}"#,
        );
        assert_eq!(
            snapshot["result"]["agents"].as_array().map(Vec::len),
            Some(0)
        );

        let browser = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":2,"method":"browser.create","params":{"headless":true,"keep_alive":true}}"#,
        );
        let browser_id = browser["result"]["browser_id"]
            .as_str()
            .context("browser id")?;
        assert_eq!(context.runtime.browsers().snapshots().len(), 1);

        let close = handle_sdk_json_rpc_line(
            &context,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "browser.close",
                "params": { "browser_id": browser_id }
            })
            .to_string(),
        );
        assert_eq!(close["result"]["ok"], true);
        assert!(context.runtime.browsers().snapshots().is_empty());

        let agent = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":3,"method":"agent.create","params":{"task":"inspect","cwd":"/tmp"}}"#,
        );
        assert!(agent["result"]["agent_id"].as_str().is_some());
        assert_eq!(context.runtime.snapshot().agents.len(), 1);
        let agent_id = agent["result"]["agent_id"].as_str().context("agent id")?;
        let session_id = agent["result"]["session_id"]
            .as_str()
            .context("session id")?;
        let agent_snapshot = handle_sdk_json_rpc_line(
            &context,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "agent.snapshot",
                "params": { "agent_id": agent_id }
            })
            .to_string(),
        );
        assert_eq!(agent_snapshot["result"]["agent_id"], agent_id);
        let session_snapshot = handle_sdk_json_rpc_line(
            &context,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "agent.snapshot",
                "params": { "session_id": session_id }
            })
            .to_string(),
        );
        assert_eq!(session_snapshot["result"]["session_id"], session_id);
        let events = context
            .runtime
            .events_for_session(&SessionId::from_string(session_id.to_string())?)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.input"));
        assert!(
            !temp.join("state.db").exists(),
            "SDK memory context must not create a SQLite database"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn sdk_json_rpc_reports_protocol_errors() -> Result<()> {
        let temp = unique_cli_test_dir("sdk-json-rpc-errors")?;
        let context = SdkServerContext::memory()?;

        let parse = handle_sdk_json_rpc_line(&context, "{not-json");
        assert_eq!(parse["error"]["code"], -32700);

        let missing = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":4,"method":"missing.method","params":{}}"#,
        );
        assert_eq!(missing["error"]["code"], -32601);
        assert!(
            !temp.join("state.db").exists(),
            "SDK memory context must not create a SQLite database"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn sdk_json_rpc_agent_run_executes_fake_backend() -> Result<()> {
        let temp = unique_cli_test_dir("sdk-json-rpc-run-fake")?;
        let context = SdkServerContext::memory()?;
        let agent = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":1,"method":"agent.create","params":{"task":"inspect","cwd":"/tmp"}}"#,
        );
        let browser = handle_sdk_json_rpc_line(
            &context,
            r#"{"jsonrpc":"2.0","id":20,"method":"browser.create","params":{"headless":true}}"#,
        );
        let browser_id = browser["result"]["browser_id"]
            .as_str()
            .context("browser id")?;
        let agent_id = agent["result"]["agent_id"].as_str().context("agent id")?;
        let session_id = agent["result"]
            .get("session_id")
            .and_then(Value::as_str)
            .context("session id")?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "agent.run",
            "params": {
                "agent_id": agent_id,
                "browser_id": browser_id,
                "max_steps": 2,
                "llm": {"provider": "fake", "model": "fake"},
                "followups": ["extract title next"]
            }
        });

        let result = handle_sdk_json_rpc_line(&context, &serde_json::to_string(&request)?);

        assert_eq!(result["result"]["history"]["success"], true);
        assert_eq!(
            result["result"]["history"]["output"],
            serde_json::Value::String("Fake result for: inspect".to_string())
        );
        assert_eq!(
            result["result"]["final_projected_event"]["kind"],
            serde_json::Value::String("turn_completed".to_string())
        );
        assert_eq!(
            result["result"]["final_projected_event"]["snapshot"]["agents"][0]["live"]
                ["final_result"],
            serde_json::Value::String("Fake result for: inspect".to_string())
        );
        let events = context
            .runtime
            .events_for_session(&SessionId::from_string(session_id.to_string())?)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "mailbox.enqueued"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "mailbox.delivered"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "mailbox.consumed"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.followup.runtime_queued"));
        assert!(
            !temp.join("state.db").exists(),
            "SDK memory runs must not create Store-backed agent_messages or SQLite state"
        );
        let browser_snapshot = context
            .runtime
            .browsers()
            .snapshot(&BrowserId::from_string(browser_id.to_string())?)?;
        assert_eq!(browser_snapshot.active_agent_id, None);
        assert_eq!(
            browser_snapshot.status,
            browser_use_runtime::BrowserStatus::Released
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn sync_cookies_command_accepts_local_profile_without_global_profile_conflict() -> Result<()> {
        let parsed = Args::try_parse_from([
            "browser-use-terminal",
            "sync-cookies",
            "google-chrome:Profile 1",
            "--all-cookies",
            "--new-cloud-profile-name",
            "Imported Profile",
        ])?;

        match parsed.command {
            Command::SyncCookies {
                profile,
                local_profile,
                all_cookies,
                new_cloud_profile_name,
                ..
            } => {
                assert_eq!(profile.as_deref(), Some("google-chrome:Profile 1"));
                assert_eq!(local_profile, None);
                assert!(all_cookies);
                assert_eq!(new_cloud_profile_name.as_deref(), Some("Imported Profile"));
                assert_eq!(parsed.config_profile, None);
            }
            other => panic!("expected sync-cookies command, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn cookie_sync_runtime_command_quotes_profile_ids() {
        let command = browser_command_from_args(&[
            "profile".to_string(),
            "sync".to_string(),
            "--profile".to_string(),
            "google-chrome:Profile 1".to_string(),
            "--all-cookies".to_string(),
        ]);

        assert_eq!(
            command,
            "browser profile sync --profile 'google-chrome:Profile 1' --all-cookies"
        );
    }

    #[test]
    fn dataset_browser_cleanup_records_event_even_without_browser_state() -> Result<()> {
        let temp = unique_cli_test_dir("dataset-browser-cleanup")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("cwd");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let session = store.create_session(None, &cwd)?;

        let removed = cleanup_dataset_browser_session(&store, &session.id)?;

        assert_eq!(removed, 0);
        let events = store.events_for_session(&session.id)?;
        let cleanup = events
            .iter()
            .find(|event| event.event_type == "browser.cleaned_up")
            .context("browser.cleaned_up event")?;
        assert_eq!(cleanup.payload["source"], "dataset-run");
        assert_eq!(cleanup.payload["removed_sessions"], 0);

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn dataset_browser_cleanup_can_run_asynchronously() -> Result<()> {
        let temp = unique_cli_test_dir("dataset-browser-cleanup-async")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("cwd");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let session = store.create_session(None, &cwd)?;

        spawn_dataset_browser_cleanup(&store, &session.id);

        let cleanup = {
            let started = Instant::now();
            loop {
                let events = store.events_for_session(&session.id)?;
                if let Some(event) = events
                    .iter()
                    .find(|event| event.event_type == "browser.cleaned_up")
                {
                    break event.payload.clone();
                }
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "timed out waiting for async cleanup event"
                );
                thread::sleep(Duration::from_millis(20));
            }
        };
        assert_eq!(cleanup["source"], "dataset-run");
        assert_eq!(cleanup["removed_sessions"], 0);
        assert_eq!(cleanup["async"], true);

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn dataset_manifest_usage_summary_aggregates_reasoning_tokens() {
        let manifest = serde_json::json!({
            "sessions": [
                {
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 20,
                        "reasoning_output_tokens": 7,
                        "total_tokens": 37,
                        "invocation_count": 1,
                    }
                },
                {
                    "usage": {
                        "input_tokens": 3,
                        "output_tokens": 4,
                        "reasoning_output_tokens": 5,
                        "total_tokens": 12,
                        "invocation_count": 2,
                    }
                }
            ]
        });

        let usage = usage_summary_from_manifest(&manifest);

        assert_eq!(usage["input_tokens"], 13);
        assert_eq!(usage["output_tokens"], 24);
        assert_eq!(usage["reasoning_output_tokens"], 12);
        assert_eq!(usage["total_tokens"], 49);
        assert_eq!(usage["invocation_count"], 3);
    }

    #[test]
    fn cli_spawn_agent_uses_parent_cwd_and_core_context_metadata() -> Result<()> {
        let temp = unique_cli_test_dir("spawn-agent-context")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let (_runtime, socket_path) =
            start_runtime_socket_for_parent(&state_dir, &parent, &parent_cwd, 3)?;

        spawn_agent(
            &store,
            &parent.id,
            "inspect from cli".to_string(),
            Some("cli_child".to_string()),
            None,
            Some("CliNick".to_string()),
            Some("explorer".to_string()),
        )?;

        let child = store
            .list_child_agents(&parent.id)?
            .into_iter()
            .next()
            .context("child agent")?;
        assert_eq!(child.agent_path.as_deref(), Some("/root/cli_child"));
        assert_eq!(child.agent_nickname.as_deref(), Some("CliNick"));
        assert_eq!(child.agent_role.as_deref(), Some("explorer"));
        let child_session = store
            .load_session(&child.child_session_id)?
            .context("child session")?;
        assert_eq!(child_session.cwd, parent_cwd.display().to_string());
        let child_events = store.events_for_session(&child.child_session_id)?;
        let context = child_events
            .iter()
            .find(|event| event.event_type == "agent.context")
            .context("agent.context")?;
        assert_eq!(context.payload["from_session_id"], parent.id);
        assert_eq!(context.payload["agent_path"], "/root/cli_child");
        assert_eq!(context.payload["nickname"], "CliNick");
        assert_eq!(context.payload["role"], "explorer");
        assert_eq!(context.payload["history_mode"], "compact_context");
        let err = spawn_agent(
            &store,
            &parent.id,
            "bad path shape".to_string(),
            Some("other_child".to_string()),
            Some("/root/other_child".to_string()),
            None,
            None,
        )
        .expect_err("task-name and path should be mutually exclusive");
        assert!(err
            .to_string()
            .contains("either --task-name or --path, not both"));

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_spawn_agent_requires_live_runtime_socket() -> Result<()> {
        let temp = unique_cli_test_dir("spawn-agent-no-runtime")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;

        let err = spawn_agent(
            &store,
            &parent.id,
            "inspect from cli".to_string(),
            Some("cli_child".to_string()),
            None,
            None,
            None,
        )
        .expect_err("Store-backed CLI spawn must not be a live path");
        assert!(
            err.to_string().contains("requires a live runtime socket"),
            "{err}"
        );
        assert!(store.list_child_agents(&parent.id)?.is_empty());

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_start_and_followup_persist_typed_input_payload_for_linked_mentions() -> Result<()> {
        let temp = unique_cli_test_dir("typed-input-start-followup")?;
        let state_dir = temp.join("state");
        let store = Store::open(&state_dir)?;

        start(
            &store,
            "check [$Calendar](app://calendar) availability".to_string(),
        )?;
        let session = store
            .list_sessions()?
            .into_iter()
            .next()
            .context("created session")?;
        let input = store
            .events_for_session(&session.id)?
            .into_iter()
            .find(|event| event.event_type == "session.input")
            .context("session.input")?;
        assert_eq!(
            input.payload["app_connector_ids"],
            serde_json::json!(["calendar"])
        );
        assert!(!input.payload["content"]
            .as_array()
            .context("input content")?
            .iter()
            .any(|part| part["text"].as_str().unwrap_or_default().contains("app://")));

        followup(
            &store,
            &session.id,
            "then use [@Notes](plugin://notes@example)".to_string(),
        )?;
        let followup = store
            .events_for_session(&session.id)?
            .into_iter()
            .find(|event| event.event_type == "session.followup")
            .context("session.followup")?;
        assert_eq!(
            followup.payload["plugin_mentions"][0]["path"],
            "plugin://notes@example"
        );
        assert!(!followup.payload["content"]
            .as_array()
            .context("followup content")?
            .iter()
            .any(|part| part["text"]
                .as_str()
                .unwrap_or_default()
                .contains("plugin://")));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_spawn_agent_uses_typed_payload_for_child_input() -> Result<()> {
        let temp = unique_cli_test_dir("spawn-agent-typed-input")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let (_runtime, socket_path) =
            start_runtime_socket_for_parent(&state_dir, &parent, &parent_cwd, 3)?;

        spawn_agent(
            &store,
            &parent.id,
            "inspect [$Calendar](app://calendar)".to_string(),
            Some("typed_child".to_string()),
            None,
            None,
            None,
        )?;

        let child = store
            .list_child_agents(&parent.id)?
            .into_iter()
            .next()
            .context("child agent")?;
        let input = store
            .events_for_session(&child.child_session_id)?
            .into_iter()
            .find(|event| event.event_type == "session.input")
            .context("child session.input")?;
        assert_eq!(
            input.payload["app_connector_ids"],
            serde_json::json!(["calendar"])
        );
        assert!(!input.payload["content"]
            .as_array()
            .context("child input content")?
            .iter()
            .any(|part| part["text"].as_str().unwrap_or_default().contains("app://")));

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_agent_message_resolves_paths_and_rejects_empty_or_root_task() -> Result<()> {
        let temp = unique_cli_test_dir("agent-message-paths")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;

        let err = send_agent_message(&store, &parent.id, "cli_child", "  ", false)
            .expect_err("empty messages should fail");
        assert!(err
            .to_string()
            .contains("Empty message can't be sent to an agent"));
        let err = send_agent_message(&store, &parent.id, "cli_child", "inspect this", false)
            .expect_err("Store-backed CLI send should fail without live runtime");
        assert!(err
            .to_string()
            .contains("send_agent_message requires a live runtime mailbox"));
        let mail = store.messages_for_agent(&child.id)?;
        assert!(
            mail.is_empty(),
            "offline CLI send must not enqueue Store-backed agent_messages rows"
        );
        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events
            .iter()
            .all(|event| event.event_type != "agent.message"));

        let err = send_agent_message(&store, &child.id, "root", "new task", true)
            .expect_err("root trigger turns should fail");
        assert!(err
            .to_string()
            .contains("Tasks can't be assigned to the root agent"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_agent_message_uses_live_runtime_socket_when_available() -> Result<()> {
        let temp = unique_cli_test_dir("agent-message-live-runtime")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/live_child"),
            Some("LiveChild"),
            Some("worker"),
        )?;

        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent.id.clone())?,
            child_agent_id: AgentId::from_string(child.id.clone())?,
            child_session_id: SessionId::from_string(child.id.clone())?,
            cwd: parent_cwd.clone(),
            agent_path: "/root/live_child".to_string(),
            nickname: Some("LiveChild".to_string()),
            role: Some("worker".to_string()),
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime.clone())?;

        send_agent_message(&store, &parent.id, "live_child", "inspect live", false)?;

        assert!(
            store.messages_for_agent(&child.id)?.is_empty(),
            "live runtime socket path must not enqueue store-backed agent_messages rows"
        );
        let pending =
            runtime.pending_agent_mail_for_session(&SessionId::from_string(child.id.clone())?)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content, "inspect live");
        let child_events = store.events_for_session(&child.id)?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "mailbox.enqueued"));

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_followup_uses_live_runtime_socket_when_available() -> Result<()> {
        let temp = unique_cli_test_dir("followup-live-runtime")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;

        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime.clone())?;

        followup(&store, &parent.id, "live followup".to_string())?;

        let pending =
            runtime.pending_agent_mail_for_session(&SessionId::from_string(parent.id.clone())?)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, RuntimeMailboxItemKind::Followup);
        assert_eq!(pending[0].content, "live followup");
        let events = store.events_for_session(&parent.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "mailbox.enqueued"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.followup.runtime_queued"));
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == "session.followup"),
            "live followup should be journaled only when runtime consumes the mailbox item"
        );

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_wait_agent_requires_live_runtime_socket() -> Result<()> {
        let temp = unique_cli_test_dir("wait-agent-events")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;

        let err = wait_agent(&store, &parent.id, Vec::new(), 0)
            .expect_err("Store-backed wait must not be a live CLI wait path");
        assert!(
            err.to_string().contains("requires a live runtime socket"),
            "{err}"
        );
        let events = store.events_for_session(&parent.id)?;
        assert!(events.iter().all(|event| {
            event.event_type != "agent.wait.started"
                && event.event_type != "agent.wait.finished"
                && event.event_type != "wait_agent.started"
                && event.event_type != "wait_agent.timed_out"
        }));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_completion_queues_mail_for_done_parent() -> Result<()> {
        let temp = unique_cli_test_dir("done-parent-completion-mail")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        store.set_status(&parent.id, browser_use_protocol::SessionStatus::Done)?;
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "cli late result"}),
        )?;
        store.set_status(&child.id, browser_use_protocol::SessionStatus::Done)?;

        update_parent_from_child_run(&store, &parent.id, &child.id, None, None)?;

        let events = store.events_for_session(&parent.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "agent.completed"));
        assert!(
            store.messages_for_agent(&parent.id)?.is_empty(),
            "CLI parent projection must not enqueue Store-backed mailbox rows"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_list_agents_accepts_codex_path_prefixes() -> Result<()> {
        let temp = unique_cli_test_dir("list-agent-prefix")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        let grandchild = store.create_child_session(
            &child.id,
            &parent_cwd,
            Some("/root/cli_child/grand"),
            None,
            None,
        )?;
        store.append_event(
            &grandchild.id,
            "session.input",
            serde_json::json!({"text": "nested task"}),
        )?;

        list_agents(&store, &parent.id, Some("/root/cli_child"), true)?;
        list_agents(&store, &child.id, Some("grand"), true)?;

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_close_agent_rejects_root_and_records_parent_cancellation() -> Result<()> {
        let temp = unique_cli_test_dir("close-agent")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;

        let err = close_agent(&store, None, &parent.id, "not valid")
            .expect_err("root sessions should not close as spawned agents");
        assert!(err.to_string().contains("root is not a spawned agent"));
        close_agent(&store, Some(&parent.id), "cli_child", "done with child")?;

        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "closed"
        );
        let parent_events = store.events_for_session(&parent.id)?;
        let cancelled = parent_events
            .iter()
            .find(|event| event.event_type == "agent.cancelled")
            .context("agent.cancelled")?;
        assert_eq!(cancelled.payload["child_session_id"], child.id);
        assert_eq!(cancelled.payload["payload"]["reason"], "done with child");
        let err = close_agent(&store, Some(&parent.id), "root", "nope")
            .expect_err("path root should not close");
        assert!(err.to_string().contains("root is not a spawned agent"));
        let err = close_agent(&store, None, "cli_child", "missing current")
            .expect_err("path close should require current id");
        assert!(err
            .to_string()
            .contains("requires --current-id when target is not an agent id"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_close_agent_uses_live_runtime_socket_when_available() -> Result<()> {
        let temp = unique_cli_test_dir("close-agent-live-runtime")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/live_child"),
            Some("LiveChild"),
            Some("worker"),
        )?;
        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent.id.clone())?,
            child_agent_id: AgentId::from_string(child.id.clone())?,
            child_session_id: SessionId::from_string(child.id.clone())?,
            cwd: parent_cwd.clone(),
            agent_path: "/root/live_child".to_string(),
            nickname: Some("LiveChild".to_string()),
            role: Some("worker".to_string()),
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime.clone())?;

        close_agent(
            &store,
            Some(&parent.id),
            "live_child",
            "done with live child",
        )?;

        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "closed"
        );
        assert_eq!(
            store.load_session(&child.id)?.unwrap().status,
            browser_use_protocol::SessionStatus::Cancelled
        );
        let child_events = store.events_for_session(&child.id)?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.closed"));
        let parent_events = store.events_for_session(&parent.id)?;
        let cancelled = parent_events
            .iter()
            .find(|event| event.event_type == "agent.cancelled")
            .context("agent.cancelled")?;
        assert_eq!(cancelled.payload["child_session_id"], child.id);
        assert_eq!(
            cancelled.payload["payload"]["reason"].as_str(),
            Some("done with live child")
        );

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_resume_agent_reopens_closed_subtree_like_v1() -> Result<()> {
        let temp = unique_cli_test_dir("resume-agent")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        let grandchild = store.create_child_session(
            &child.id,
            &parent_cwd,
            Some("/root/cli_child/grand"),
            None,
            None,
        )?;

        close_agent(&store, None, &child.id, "pause child")?;
        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "closed"
        );
        assert_eq!(
            store
                .agent_summary_for_child(&grandchild.id)?
                .unwrap()
                .status,
            "open"
        );
        assert_eq!(
            store.load_session(&child.id)?.unwrap().status,
            browser_use_protocol::SessionStatus::Cancelled
        );
        assert_eq!(
            store.load_session(&grandchild.id)?.unwrap().status,
            browser_use_protocol::SessionStatus::Cancelled
        );

        resume_agent(&store, &child.id)?;

        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "open"
        );
        assert_eq!(
            store
                .agent_summary_for_child(&grandchild.id)?
                .unwrap()
                .status,
            "open"
        );
        assert_eq!(
            store.load_session(&child.id)?.unwrap().status,
            browser_use_protocol::SessionStatus::Created
        );
        assert_eq!(
            store.load_session(&grandchild.id)?.unwrap().status,
            browser_use_protocol::SessionStatus::Created
        );
        let err = resume_agent(&store, &parent.id).expect_err("root should not resume as child");
        assert!(err.to_string().contains("root is not a spawned agent"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_wait_agent_uses_live_runtime_socket_when_available() -> Result<()> {
        let temp = unique_cli_test_dir("wait-agent-live-runtime")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/live_child"),
            Some("LiveChild"),
            Some("worker"),
        )?;

        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent.id.clone())?,
            child_agent_id: AgentId::from_string(child.id.clone())?,
            child_session_id: SessionId::from_string(child.id.clone())?,
            cwd: parent_cwd.clone(),
            agent_path: "/root/live_child".to_string(),
            nickname: Some("LiveChild".to_string()),
            role: Some("worker".to_string()),
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime.clone())?;
        runtime.send_agent_message(browser_use_runtime::SendAgentMessageRequest {
            author_agent_id: AgentId::from_string(child.id.clone())?,
            target_agent_id: AgentId::from_string(parent.id.clone())?,
            content: "child finished".to_string(),
            trigger_turn: false,
            kind: RuntimeMailboxItemKind::Completion,
            delivery_phase: RuntimeMailboxDeliveryPhase::NextTurn,
            payload: serde_json::json!({"source": "test"}),
        })?;

        wait_agent(&store, &parent.id, Vec::new(), 50)?;

        assert!(
            store.messages_for_agent(&parent.id)?.is_empty(),
            "live runtime wait path must not depend on store-backed agent_messages rows"
        );
        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "wait_agent.completed"));
        assert!(!parent_events
            .iter()
            .any(|event| event.event_type == "agent.wait.finished"));

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_runtime_server_guard_exposes_runtime_socket_for_run_process() -> Result<()> {
        let temp = unique_cli_test_dir("run-runtime-socket")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("work");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let _session = store.create_session(None, &cwd)?;
        let runtime = cli_runtime_handle(&store)?;
        let socket_path = browser_use_runtime::local_runtime_socket_path(&state_dir);
        assert!(!socket_path.exists());

        let guard = CliLocalRuntimeServer::ensure(&store, &runtime)?;
        let response = send_local_runtime_request(
            &state_dir,
            &LocalRuntimeRequest::Ping,
            Duration::from_millis(500),
        )?;
        assert!(response.is_some_and(|response| response.ok));
        assert!(socket_path.exists());

        drop(guard);
        assert!(
            !socket_path.exists(),
            "run-owned local runtime socket should be removed when the run bridge drops"
        );
        Ok(())
    }

    #[test]
    fn cli_cancel_uses_live_runtime_socket_when_available() -> Result<()> {
        let temp = unique_cli_test_dir("cancel-live-runtime")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("work");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let session = store.create_session(None, &cwd)?;

        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(session.id.clone())?,
            cwd: cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        let token = runtime.register_run(SessionId::from_string(session.id.clone())?);
        let socket_path = browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime)?;

        cancel(&store, &session.id, "test cancel")?;

        assert!(
            token.is_cancelled(),
            "CLI cancel must cancel the live runtime token"
        );
        let events = store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "agent.cancel_requested"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "runtime.cancel.forwarded"));
        assert!(events
            .iter()
            .all(|event| event.event_type != "session.cancel_requested"));
        assert!(events
            .iter()
            .all(|event| event.event_type != "session.cancelled"));

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_wait_agent_target_uses_live_runtime_socket() -> Result<()> {
        let temp = unique_cli_test_dir("wait-agent-targets")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;

        let err = wait_agent(&store, &parent.id, vec!["not_an_id".to_string()], 0)
            .expect_err("invalid target id should fail before runtime lookup");
        assert!(err.to_string().contains("invalid agent id `not_an_id`"));

        let journal = Arc::new(SqliteJournal::from_store(Store::open(&state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.clone(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent.id.clone())?,
            child_agent_id: AgentId::from_string(child.id.clone())?,
            child_session_id: SessionId::from_string(child.id.clone())?,
            cwd: parent_cwd.clone(),
            agent_path: "/root/cli_child".to_string(),
            nickname: Some("CliNick".to_string()),
            role: Some("worker".to_string()),
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(&state_dir, runtime.clone())?;
        runtime.send_agent_message(browser_use_runtime::SendAgentMessageRequest {
            author_agent_id: AgentId::from_string(child.id.clone())?,
            target_agent_id: AgentId::from_string(parent.id.clone())?,
            content: "child finished".to_string(),
            trigger_turn: false,
            kind: RuntimeMailboxItemKind::Completion,
            delivery_phase: RuntimeMailboxDeliveryPhase::NextTurn,
            payload: serde_json::json!({"source": "test", "result": "complete", "success": true, "agent_path": "/root/cli_child"}),
        })?;

        wait_agent(&store, &parent.id, vec![child.id.clone()], 0)?;

        let events = store.events_for_session(&parent.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "wait_agent.completed"));
        assert!(events
            .iter()
            .all(|event| event.event_type != "agent.wait.finished"));
        assert!(store.messages_for_agent(&parent.id)?.is_empty());

        let _ = std::fs::remove_file(socket_path);
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_terminal_status_projects_parent_without_store_mail() -> Result<()> {
        let temp = unique_cli_test_dir("child-notification")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;
        let child = store.load_session(&child.id)?.context("child session")?;

        notify_parent_agent_done(&store, &child)?;

        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "done"
        );
        let parent_events = store.events_for_session(&parent.id)?;
        let completed = parent_events
            .iter()
            .find(|event| event.event_type == "agent.completed")
            .context("agent.completed")?;
        assert_eq!(completed.payload["payload"]["child_session_id"], child.id);
        assert_eq!(completed.payload["payload"]["status"], "done");
        assert_eq!(completed.payload["payload"]["result"], "done");
        assert!(
            store.messages_for_agent(&parent.id)?.is_empty(),
            "CLI parent projection must not enqueue Store-backed mailbox rows"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_terminal_status_ignores_late_completion_after_close() -> Result<()> {
        let temp = unique_cli_test_dir("child-notification-closed")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        store.append_event(
            &child.id,
            "agent.run.started",
            serde_json::json!({ "run_id": "run-closed" }),
        )?;
        store.close_child_agent(&child.id, "closed by close_agent")?;
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "late"}),
        )?;
        let child = store.load_session(&child.id)?.context("child session")?;

        notify_parent_agent_done(&store, &child)?;

        assert_eq!(
            store.agent_summary_for_child(&child.id)?.unwrap().status,
            "closed"
        );
        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events.iter().all(|event| {
            event.event_type != "agent.completed" && event.event_type != "agent.failed"
        }));
        assert!(store.messages_for_agent(&parent.id)?.is_empty());

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_terminal_status_projects_parent_once_without_store_mail() -> Result<()> {
        let temp = unique_cli_test_dir("child-notification-once")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        store.append_event(
            &child.id,
            "agent.run.started",
            serde_json::json!({ "run_id": "run-once" }),
        )?;
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "done once"}),
        )?;
        let child = store.load_session(&child.id)?.context("child session")?;

        notify_parent_agent_done(&store, &child)?;
        notify_parent_agent_done(&store, &child)?;

        let parent_events = store.events_for_session(&parent.id)?;
        assert_eq!(
            parent_events
                .iter()
                .filter(|event| event.event_type == "agent.completed")
                .count(),
            1
        );
        assert!(
            store.messages_for_agent(&parent.id)?.is_empty(),
            "CLI parent projection must not enqueue Store-backed mailbox rows"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_terminal_status_ignores_stale_old_run_after_restart() -> Result<()> {
        let temp = unique_cli_test_dir("child-notification-stale-run")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;
        let child = store.create_child_session(
            &parent.id,
            &parent_cwd,
            Some("/root/cli_child"),
            Some("CliNick"),
            Some("worker"),
        )?;
        store.append_event(
            &child.id,
            "agent.run.started",
            serde_json::json!({ "run_id": "run-old" }),
        )?;
        store.append_event(
            &child.id,
            "agent.run.started",
            serde_json::json!({ "run_id": "run-new" }),
        )?;
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "late old completion"}),
        )?;

        let stale =
            update_parent_from_child_run(&store, &parent.id, &child.id, None, Some("run-old"))?;

        assert_eq!(stale["status"], "stale");
        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events.iter().all(|event| {
            event.event_type != "agent.completed" && event.event_type != "agent.failed"
        }));
        assert!(store.messages_for_agent(&parent.id)?.is_empty());

        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "new completion"}),
        )?;
        let completed =
            update_parent_from_child_run(&store, &parent.id, &child.id, None, Some("run-new"))?;

        assert_eq!(completed["status"], "done");
        assert_eq!(completed["result"], "new completion");
        let parent_events = store.events_for_session(&parent.id)?;
        assert_eq!(
            parent_events
                .iter()
                .filter(|event| event.event_type == "agent.completed")
                .count(),
            1
        );
        assert!(
            store.messages_for_agent(&parent.id)?.is_empty(),
            "CLI parent projection must not enqueue Store-backed mailbox rows"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_show_uses_latest_terminal_event_not_stale_failure() -> Result<()> {
        let temp = unique_cli_test_dir("show-latest-terminal")?;
        let state_dir = temp.join("state");
        let cwd = temp.join("cwd");
        std::fs::create_dir_all(&cwd)?;
        let store = Store::open(&state_dir)?;
        let session = store.create_session(None, &cwd)?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect"}),
        )?;
        store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({"error": "old failure"}),
        )?;
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "later success"}),
        )?;

        let events = store.events_for_session(&session.id)?;
        let terminal = latest_child_terminal_from_events(&events).context("latest terminal")?;
        assert_eq!(terminal.result.as_deref(), Some("later success"));
        assert!(terminal.failure.is_none());
        show(&store, &session.id)?;

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_default_model_uses_config_model() -> Result<()> {
        // An explicit `model=` override resolves through the config layer for a
        // real backend (openai). (The codex backend is cut: it no longer resolves
        // a chatgpt model and folds into the fake default.)
        let overrides = vec![(
            "model".to_string(),
            toml::Value::String("configured-model".to_string()),
        )];

        assert_eq!(
            default_cli_model_for_backend_with_overrides(
                ProviderBackend::Openai,
                None,
                &overrides
            )?,
            "configured-model"
        );
        Ok(())
    }

    #[test]
    fn cli_model_source_treats_config_model_override_as_explicit() -> Result<()> {
        let (model, source) = resolve_cli_model_with_source(
            ProviderBackend::Openai,
            None,
            None,
            &["model=\"configured-model\"".to_string()],
        )?;

        assert_eq!(model, "configured-model");
        assert_eq!(source, RunConfigValueSource::Explicit);

        let (model, source) = resolve_cli_model_with_source(
            ProviderBackend::Openai,
            Some("flag-model".to_string()),
            None,
            &["model=\"configured-model\"".to_string()],
        )?;

        assert_eq!(model, "flag-model");
        assert_eq!(source, RunConfigValueSource::Explicit);
        Ok(())
    }

    #[test]
    fn cli_default_provider_id_uses_config_provider() -> Result<()> {
        // The new engine's config layer reads the `model_provider_id` override
        // key (not the legacy `model_provider` alias), so the explicit provider
        // id flows through for a real backend.
        let overrides = vec![(
            "model_provider_id".to_string(),
            toml::Value::String("corp".to_string()),
        )];

        assert_eq!(
            resolved_cli_provider_id_for_backend_with_overrides(
                ProviderBackend::Openai,
                None,
                &overrides
            )?,
            "corp"
        );

        let defaults = default_settings(None, &overrides)?;
        assert_eq!(
            defaults
                .iter()
                .find(|(key, _)| key == "provider.id")
                .map(|(_, value)| value.as_str()),
            Some("corp")
        );
        Ok(())
    }

    #[test]
    fn cli_default_settings_use_openrouter_provider_path() -> Result<()> {
        let defaults = default_settings(None, &[])?;
        let value_for = |key: &str| {
            defaults
                .iter()
                .find(|(setting, _)| setting == key)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(value_for("account"), Some("OpenRouter API key"));
        assert_eq!(value_for("model"), Some("GPT-5.5"));
        assert_eq!(value_for("provider.model"), Some("openai/gpt-5.5"));
        assert_eq!(value_for("provider.id"), Some("openrouter"));
        assert_eq!(value_for("agent.backend"), Some("openrouter"));
        Ok(())
    }

    #[test]
    fn cli_provider_source_treats_config_provider_override_as_explicit() {
        assert_eq!(cli_provider_id_source(&[]), RunConfigValueSource::Default);
        assert_eq!(
            cli_provider_id_source(&[(
                "model_provider".to_string(),
                toml::Value::String("corp".to_string())
            )]),
            RunConfigValueSource::Explicit
        );
    }

    #[test]
    fn cli_default_model_falls_back_to_bundled_catalog_default() -> Result<()> {
        // The new engine's config layer does not load a `model_catalog_json`
        // override or apply chatgpt/api auth-filtering (a documented Phase-E
        // gap-fill); with no configured model a real backend resolves the bundled
        // catalog default.
        let overrides = vec![("model".to_string(), toml::Value::String(String::new()))];

        assert_eq!(
            default_cli_model_for_backend_with_overrides(
                ProviderBackend::Openai,
                None,
                &overrides
            )?,
            browser_use_agent::config_model::BUNDLED_DEFAULT_MODEL
        );
        Ok(())
    }

    fn unique_cli_test_dir(name: &str) -> Result<std::path::PathBuf> {
        std::env::set_var("BUT_PRODUCT_ANALYTICS", "false");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "browser-use-cli-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn start_runtime_socket_for_parent(
        state_dir: &std::path::Path,
        parent: &browser_use_protocol::SessionMeta,
        parent_cwd: &std::path::Path,
        max_concurrent_threads_per_session: usize,
    ) -> Result<(RuntimeHandle, std::path::PathBuf)> {
        let journal = Arc::new(SqliteJournal::from_store(Store::open(state_dir)?));
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        let runtime = BrowserUseRuntime::new(persistence, state_index).handle();
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: SessionId::from_string(parent.id.clone())?,
            cwd: parent_cwd.to_path_buf(),
            task: "root task".to_string(),
            max_concurrent_threads_per_session,
        })?;
        let socket_path =
            browser_use_runtime::spawn_local_runtime_server(state_dir, runtime.clone())?;
        Ok((runtime, socket_path))
    }
}
