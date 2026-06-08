//! Base system prompt + model-facing prompt assets for the async agent engine.
//!
//! This module is the agent crate's home for the **model-visible** prompt
//! content the engine sends to the provider: the browser-agent base system
//! prompt, the browser tool descriptions, the collaboration-mode developer
//! instructions, the compacted-context system message, the helper-session
//! prompts, and the review prompt. The content itself lives in the repo-root
//! `prompts/` directory (shared with the legacy `browser-use-core` engine) and
//! is already browser-use branded; this module `include_str!`s those assets and
//! exposes them as `pub const`s plus accessor functions.
//!
//! The accessors mirror the legacy `browser-use-core::prompts` API
//! (`crates/browser-use-core/src/prompts.rs`) so the cutover from the sync
//! engine to this async engine is a drop-in swap:
//! - [`collaboration_mode_prompt`] ↔ legacy `collaboration_mode_instructions`
//! - [`compacted_context_system_message`] ↔ legacy `compacted_context_system_message`
//! - [`render_prompt_template`] ↔ legacy `render_prompt_template`
//!
//! The base system prompt ([`BASE_SYSTEM_PROMPT`] / [`system_prompt`]) mirrors
//! the legacy provider preamble assembly in
//! `crates/browser-use-providers/src/lib.rs` (it `push_str`s the trimmed
//! `browser-agent-system.md` asset).
//!
//! Branding: every model-facing const here is browser-use branded. The
//! [`tests`] module includes a de-brand guard that asserts no `codex`/`chatgpt`
//! brand string leaks into any model-facing prompt const.

/// Selecting a built-in collaboration mode toggles the developer instructions
/// the agent runs under.
///
/// Mirrors `browser-use-core::CollaborationModeKind`
/// (`crates/browser-use-core/src/lib.rs:298`) so the two engines agree on the
/// mode→asset mapping during cutover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollaborationModeKind {
    /// Default execution-oriented behavior.
    Default,
    /// Deprecated compatibility alias for [`Self::Default`].
    Plan,
}

/// Opening tag wrapping a rendered collaboration-mode block.
///
/// Mirrors `browser-use-core` `COLLABORATION_MODE_OPEN_TAG`
/// (`crates/browser-use-core/src/constants.rs:121`).
pub const COLLABORATION_MODE_OPEN_TAG: &str = "<collaboration_mode>";

/// Closing tag wrapping a rendered collaboration-mode block.
///
/// Mirrors `browser-use-core` `COLLABORATION_MODE_CLOSE_TAG`
/// (`crates/browser-use-core/src/constants.rs:122`).
pub const COLLABORATION_MODE_CLOSE_TAG: &str = "</collaboration_mode>";

/// The human-readable list of known collaboration mode names, substituted for
/// the `{{KNOWN_MODE_NAMES}}` placeholder in the collaboration-mode assets.
///
/// Mirrors the legacy replacement value in
/// `crates/browser-use-core/src/prompts.rs:48`.
pub const KNOWN_MODE_NAMES: &str = "Default";

/// The browser-agent base system prompt (the BROWSER_AGENT preamble / base
/// instructions sent to the model).
///
/// Sourced from `prompts/browser-agent-system.md`. This is the asset the legacy
/// provider preamble builder loads
/// (`crates/browser-use-providers/src/lib.rs:4874`).
pub const BASE_SYSTEM_PROMPT: &str = include_str!("../../../../prompts/browser-agent-system.md");

/// The `browser` runtime-control tool description (the control-plane CLI tool).
///
/// Sourced from `prompts/browser-tool-description.md`
/// (legacy `include_str!`s at `crates/browser-use-core/src/tools/mod.rs:1606`
/// and `crates/browser-use-browser/src/lib.rs:4609`).
pub const BROWSER_TOOL_DESCRIPTION: &str =
    include_str!("../../../../prompts/browser-tool-description.md");

/// The `browser_script` page-interaction tool description (the data-plane tool).
///
/// Sourced from `prompts/browser-script-tool-description.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/tools/mod.rs:1630`).
pub const BROWSER_SCRIPT_TOOL_DESCRIPTION: &str =
    include_str!("../../../../prompts/browser-script-tool-description.md");

/// Browser connection / tab-visibility interaction guidance (the
/// execute/connection guidance bundled with the browser interaction skills).
///
/// Sourced from `prompts/interaction-skills/connection.md`
/// (legacy `include_str!` at `crates/browser-use-providers/src/lib.rs:5095`).
pub const BROWSER_CONNECTION_GUIDANCE: &str =
    include_str!("../../../../prompts/interaction-skills/connection.md");

