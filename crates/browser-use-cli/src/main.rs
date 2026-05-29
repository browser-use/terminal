use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_core::{
    append_user_shell_command_context_event, append_workspace_context_event,
    append_workspace_context_event_with_options, canonical_agent_path_from_task_name,
    canonical_agent_reference, cleanup_agent_runtime_state_for_agent_subtree, collect_agent_tree,
    configured_model_provider_id_for_cwd_with_options, default_model_for_cwd_with_options,
    display_agent_path_for_session, final_statuses_for_v1_wait, install_process_crypto_provider,
    last_task_message_for_agent, local_agent_status_value, model_catalog_for_cwd_with_options,
    parse_config_overrides, product_analytics, record_browser_script_response_events,
    record_python_response_final_event, record_python_worker_event,
    resolve_agent_reference_in_tree, review_prompt_base_branch, review_prompt_commit,
    review_prompt_custom, review_prompt_uncommitted_changes, root_session_id,
    run_agent_from_config, run_existing_session_from_config, run_existing_session_with_provider,
    run_fake_agent, start_review_session, typed_user_input_payload_from_text_for_cwd,
    update_parent_from_child_run, AgentRunOptions, CollaborationModeKind, ConfigOverrides,
    FakeAgentOptions, ProviderBackend, ProviderRunConfig, RunConfigValueSource,
    UnifiedExecShutdownCleanup,
};
use browser_use_protocol::{
    browser_summary_from_events, failure_from_events, sanitized_agent_context_from_events,
    session_result_from_events, task_from_events,
};
use browser_use_providers::{
    claude_code_oauth_authorize_url, claude_code_oauth_pkce,
    exchange_claude_code_authorization_code, load_codex_auth, load_codex_auth_file,
    load_codex_managed_auth, load_codex_managed_auth_file, parse_claude_code_authorization_input,
    refresh_claude_code_oauth, AnthropicMessagesProvider, ClaudeCodeOAuthCredential, CodexAuth,
    CodexManagedAuth, FakeProvider, ModelProvider, OpenAICompatibleChatProvider,
    CLAUDE_CODE_CALLBACK_HOST, CLAUDE_CODE_CALLBACK_PATH, CLAUDE_CODE_CALLBACK_PORT,
};
use browser_use_python_worker::PythonWorker;
use browser_use_store::{now_ms, resolve_state_dir, Store};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CollaborationModeArg {
    Default,
    Plan,
}

