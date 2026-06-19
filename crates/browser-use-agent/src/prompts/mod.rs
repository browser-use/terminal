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
            "Selected browser mode: Local Chrome via raw browser-harness. ",
            "Use normal browser-harness page helpers. If setup asks for a profile, run `browser_profiles()`, ask the user which `id` to use, run `browser_use_profile(id)`, then retry page work. Do not use old `browser connect local` commands."
        )
        .to_string(),
        "cloud" | "browser-use-cloud" => concat!(
            "Selected browser mode: Browser Use cloud via raw browser-harness. ",
            "Create a browser with `browser_new(\"cloud\")`, keep the returned `id`, and call `browser(id)` before page helpers in each script. If auth is required, tell the user to run `browser-harness auth login` or complete the terminal auth flow."
        )
        .to_string(),
        "headless" | "headless-chromium" | "managed" | "managed-headed" | "managed-headless" => concat!(
            "Selected browser mode: isolated private browser via raw browser-harness. ",
            "Create it with `browser_new(\"private\")`, keep the returned `id`, and call `browser(id)` before page helpers in each script. Do not use old managed-browser connect commands."
        )
        .to_string(),
        "remote-cdp" | "cdp" => concat!(
            "Selected browser mode: externally provided CDP/browser context. ",
            "Use raw browser-harness helpers directly. If a managed browser id is provided, call `browser(id)` before page helpers; otherwise inspect with normal helpers and follow browser-harness setup errors."
        )
        .to_string(),
        other => format!(
            "Selected browser mode: {other}. Use raw browser-harness helpers; create `browser_new(\"private\")` or `browser_new(\"cloud\")` when an isolated managed browser is needed, then call `browser(id)` before page helpers."
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