/// Default collaboration-mode developer instructions (raw asset, before
/// `{{KNOWN_MODE_NAMES}}` substitution and tag wrapping).
///
/// Sourced from `prompts/collaboration-mode-default.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/prompts.rs:44`).
pub const COLLABORATION_MODE_DEFAULT: &str =
    include_str!("../../../../prompts/collaboration-mode-default.md");

/// The compacted-context system prompt template (re-establishes the operating
/// contract after context compaction).
///
/// Sourced from `prompts/compacted-context-system.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/prompts.rs:58`).
/// Contains the `{{browser_agent_contract}}` and `{{context_json}}`
/// placeholders rendered by [`compacted_context_system_message`].
pub const COMPACTED_CONTEXT_SYSTEM: &str =
    include_str!("../../../../prompts/compacted-context-system.md");

/// The helper-session identity prompt template.
///
/// Sourced from `prompts/helper-session-identity.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/lib.rs:9206`).
/// Contains `{{role}}`, `{{canonical_task_sentence}}`, and
/// `{{explorer_instruction}}` placeholders rendered by the caller.
pub const HELPER_SESSION_IDENTITY: &str =
    include_str!("../../../../prompts/helper-session-identity.md");

/// The helper-session inherited-context prompt template.
///
/// Sourced from `prompts/helper-session-inherited-context.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/lib.rs:9220`).
/// Contains the `{{context}}` placeholder rendered by the caller.
pub const HELPER_SESSION_INHERITED_CONTEXT: &str =
    include_str!("../../../../prompts/helper-session-inherited-context.md");

/// The review-mode system prompt.
///
/// Sourced from `prompts/review-prompt.md`
/// (legacy `include_str!` at `crates/browser-use-core/src/review.rs:56`).
pub const REVIEW_PROMPT: &str = include_str!("../../../../prompts/review-prompt.md");

/// Returns the browser-agent base system prompt (trimmed).
///
/// Mirrors the legacy provider preamble assembly in
/// `crates/browser-use-providers/src/lib.rs:4874`, which `push_str`s
/// `browser-agent-system.md` trimmed. The collaboration-mode block is appended
/// by the caller via [`collaboration_mode_prompt`], not here.
pub fn system_prompt() -> &'static str {
    BASE_SYSTEM_PROMPT.trim()
}

/// Builds the browser-agent system prompt with the browser-harness interaction
/// skills appended, matching main's provider instruction assembly.
pub fn browser_agent_system_prompt() -> String {
    let mut instructions = String::from(system_prompt());
    instructions.push_str("\n\n## Loaded Browser-Harness Interaction Skills");
    instructions.push_str(
        "\n\nThese are the same interaction-skill playbooks from browser-harness. Apply the relevant section when the page mechanic appears.",
    );
    for (path, content) in browser_harness_interaction_skills() {
        instructions.push_str("\n\n### ");
        instructions.push_str(path);
        instructions.push_str("\n\n");
        instructions.push_str(content.trim());
    }
    instructions
}