impl From<CollaborationModeArg> for CollaborationModeKind {
    fn from(value: CollaborationModeArg) -> Self {
        match value {
            CollaborationModeArg::Default => CollaborationModeKind::Default,
            CollaborationModeArg::Plan => CollaborationModeKind::Plan,
        }
    }
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
    RunCodex {
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
    RunOpencode {
        text: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunOpencodeGo {
        text: String,
        #[arg(long, default_value = "kimi-k2.6")]
        model: String,
    },
    RunOpenaiSession {
        task_id: String,
        #[arg(long)]
        model: Option<String>,
    },
    RunCodexSession {
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
    RunOpencodeSession {
        task_id: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunOpencodeGoSession {
        task_id: String,
        #[arg(long, default_value = "kimi-k2.6")]
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
    Opencode,
    OpencodeGo,
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
struct DirectDatasetRunner<P> {
    provider: P,
}

impl<P> DatasetRunner for DirectDatasetRunner<P>
where
    P: ModelProvider + Clone + Send + Sync + 'static,
{
    fn run_dataset_session(
        &self,
        store: &Store,
        session_id: &str,
        options: AgentRunOptions,
    ) -> Result<()> {
        run_existing_session_with_provider(store, &self.provider, session_id, options)?;
        Ok(())
    }
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
        merged_options.collaboration_mode = config.options.collaboration_mode;
        config.options = merged_options;
        run_existing_session_from_config_and_notify(store, session_id, config)?;
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
    args.state_dir = resolve_state_dir(&args.state_dir);
    let store = Store::open(&args.state_dir)?;
    product_analytics::capture_async(
        &store,
        "bu:cli command ran",
        serde_json::json!({ "command": command_name(&args.command), "surface": "cli" }),
    );
    let config_profile = args.config_profile.clone();
    let config_overrides = args.config_overrides.clone();
    let collaboration_mode = args.collaboration_mode.into();
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
        ),
        Command::RunCodex { text, model } => run_codex(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunAnthropic { text, model } => run_anthropic(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunOpenrouter { text, model } => run_openrouter(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunDeepseek { text, model } => run_deepseek(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunOpencode { text, model } => run_opencode(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            ProviderBackend::Opencode,
        ),
        Command::RunOpencodeGo { text, model } => run_opencode(
            &store,
            text,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            ProviderBackend::OpencodeGo,
        ),
        Command::RunOpenaiSession { task_id, model } => run_openai_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunCodexSession { task_id, model } => run_codex_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunAnthropicSession { task_id, model } => run_anthropic_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunOpenrouterSession { task_id, model } => run_openrouter_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunDeepseekSession { task_id, model } => run_deepseek_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
        ),
        Command::RunOpencodeSession { task_id, model } => run_opencode_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            ProviderBackend::Opencode,
        ),
        Command::RunOpencodeGoSession { task_id, model } => run_opencode_session(
            &store,
            &task_id,
            model,
            config_profile.as_deref(),
            &config_overrides,
            collaboration_mode,
            ProviderBackend::OpencodeGo,
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
        ),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Start { .. } => "start",
        Command::RunFake { .. } => "run_fake",
        Command::RunOpenai { .. } => "run_openai",
        Command::RunCodex { .. } => "run_codex",
        Command::RunAnthropic { .. } => "run_anthropic",
        Command::RunOpenrouter { .. } => "run_openrouter",
        Command::RunDeepseek { .. } => "run_deepseek",
        Command::RunOpencode { .. } => "run_opencode",
        Command::RunOpencodeGo { .. } => "run_opencode_go",
        Command::RunOpenaiSession { .. } => "run_openai_session",
        Command::RunCodexSession { .. } => "run_codex_session",
        Command::RunAnthropicSession { .. } => "run_anthropic_session",
        Command::RunOpenrouterSession { .. } => "run_openrouter_session",
        Command::RunDeepseekSession { .. } => "run_deepseek_session",
        Command::RunOpencodeSession { .. } => "run_opencode_session",
        Command::RunOpencodeGoSession { .. } => "run_opencode_go_session",
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
        Command::UserShell { .. } => "user_shell",
        Command::Review { .. } => "review",
        Command::Export { .. } => "export",
        Command::Import { .. } => "import",
        Command::Config { .. } => "config",
        Command::Auth { .. } => "auth",
        Command::Diagnostics => "diagnostics",
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
        product_analytics::capture_blocking(
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
    product_analytics::capture_blocking(
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
        product_analytics::capture_blocking(
            store,
            "bu:cli update failed",
            serde_json::json!({ "surface": "cli", "release": release.as_str() }),
        );
        bail!("installer exited with status {status}");
    }
    product_analytics::capture_blocking(
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
    append_workspace_context_event_with_options(store, &session, &config.options)?;
    store.append_event(
        &session.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&text, &cwd)?,
    )?;
    maybe_append_message_history(&session.id, &text, &cwd, &config.options);
    let session_id = run_existing_session_from_config(store, &session.id, config)?;
    println!("{session_id}");
    Ok(())
}

fn maybe_append_message_history(
    session_id: &str,
    text: &str,
    cwd: &Path,
    options: &AgentRunOptions,
) {
    #[cfg(not(test))]
    {
        let _ =
            browser_use_core::append_message_history_entry_for_cwd(text, session_id, cwd, options);
    }
    #[cfg(test)]
    {
        let _ = (session_id, text, cwd, options);
    }
}

fn run_fake(store: &Store, text: String, python_code: Option<String>) -> Result<()> {
    let session_id = run_fake_agent(
        store,
        &text,
        std::env::current_dir()?,
        FakeAgentOptions {
            python_code: python_code.as_deref(),
        },
    )?;
    println!("{session_id}");
    Ok(())
}

fn cli_agent_options(
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
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
    if !config_overrides.is_empty() {
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
        ProviderBackend::Codex => {
            default_model_for_cwd_with_options(cwd, config_profile, config_overrides, true)
        }
        ProviderBackend::Openai => {
            default_model_for_cwd_with_options(cwd, config_profile, config_overrides, false)
        }
        ProviderBackend::Anthropic => Ok("claude-sonnet-4-6".to_string()),
        ProviderBackend::Openrouter => Ok("openai/gpt-5.5".to_string()),
        ProviderBackend::Deepseek => Ok("deepseek-v4-pro".to_string()),
        ProviderBackend::Opencode => Ok("gpt-5.5".to_string()),
        ProviderBackend::OpencodeGo => Ok("kimi-k2.6".to_string()),
        ProviderBackend::Fake | ProviderBackend::None => Ok("fake".to_string()),
    }
}

fn default_provider_id_for_backend(backend: ProviderBackend) -> &'static str {
    match backend {
        ProviderBackend::Codex => "codex",
        ProviderBackend::Openai => "openai",
        ProviderBackend::Anthropic => "anthropic",
        ProviderBackend::Openrouter => "openrouter",
        ProviderBackend::Deepseek => "deepseek",
        ProviderBackend::Opencode => "opencode",
        ProviderBackend::OpencodeGo => "opencode-go",
        ProviderBackend::Fake => "fake",
        ProviderBackend::None => "none",
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
        )?);
    run_new_session_from_config(store, text, config)
}

fn run_codex(
    store: &Store,
    text: String,
    model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
) -> Result<()> {
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Codex,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config = ProviderRunConfig::new(ProviderBackend::Codex, model)
        .with_model_source(model_source)
        .with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
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
) -> Result<()> {
    let config = ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    run_new_session_from_config(store, text, config)
}

fn run_openrouter(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
) -> Result<()> {
    let config = ProviderRunConfig::new(ProviderBackend::Openrouter, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    run_new_session_from_config(store, text, config)
}

fn run_deepseek(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
) -> Result<()> {
    let config = ProviderRunConfig::new(ProviderBackend::Deepseek, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    let session_id = run_agent_from_config(store, &text, std::env::current_dir()?, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_opencode(
    store: &Store,
    text: String,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    backend: ProviderBackend,
) -> Result<()> {
    let config = ProviderRunConfig::new(backend, model).with_options(cli_agent_options(
        config_profile,
        raw_config_overrides,
        collaboration_mode,
    )?);
    run_new_session_from_config(store, text, config)
}

fn run_openai_session(
    store: &Store,
    task_id: &str,
    model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
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
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_codex_session(
    store: &Store,
    task_id: &str,
    model: Option<String>,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Codex,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config = ProviderRunConfig::new(ProviderBackend::Codex, model)
        .with_model_source(model_source)
        .with_options(cli_agent_options(
            config_profile,
            raw_config_overrides,
            collaboration_mode,
        )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
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
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config = ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
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
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config = ProviderRunConfig::new(ProviderBackend::Openrouter, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
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
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config = ProviderRunConfig::new(ProviderBackend::Deepseek, model).with_options(
        cli_agent_options(config_profile, raw_config_overrides, collaboration_mode)?,
    );
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_opencode_session(
    store: &Store,
    task_id: &str,
    model: String,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
    collaboration_mode: CollaborationModeKind,
    backend: ProviderBackend,
) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config = ProviderRunConfig::new(backend, model).with_options(cli_agent_options(
        config_profile,
        raw_config_overrides,
        collaboration_mode,
    )?);
    let session_id = run_existing_session_from_config_and_notify(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_existing_session_from_config_and_notify(
    store: &Store,
    task_id: &str,
    config: ProviderRunConfig,
) -> Result<String> {
    let result = run_existing_session_from_config(store, task_id, config);
    let run_error = result.as_ref().err().map(|error| format!("{error:#}"));
    let child_id = result.as_deref().unwrap_or(task_id);
    notify_parent_after_cli_child_run(store, child_id, run_error)?;
    result
}

fn notify_parent_after_cli_child_run(
    store: &Store,
    child_id: &str,
    run_error: Option<String>,
) -> Result<()> {
    let Some(child) = store.load_session(child_id)? else {
        return Ok(());
    };
    let Some(parent_id) = child.parent_id.as_deref() else {
        return Ok(());
    };
    update_parent_from_child_run(store, parent_id, child_id, run_error)?;
    Ok(())
}

fn followup(store: &Store, task_id: &str, text: String) -> Result<()> {
    let session = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.followup",
        typed_user_input_payload_from_text_for_cwd(&text, &session.cwd)?,
    )?;
    maybe_append_message_history(
        task_id,
        &text,
        Path::new(&session.cwd),
        &AgentRunOptions::default(),
    );
    println!("followup {task_id}");
    Ok(())
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
    store.request_cancel(task_id, reason)?;
    cleanup_agent_runtime_state_for_agent_subtree(store, task_id)?;
    notify_parent_agent_done(store, &task)?;
    println!("cancelled {task_id}");
    Ok(())
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
    if let Some(result) = session_result_from_events(&events) {
        println!();
        println!("Result");
        println!("{result}");
    }
    if let Some(error) = failure_from_events(&events) {
        println!();
        println!("Failure");
        println!("{error}");
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
    if let Some(cdp_url) = std::env::var("BU_CDP_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    {
        let connect = browser_use_browser::run_browser_command(
            task_id,
            &task.cwd,
            &task.artifact_root,
            &format!("browser connect remote-cdp --url {}", cdp_url.trim()),
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
    append_user_shell_command_context_event(
        store, task_id, &command, exit_code, duration, &combined,
    )?;
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
    }
}

fn default_settings(
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<Vec<(String, String)>> {
    let provider_model = default_cli_model_for_backend_with_overrides(
        ProviderBackend::Codex,
        config_profile,
        config_overrides,
    )?;
    let display_model =
        display_model_for_provider_model(&provider_model, config_profile, config_overrides)?;
    let provider_id = resolved_cli_provider_id_for_backend_with_overrides(
        ProviderBackend::Codex,
        config_profile,
        config_overrides,
    )?;
    Ok(vec![
        ("account".to_string(), "Codex login".to_string()),
        ("model".to_string(), display_model),
        ("provider.model".to_string(), provider_model),
        ("provider.id".to_string(), provider_id),
        ("browser".to_string(), "Local Chrome".to_string()),
        ("agent.backend".to_string(), "codex".to_string()),
        ("setup.complete".to_string(), "0".to_string()),
    ])
}

fn display_model_for_provider_model(
    model: &str,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let catalog = model_catalog_for_cwd_with_options(cwd, config_profile, config_overrides)?;
    Ok(catalog
        .entry_for_model(model)
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

fn auth(store: &Store, command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Status => {
            print_api_key_status(
                store,
                "Browser Use cloud key",
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
            print_api_key_status(
                store,
                "OpenCode Zen API key",
                "auth.opencode.api_key",
                &["OPENCODE_API_KEY", "OPENCODE_ZEN_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "OpenCode Go API key",
                "auth.opencode_go.api_key",
                &["OPENCODE_GO_API_KEY"],
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
            store.set_setting("browser", "Browser Use cloud")?;
            println!("{}: connected (stored)", auth_account_label(account));
            Ok(())
        }
        AuthAccount::Openai
        | AuthAccount::Anthropic
        | AuthAccount::Openrouter
        | AuthAccount::Deepseek
        | AuthAccount::Opencode
        | AuthAccount::OpencodeGo => {
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
        | AuthAccount::Deepseek
        | AuthAccount::Opencode
        | AuthAccount::OpencodeGo => {
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

fn stored_or_env(store: &Store, setting_key: &str, env_names: &[&str]) -> Result<Option<String>> {
    if let Some(value) = store.get_setting(setting_key)? {
        if !value.trim().is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(env_names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty()))
}

fn setting_or_env_or_default(
    store: &Store,
    setting_key: &str,
    env_names: &[&str],
    default: &str,
) -> Result<String> {
    Ok(stored_or_env(store, setting_key, env_names)?.unwrap_or_else(|| default.to_string()))
}

fn anthropic_provider(store: &Store, model: String) -> Result<AnthropicMessagesProvider> {
    let base_url = setting_or_env_or_default(
        store,
        "auth.anthropic.base_url",
        &["LLM_BROWSER_ANTHROPIC_BASE_URL"],
        "https://api.anthropic.com/v1",
    )?;
    if store
        .get_setting("account")?
        .as_deref()
        .is_some_and(is_claude_code_account)
    {
        let auth_token = claude_code_access_token(store)?;
        return Ok(AnthropicMessagesProvider::with_auth_token(
            auth_token, model, base_url,
        ));
    }
    let api_key = stored_or_env(
        store,
        "auth.anthropic.api_key",
        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
    )?
    .context("run `auth login anthropic --api-key ...` or set LLM_BROWSER_ANTHROPIC_API_KEY")?;
    Ok(AnthropicMessagesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn claude_code_access_token(store: &Store) -> Result<String> {
    if let Some(refresh_token) = store.get_setting("auth.claude_code.refresh_token")? {
        let expires_ms = store
            .get_setting("auth.claude_code.expires_ms")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        if !refresh_token.trim().is_empty() && expires_ms <= now_ms() + 60_000 {
            let credential = refresh_claude_code_oauth(refresh_token.trim())
                .context("refresh Claude Code OAuth token")?;
            store_claude_code_oauth(store, &credential)?;
            return Ok(credential.access_token);
        }
    }
    if let Some(access_token) = stored_or_env(
        store,
        "auth.claude_code.access_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )? {
        return Ok(access_token);
    }
    stored_or_env(
        store,
        "auth.claude_code.auth_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )?
    .context(
        "run `auth login claude-code` to sign in with Claude Code, or set CLAUDE_CODE_OAUTH_TOKEN",
    )
}

fn is_claude_code_account(account: &str) -> bool {
    matches!(account, "Claude Code login" | "Claude Code subscription")
}

fn openrouter_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openrouter.api_key",
        &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
    )?
    .context("run `auth login openrouter --api-key ...` or set OPENROUTER_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openrouter.base_url",
        &["LLM_BROWSER_OPENAI_COMPAT_BASE_URL", "OPENROUTER_BASE_URL"],
        "https://openrouter.ai/api/v1",
    )?;
    Ok(OpenAICompatibleChatProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn api_key_setting(account: AuthAccount) -> Option<&'static str> {
    match account {
        AuthAccount::Openai => Some("auth.openai.api_key"),
        AuthAccount::Anthropic => Some("auth.anthropic.api_key"),
        AuthAccount::Openrouter => Some("auth.openrouter.api_key"),
        AuthAccount::Deepseek => Some("auth.deepseek.api_key"),
        AuthAccount::Opencode => Some("auth.opencode.api_key"),
        AuthAccount::OpencodeGo => Some("auth.opencode_go.api_key"),
        AuthAccount::BrowserUseCloud => Some(BROWSER_USE_CLOUD_API_KEY_SETTING),
        AuthAccount::Codex | AuthAccount::ClaudeCode => None,
    }
}

fn auth_account_label(account: AuthAccount) -> &'static str {
    match account {
        AuthAccount::Codex => "Codex login",
        AuthAccount::ClaudeCode => "Claude Code login",
        AuthAccount::BrowserUseCloud => "Browser Use cloud",
        AuthAccount::Openai => "OpenAI API key",
        AuthAccount::Anthropic => "Anthropic API key",
        AuthAccount::Openrouter => "OpenRouter API key",
        AuthAccount::Deepseek => "DeepSeek API key",
        AuthAccount::Opencode => "OpenCode Zen API key",
        AuthAccount::OpencodeGo => "OpenCode Go API key",
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

fn spawn_agent(
    store: &Store,
    parent_id: &str,
    message: String,
    task_name: Option<String>,
    path: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
) -> Result<()> {
    let parent = ensure_task_exists(store, parent_id)?;
    if task_name.is_some() && path.is_some() {
        bail!("spawn-agent accepts either --task-name or --path, not both");
    }
    let agent_path = match (task_name.as_deref(), path.as_deref()) {
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
    let child = store.create_child_session(
        parent_id,
        Path::new(&parent.cwd),
        agent_path.as_deref(),
        nickname.as_deref(),
        role.as_deref(),
    )?;
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
    append_workspace_context_event(store, &child)?;
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
    cleanup_agent_runtime_state_for_agent_subtree(store, &child_id)?;
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
    let needs_reopen = child.status == browser_use_protocol::SessionStatus::Cancelled
        || summary.status == "closed";
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
    value.len() == 12 && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
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
    let msg = store.send_agent_message(author_id, &target.session_id, message, trigger_turn)?;
    let author_path = display_agent_path_for_session(store, author_id)?;
    store.append_event(
        author_id,
        "agent.message",
        serde_json::json!({
            "id": msg.id,
            "author_session_id": msg.author_session_id,
            "target_session_id": msg.target_session_id,
            "author_path": author_path,
            "recipient_path": target.agent_path,
            "child_session_id": target.session_id,
            "content": msg.content,
            "trigger_turn": msg.trigger_turn,
        }),
    )?;
    println!("{}", msg.id);
    Ok(())
}

fn wait_agent(store: &Store, target_id: &str, targets: Vec<String>, timeout_ms: u64) -> Result<()> {
    let session = ensure_task_exists(store, target_id)?;
    if !targets.is_empty() {
        return wait_agent_targets(store, &session.id, &targets, timeout_ms);
    }
    store.append_event(
        &session.id,
        "agent.wait.started",
        serde_json::json!({
            "timeout_ms": timeout_ms,
        }),
    )?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let timed_out = loop {
        if !store.messages_for_agent(target_id)?.is_empty() {
            break false;
        }
        if started.elapsed() >= timeout {
            break true;
        }
        std::thread::sleep(
            Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
        );
    };
    let waited_ms = started.elapsed().as_millis() as u64;
    store.append_event(
        &session.id,
        "agent.wait.finished",
        serde_json::json!({
            "timed_out": timed_out,
            "waited_ms": waited_ms,
        }),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "message": if timed_out { "Wait timed out." } else { "Wait completed." },
            "timed_out": timed_out,
        }))?
    );
    Ok(())
}

fn wait_agent_targets(
    store: &Store,
    waiter_id: &str,
    targets: &[String],
    timeout_ms: u64,
) -> Result<()> {
    if let Some(invalid) = targets.iter().find(|target| !is_local_agent_id(target)) {
        bail!("invalid agent id `{invalid}`");
    }
    let target_refs = targets.iter().map(String::as_str).collect::<Vec<_>>();
    let timeout_ms = if timeout_ms == 0 {
        0
    } else {
        timeout_ms.clamp(10_000, 3_600_000)
    };
    store.append_event(
        waiter_id,
        "agent.wait.started",
        serde_json::json!({
            "targets": targets,
            "timeout_ms": timeout_ms,
        }),
    )?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let statuses = loop {
        let statuses = final_statuses_for_v1_wait(store, &target_refs)?;
        if !statuses.is_empty() {
            break statuses;
        }
        if started.elapsed() >= timeout {
            break serde_json::Map::new();
        }
        std::thread::sleep(
            Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())),
        );
    };
    let timed_out = statuses.is_empty();
    let waited_ms = started.elapsed().as_millis() as u64;
    store.append_event(
        waiter_id,
        "agent.wait.finished",
        serde_json::json!({
            "targets": targets,
            "timed_out": timed_out,
            "waited_ms": waited_ms,
            "status": statuses,
        }),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": statuses,
            "timed_out": timed_out,
        }))?
    );
    Ok(())
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
    let provider = FakeProvider::with_text("Fake dataset case completed.");
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        DirectDatasetRunner { provider },
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
    model: Option<String>,
    max_turns: usize,
    python_timeout_seconds: u64,
    config_profile: Option<&str>,
    raw_config_overrides: &[String],
) -> Result<()> {
    let (model, model_source) = resolve_cli_model_with_source(
        ProviderBackend::Codex,
        model,
        config_profile,
        raw_config_overrides,
    )?;
    let config_overrides = parse_cli_config_overrides(raw_config_overrides)?;
    let provider_id = resolved_cli_provider_id_for_backend_with_overrides(
        ProviderBackend::Codex,
        config_profile,
        &config_overrides,
    )?;
    let provider_id_source = cli_provider_id_source(&config_overrides);
    let browser_mode = dataset_browser_mode(&options);
    let mut agent_options = cli_agent_options(
        config_profile,
        raw_config_overrides,
        CollaborationModeKind::Default,
    )?;
    agent_options = if provider_id_source == RunConfigValueSource::Explicit {
        agent_options.with_model_provider_id(provider_id.clone())
    } else {
        agent_options.with_default_model_provider_id(provider_id.clone())
    };
    let run_config = ProviderRunConfig::new(ProviderBackend::Codex, model.clone())
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

fn dataset_run_anthropic(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let provider = anthropic_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        DirectDatasetRunner { provider },
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
) -> Result<()> {
    let provider = openrouter_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        DirectDatasetRunner { provider },
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
        model_provider_id: Some(config.provider.clone()),
        model_provider_id_source: RunConfigValueSource::Explicit,
        python_tool_timeout_seconds: config.python_timeout_seconds,
        python_env: dataset_python_env(run_id, case, attempt, &paths, &config),
        child_agent_runner: None,
        final_output_json_schema: None,
        final_output_json_schema_strict: true,
        model_compaction_enabled: true,
        analytics_source: Some("cli".to_string()),
        analytics_provider_kind: Some(config.provider.clone()),
        analytics_model: Some(config.model.clone()),
    };
    let mut run_error = runner
        .run_dataset_session(store, &session_id, agent_options)
        .err()
        .map(|error| format!("{error:#}"));
    if let Err(error) = cleanup_dataset_browser_session(store, &session_id) {
        let cleanup_error = format!("dataset browser cleanup failed: {error:#}");
        if let Some(run_error) = run_error.as_mut() {
            run_error.push_str("\n");
            run_error.push_str(&cleanup_error);
        } else {
            run_error = Some(cleanup_error);
        }
    }
    dataset_attempt_result(store, case, &session_id, config, attempt, run_error)
}

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
    update_parent_from_child_run(store, parent_id, &task.id, None)?;
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
    fn cli_agent_options_pass_collaboration_mode_to_core() -> Result<()> {
        let options = cli_agent_options(None, &[], CollaborationModeKind::Plan)?;

        assert_eq!(options.collaboration_mode, CollaborationModeKind::Plan);
        Ok(())
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
        send_agent_message(&store, &parent.id, "cli_child", "inspect this", false)?;

        let mail = store.messages_for_agent(&child.id)?;
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].content, "inspect this");
        let parent_events = store.events_for_session(&parent.id)?;
        let message_event = parent_events
            .iter()
            .find(|event| event.event_type == "agent.message")
            .context("agent.message")?;
        assert_eq!(message_event.payload["author_path"], "/root");
        assert_eq!(message_event.payload["recipient_path"], "/root/cli_child");
        assert_eq!(message_event.payload["child_session_id"], child.id);

        let err = send_agent_message(&store, &child.id, "root", "new task", true)
            .expect_err("root trigger turns should fail");
        assert!(err
            .to_string()
            .contains("Tasks can't be assigned to the root agent"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_wait_agent_records_wait_events_without_dumping_mail() -> Result<()> {
        let temp = unique_cli_test_dir("wait-agent-events")?;
        let state_dir = temp.join("state");
        let parent_cwd = temp.join("parent");
        std::fs::create_dir_all(&parent_cwd)?;
        let store = Store::open(&state_dir)?;
        let parent = store.create_session(None, &parent_cwd)?;

        wait_agent(&store, &parent.id, Vec::new(), 0)?;
        let events = store.events_for_session(&parent.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "agent.wait.started"));
        assert!(events.iter().any(|event| {
            event.event_type == "agent.wait.finished"
                && event.payload["timed_out"].as_bool() == Some(true)
        }));

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
    fn cli_wait_agent_targets_returns_final_statuses_like_v1() -> Result<()> {
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
        store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "complete"}),
        )?;

        wait_agent(&store, &parent.id, vec![child.id.clone()], 0)?;

        let events = store.events_for_session(&parent.id)?;
        let finished = events
            .iter()
            .rev()
            .find(|event| event.event_type == "agent.wait.finished")
            .context("agent.wait.finished")?;
        assert_eq!(finished.payload["timed_out"], false);
        assert_eq!(
            finished.payload["status"][&child.id]["completed"],
            "complete"
        );
        let err = wait_agent(&store, &parent.id, vec!["not_an_id".to_string()], 0)
            .expect_err("invalid target id should fail");
        assert!(err.to_string().contains("invalid agent id `not_an_id`"));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_child_terminal_status_queues_parent_subagent_notification_like_core() -> Result<()> {
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
        let mail = store.messages_for_agent(&parent.id)?;
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].author_session_id, child.id);
        assert_eq!(mail[0].target_session_id, parent.id);
        assert!(!mail[0].trigger_turn);
        assert!(mail[0].content.contains("<subagent_notification>"));
        assert!(mail[0]
            .content
            .contains("\"agent_path\":\"/root/cli_child\""));
        assert!(mail[0].content.contains("\"completed\":\"done\""));

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    #[test]
    fn cli_default_model_uses_config_model_like_codex() -> Result<()> {
        let overrides = vec![(
            "model".to_string(),
            toml::Value::String("configured-model".to_string()),
        )];

        assert_eq!(
            default_cli_model_for_backend_with_overrides(ProviderBackend::Codex, None, &overrides)?,
            "configured-model"
        );
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
    fn cli_model_source_treats_config_model_override_as_explicit_like_codex() -> Result<()> {
        let (model, source) = resolve_cli_model_with_source(
            ProviderBackend::Codex,
            None,
            None,
            &["model=\"configured-model\"".to_string()],
        )?;

        assert_eq!(model, "configured-model");
        assert_eq!(source, RunConfigValueSource::Explicit);

        let (model, source) = resolve_cli_model_with_source(
            ProviderBackend::Codex,
            Some("flag-model".to_string()),
            None,
            &["model=\"configured-model\"".to_string()],
        )?;

        assert_eq!(model, "flag-model");
        assert_eq!(source, RunConfigValueSource::Explicit);
        Ok(())
    }

    #[test]
    fn cli_default_provider_id_uses_config_provider_like_codex() -> Result<()> {
        let overrides = vec![(
            "model_provider".to_string(),
            toml::Value::String("corp".to_string()),
        )];

        assert_eq!(
            resolved_cli_provider_id_for_backend_with_overrides(
                ProviderBackend::Codex,
                None,
                &overrides
            )?,
            "corp"
        );
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
    fn cli_provider_source_treats_config_provider_override_as_explicit_like_codex() {
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
    fn cli_default_model_uses_active_catalog_auth_filtering_like_codex() -> Result<()> {
        let temp = unique_cli_test_dir("catalog-default")?;
        let catalog_path = temp.join("models.json");
        std::fs::write(
            &catalog_path,
            serde_json::json!({
                "models": [
                    {
                        "slug": "chatgpt-only",
                        "display_name": "ChatGPT Only",
                        "description": "ChatGPT-only default",
                        "visibility": "list",
                        "supported_in_api": false,
                        "priority": 0,
                        "supports_parallel_tool_calls": true,
                        "input_modalities": ["text"]
                    },
                    {
                        "slug": "api-supported",
                        "display_name": "API Supported",
                        "description": "API-supported default",
                        "visibility": "list",
                        "supported_in_api": true,
                        "priority": 2,
                        "supports_parallel_tool_calls": true,
                        "input_modalities": ["text"]
                    }
                ]
            })
            .to_string(),
        )?;
        let overrides = vec![
            ("model".to_string(), toml::Value::String(String::new())),
            (
                "model_catalog_json".to_string(),
                toml::Value::String(catalog_path.display().to_string()),
            ),
        ];

        assert_eq!(
            default_cli_model_for_backend_with_overrides(ProviderBackend::Codex, None, &overrides)?,
            "chatgpt-only"
        );
        assert_eq!(
            default_cli_model_for_backend_with_overrides(
                ProviderBackend::Openai,
                None,
                &overrides
            )?,
            "api-supported"
        );

        std::fs::remove_dir_all(temp)?;
        Ok(())
    }

    fn unique_cli_test_dir(name: &str) -> Result<std::path::PathBuf> {
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
}