/// Build the per-run browser-mode instruction inserted ahead of the task.
///
/// Mirrors `browser-use-core::browser_mode_instruction` so a selected terminal
/// browser mode gives the model the same first action on the Rust engine.
pub fn browser_mode_instruction(mode: &str) -> String {
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => concat!(
            "Selected browser mode: Local Chrome. Use `browser connect local` before page work. ",
            "This attaches to a local Chromium-family browser exposing CDP.\n\n",
            "Local Chrome requires a default profile before the first connect. Run `browser connect local`; if it reports `status: \"needs-user-action\"` because no default profile is set, show its `user_prompt` exactly and wait for the user's choice. Then run `browser profile use <profile-id>` and retry `browser connect local`. Use that one default profile setting for local Chrome.\n\n",
            "After `browser connect local`, the runtime's `browser connect local` / `browser status --json` output is the source of truth. If it reports `status: \"needs-user-action\"` with multiple reachable `candidates`, ask the user which candidate/browser/profile to attach, then run `browser connect local --candidate <id>`.\n\n",
            "If a browser/search/page task is blocked by Local Chrome connection, setup, permission, profile targeting, or disconnected status, STOP browser work and handle that blocker. Do NOT answer from memory, cached knowledge, or general web knowledge as a substitute for using Chrome. Follow the tool's `next_step` / `model_instruction`, ask the user for the required Chrome action if needed, then retry browser work.\n\n",
            "When the connection is blocked, route by the right fields — there are two different `state` values in the JSON and they mean different things:\n",
            "  • Each candidate inside `candidates[]` has its own `state` label (e.g. `stale-port`, `cdp-disabled`, `reachable`). IGNORE that label for routing — different labels can map to the same fix. Look at the candidate's `browser_running` and `remote_debugging_enabled` fields instead; those are the source of truth for cases A and B.\n",
            "  • The top-level response of a connect ATTEMPT (especially `browser connect local --candidate <id>`) has its own `state` and `raw_error`. Those DO matter — they reflect what Chrome actually said when you tried to attach. Case C below routes on this top-level state (`permission-blocked` / HTTP 403) because Chrome's response to the attempt, not a pre-attempt guess from disk, is what tells you the per-session popup is in play.\n",
            "Each fix changes Chrome's state, so re-run `browser connect local` to see what's next. Problems chain (Chrome not running → launch → retry → may now be fully connectable, OR may now need permission setup → handle whichever comes next).\n\n",
            "`browser local profiles`, `browser profile suggest`, `browser profile use`, `browser local open`, and `browser local setup` are safe to run before connecting; they inspect, update, open/focus local profile state, or guide setup and do not themselves attach to a page.\n\n",
            "Routing (re-evaluate after every retry):\n\n",
            "  A) `browser_running: false` (Chrome process not running; can coexist with any value of `remote_debugging_enabled` — the field reflects the on-disk Local State even when the process is down):\n",
            "      1. If the connect response says no default profile is set, ask the user which profile to set as default, mention `/profile`, run `browser profile use <profile-id>`, then retry `browser connect local`.\n",
            "      2. If a default profile is already set, plain `browser connect local` may open/focus that saved profile before attaching when an already-reachable multi-profile Chrome endpoint would otherwise be ambiguous. If the selected profile target is missing, do not continue in an arbitrary existing Chrome profile; follow the returned `next_step`.\n",
            "      3. If that retry returns `permission-blocked` / HTTP 403, the remote-debugging checkbox is already enabled and Chrome is showing the per-session popup. Follow case C. DO NOT run `browser local setup` and DO NOT tell the user to tick the checkbox again.\n\n",
            "  B) `browser_running: true` AND `remote_debugging_enabled: false` (only this exact combination needs the chrome://inspect dance — DO NOT enter this branch if `remote_debugging_enabled` is `true` or missing):\n",
            "      1. If no default profile is set, run `browser connect local` first and follow its profile-selection preflight. If a default profile is set, proceed silently.\n",
            "      2. Run `browser local setup` to fetch the canonical URL (`chrome://inspect/#remote-debugging`) and step list. Then ALWAYS use the `shell` tool to open that URL for the user — DO NOT ask them to type chrome://inspect themselves. macOS: `open -a \"Google Chrome\" \"chrome://inspect/#remote-debugging\"` (Apple Events route chrome:// URLs; passing the URL as a plain CLI arg to the Chrome binary silently opens a blank tab on macOS — use `open -a`, not the binary). Linux: `google-chrome chrome://inspect/#remote-debugging`. Windows: `cmd /c start chrome chrome://inspect/#remote-debugging`. Adjust the app/binary if the user runs Edge/Brave/Canary. Only fall back to asking the user as a last resort if the shell command errors.\n",
            "      3. Tell the user to tick 'Allow remote debugging for this browser instance' on the page you just opened, and reply when done. STOP — do NOT retry yet, and do NOT mention any Chrome popup. There is no popup at this stage.\n",
            "      4. After they confirm, in the SAME chat message, warn them BEFORE you retry: Chrome is about to pop up asking 'Allow remote debugging?' and they should click Allow on it. THEN call `browser connect local` again in that same response. The user has to see the heads-up first so the popup doesn't blindside them.\n\n",
            "  C) Candidate fields say `browser_running: true` AND `remote_debugging_enabled: true` BUT the latest fresh `browser connect local` ATTEMPT's top-level response is `state: \"permission-blocked\"` (or `raw_error` mentions HTTP 403 / Forbidden). Routing this case from the attempt's state is correct — the candidate fields alone can't tell you Chrome will reject the WebSocket; you only find out by trying. Do NOT infer this case from old chat history, old `browser status --json`, or a stale `last_issue`; if Chrome may have been closed/reopened or the user says no popup is visible, run `browser status --json` / `browser connect local` again and follow the latest result. The on-disk toggle is already on; Chrome is showing (or has queued) its per-session \"Allow remote debugging?\" popup and waiting on the user. DO NOT touch chrome://inspect, DO NOT tell the user to tick the checkbox — it's already ticked, that'd be confusing. Instead:\n",
            "      1. If the latest tool result includes `profile_recovery_command`, run it first to open/focus the selected Chrome profile. Then immediately run `browser connect local` again; that fresh connection attempt is what triggers Chrome's visible \"Allow remote debugging?\" popup.\n",
            "      2. If that fresh retry still returns `permission-blocked`, tell the user: \"Chrome is showing an 'Allow remote debugging?' popup somewhere — check each open Chrome window and click Allow. Reply when done.\" Mention that with multiple profile windows the popup may appear in a window they aren't looking at.\n",
            "      3. After they confirm, retry `browser connect local`. If it still 403s, suggest they bring each Chrome window to the front in turn until they spot the popup, or close all but one Chrome window and try again.\n\n",
            "If the first connection succeeds, continue with page work in the connected browser. If the connected browser is clearly the wrong account/session, stop and ask the user to change the default profile with `/profile` before continuing.\n\n",
            "When the user chooses a default local profile in chat, run `browser profile use <profile-id>` and tell them they can change it anytime with `/profile`.\n\n",
            "Never describe a terminal modal or button to click."
        )
        .to_string(),
        "headless" | "headless-chromium" | "managed-headless" => concat!(
            "Selected browser mode: Headless Chromium. Use `browser connect managed --headless` before page work. ",
            "This starts a Rust-owned managed browser with an isolated automation profile."
        )
        .to_string(),
        "managed" | "managed-headed" => concat!(
            "Selected browser mode: managed headed browser. Use `browser connect managed --headed` before page work. ",
            "This starts a Rust-owned visible browser with an isolated automation profile."
        )
        .to_string(),
        "cloud" | "browser-use-cloud" => concat!(
            "Selected browser mode: Browser Use cloud. Use `browser connect` before page work. ",
            "Plain `browser connect` uses the remembered cloud profile when one is set. ",
            "Remote start means start and connect; use `browser remote live-url` to retrieve the watch URL."
        )
        .to_string(),
        "remote-cdp" | "cdp" => concat!(
            "Selected browser mode: Remote CDP. The evaluation harness already provides the browser endpoint. ",
            "Do not call `browser connect managed`, `browser connect local`, or `browser remote start`. ",
            "If the task says the browser session is already open at the start URL, first inspect the current page with `browser_script` using `page_info()` and extraction/screenshot as needed; do not navigate to that same URL again unless the current page URL is wrong. ",
            "Otherwise start page work directly with `browser_script` using `new_tab(url)` for first navigation, or `goto_url(url)` only when the current tab should be reused; trust its `navigation_ready` page_info result when the URL is correct, then inspect/extract instead of repeating the same navigation. ",
            "Use `browser status --json` only if you need to inspect the current connection."
        )
        .to_string(),
        other => format!(
            "Selected browser mode: {other}. Use `browser status --json` first, then choose an explicit browser connect command."
        ),
    }
}

/// The full browser-harness interaction-skill bundle loaded by main.
pub fn browser_harness_interaction_skills() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "interaction-skills/connection.md",
            include_str!("../../../../prompts/interaction-skills/connection.md"),
        ),
        (
            "interaction-skills/cookies.md",
            include_str!("../../../../prompts/interaction-skills/cookies.md"),
        ),
        (
            "interaction-skills/cross-origin-iframes.md",
            include_str!("../../../../prompts/interaction-skills/cross-origin-iframes.md"),
        ),
        (
            "interaction-skills/dialogs.md",
            include_str!("../../../../prompts/interaction-skills/dialogs.md"),
        ),
        (
            "interaction-skills/downloads.md",
            include_str!("../../../../prompts/interaction-skills/downloads.md"),
        ),
        (
            "interaction-skills/drag-and-drop.md",
            include_str!("../../../../prompts/interaction-skills/drag-and-drop.md"),
        ),
        (
            "interaction-skills/dropdowns.md",
            include_str!("../../../../prompts/interaction-skills/dropdowns.md"),
        ),
        (
            "interaction-skills/forms.md",
            include_str!("../../../../prompts/interaction-skills/forms.md"),
        ),
        (
            "interaction-skills/iframes.md",
            include_str!("../../../../prompts/interaction-skills/iframes.md"),
        ),
        (
            "interaction-skills/network-requests.md",
            include_str!("../../../../prompts/interaction-skills/network-requests.md"),
        ),
        (
            "interaction-skills/print-as-pdf.md",
            include_str!("../../../../prompts/interaction-skills/print-as-pdf.md"),
        ),
        (
            "interaction-skills/profile-sync.md",
            include_str!("../../../../prompts/interaction-skills/profile-sync.md"),
        ),
        (
            "interaction-skills/screenshots.md",
            include_str!("../../../../prompts/interaction-skills/screenshots.md"),
        ),
        (
            "interaction-skills/scrolling.md",
            include_str!("../../../../prompts/interaction-skills/scrolling.md"),
        ),
        (
            "interaction-skills/shadow-dom.md",
            include_str!("../../../../prompts/interaction-skills/shadow-dom.md"),
        ),
        (
            "interaction-skills/tabs.md",
            include_str!("../../../../prompts/interaction-skills/tabs.md"),
        ),
        (
            "interaction-skills/uploads.md",
            include_str!("../../../../prompts/interaction-skills/uploads.md"),
        ),
        (
            "interaction-skills/viewport.md",
            include_str!("../../../../prompts/interaction-skills/viewport.md"),
        ),
    ]
}

/// Returns the `browser` runtime-control tool description (trimmed), matching
/// the legacy tool-description loaders.
pub fn browser_tool_description() -> &'static str {
    BROWSER_TOOL_DESCRIPTION.trim()
}

/// Returns the `browser_script` page-interaction tool description (trimmed).
pub fn browser_script_tool_description() -> &'static str {
    BROWSER_SCRIPT_TOOL_DESCRIPTION.trim()
}

/// Returns the review-mode system prompt (trimmed), mirroring
/// `crates/browser-use-core/src/review.rs`.
pub fn review_prompt() -> &'static str {
    REVIEW_PROMPT.trim()
}

/// Builds the collaboration-mode developer instructions for `mode`.
///
/// Mirrors legacy `collaboration_mode_instructions`
/// (`crates/browser-use-core/src/prompts.rs:41`): selects the asset for `mode`,
/// substitutes `{{KNOWN_MODE_NAMES}}` with [`KNOWN_MODE_NAMES`], and wraps the
/// result in [`COLLABORATION_MODE_OPEN_TAG`] / [`COLLABORATION_MODE_CLOSE_TAG`].
pub fn collaboration_mode_prompt(mode: CollaborationModeKind) -> String {
    let template = match mode {
        CollaborationModeKind::Default => COLLABORATION_MODE_DEFAULT,
        CollaborationModeKind::Plan => COLLABORATION_MODE_DEFAULT,
    };
    let text = template.replace("{{KNOWN_MODE_NAMES}}", KNOWN_MODE_NAMES);
    format!("{COLLABORATION_MODE_OPEN_TAG}{text}{COLLABORATION_MODE_CLOSE_TAG}")
}

/// Renders the compacted-context system message after a context compaction.
///
/// Mirrors legacy `compacted_context_system_message`
/// (`crates/browser-use-core/src/prompts.rs:52`): pretty-prints `context` as
/// JSON and renders [`COMPACTED_CONTEXT_SYSTEM`] with the
/// `{{browser_agent_contract}}` and `{{context_json}}` placeholders.
///
/// The legacy signature returns `anyhow::Result<String>`; the only fallible
/// step is `serde_json::to_string_pretty`, so we return `serde_json::Error`
/// directly to avoid pulling `anyhow` into this crate's non-test dependencies.
pub fn compacted_context_system_message(
    context: &serde_json::Value,
    browser_agent_contract: &str,
) -> Result<String, serde_json::Error> {
    let context_json = serde_json::to_string_pretty(context)?;
    Ok(render_prompt_template(
        COMPACTED_CONTEXT_SYSTEM,
        &[
            ("{{browser_agent_contract}}", browser_agent_contract),
            ("{{context_json}}", &context_json),
        ],
    ))
}

/// Renders a prompt template by trimming it and substituting placeholders.
///
/// Mirrors legacy `render_prompt_template`
/// (`crates/browser-use-core/src/prompts.rs:78`) exactly: trims the template,
/// then applies each `(placeholder, value)` replacement in order.
pub fn render_prompt_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut rendered = template.trim().to_string();
    for (placeholder, value) in replacements {
        rendered = rendered.replace(placeholder, value);
    }
    rendered
}

#[cfg(test)]
mod tests;
