//! Tests for the agent-crate `prompts` module: a de-brand guard plus shape /
//! selector / interaction-skills checks.

use super::*;

/// Every model-facing prompt const, paired with a human label for assertions.
fn model_facing_prompts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("BASE_SYSTEM_PROMPT", BASE_SYSTEM_PROMPT),
        ("BROWSER_TOOL_DESCRIPTION", BROWSER_TOOL_DESCRIPTION),
        (
            "BROWSER_SCRIPT_TOOL_DESCRIPTION",
            BROWSER_SCRIPT_TOOL_DESCRIPTION,
        ),
        ("BROWSER_CONNECTION_GUIDANCE", BROWSER_CONNECTION_GUIDANCE),
        ("COLLABORATION_MODE_DEFAULT", COLLABORATION_MODE_DEFAULT),
        ("COMPACTED_CONTEXT_SYSTEM", COMPACTED_CONTEXT_SYSTEM),
        ("HELPER_SESSION_IDENTITY", HELPER_SESSION_IDENTITY),
        (
            "HELPER_SESSION_INHERITED_CONTEXT",
            HELPER_SESSION_INHERITED_CONTEXT,
        ),
        ("REVIEW_PROMPT", REVIEW_PROMPT),
    ]
}

/// De-brand guard: no model-facing prompt const may leak `codex` / `chatgpt`
/// brand strings (case-insensitive). The ported content is already browser-use
/// branded; this guards against regressions.
#[test]
fn model_facing_prompts_have_no_codex_or_chatgpt_brand() {
    for (label, content) in model_facing_prompts() {
        let lower = content.to_ascii_lowercase();
        assert!(
            !lower.contains("codex"),
            "model-facing prompt `{label}` leaked the `codex` brand string"
        );
        assert!(
            !lower.contains("chatgpt"),
            "model-facing prompt `{label}` leaked the `chatgpt` brand string"
        );
    }
}

/// Every model-facing prompt const is non-empty (the `include_str!` paths
/// resolve to real, populated assets).
#[test]
fn model_facing_prompts_are_non_empty() {
    for (label, content) in model_facing_prompts() {
        assert!(
            !content.trim().is_empty(),
            "model-facing prompt `{label}` is empty"
        );
    }
}

/// The base system prompt carries the recognizable browser-use preamble marker
/// and `system_prompt()` returns it trimmed.
#[test]
fn system_prompt_has_browser_use_preamble() {
    let prompt = system_prompt();
    assert!(
        prompt.contains("browser-use agent"),
        "base system prompt is missing the browser-use preamble marker"
    );
    // `system_prompt()` is the trimmed asset, matching the legacy provider
    // preamble assembly.
    assert_eq!(prompt, BASE_SYSTEM_PROMPT.trim());
    assert!(!prompt.starts_with(char::is_whitespace));
    assert!(!prompt.ends_with(char::is_whitespace));
}

#[test]
fn browser_agent_system_prompt_loads_main_interaction_skills() {
    let prompt = browser_agent_system_prompt();
    assert!(prompt.starts_with(system_prompt()));
    assert!(prompt.contains("Loaded Browser-Harness Interaction Skills"));
    assert!(prompt.contains("interaction-skills/screenshots.md"));
    assert!(prompt.contains("interaction-skills/profile-sync.md"));
    assert_eq!(browser_harness_interaction_skills().len(), 17);
}

#[test]
fn browser_mode_instruction_matches_main_local_connection_guidance() {
    let prompt = browser_mode_instruction("local");
    assert!(prompt.contains("Selected browser mode: Local Chrome via raw browser-harness"));
    assert!(prompt.contains("Use normal browser-harness page helpers"));
    assert!(prompt.contains("browser_profiles()"));
    assert!(prompt.contains("browser_use_profile(id)"));
    assert!(prompt.contains("Do not use old `browser connect local` commands"));
}

#[test]
fn browser_mode_instruction_guides_remote_cdp_to_direct_page_work() {
    let prompt = browser_mode_instruction("remote-cdp");
    assert!(prompt.contains("Selected browser mode: externally provided CDP/browser context"));
    assert!(prompt.contains("Use raw browser-harness helpers directly"));
    assert!(prompt.contains("call `browser(id)` before page helpers"));
    assert!(prompt.contains("follow browser-harness setup errors"));
}

#[test]
fn system_prompt_points_to_raw_browser_harness() {
    assert!(BASE_SYSTEM_PROMPT.contains("raw browser-harness"));
    assert!(BASE_SYSTEM_PROMPT.contains("Use `browser_script`"));
    assert!(BASE_SYSTEM_PROMPT.contains("Browser-harness owns all of that"));
    assert!(BASE_SYSTEM_PROMPT.contains("Do not use old Rust browser commands"));
}

#[test]
fn system_prompt_requires_explicit_managed_browser_ids() {
    assert!(BASE_SYSTEM_PROMPT.contains("Managed browsers have short explicit ids"));
    assert!(BASE_SYSTEM_PROMPT.contains("browser_new(\"private\")"));
    assert!(BASE_SYSTEM_PROMPT.contains("browser_new(\"cloud\")"));
    assert!(BASE_SYSTEM_PROMPT.contains("call `browser(id)` before page helpers"));
    assert!(BASE_SYSTEM_PROMPT.contains("Do not rely"));
    assert!(BASE_SYSTEM_PROMPT.contains("on a current browser across separate tool calls"));
}

#[test]
fn prompts_prefer_raw_harness_page_helpers() {
    assert!(BASE_SYSTEM_PROMPT.contains("Screenshots are the"));
    assert!(BASE_SYSTEM_PROMPT.contains("default way"));
    assert!(BASE_SYSTEM_PROMPT.contains("capture_screenshot()"));
    assert!(BASE_SYSTEM_PROMPT.contains("click_at_xy(x, y)"));
    assert!(BASE_SYSTEM_PROMPT.contains("js(...)"));
    assert!(BASE_SYSTEM_PROMPT.contains("cdp(\"Domain.method\""));

    let script = browser_script_tool_description();
    assert!(script.contains("Screenshots are the default way"));
    assert!(script.contains("capture_screenshot()"));
    assert!(script.contains("click_at_xy(x, y)"));
    assert!(script.contains("js(...)"));
    assert!(script.contains("cdp(\"Domain.method\""));
}

#[test]
fn browser_script_prompt_guides_raw_browser_harness_lifecycle() {
    let script = browser_script_tool_description();

    assert!(script.contains("browser_new(\"private\")"));
    assert!(script.contains("browser_new(\"cloud\")"));
    assert!(script.contains("browser(id)"));
    assert!(script.contains("browser_list()"));
    assert!(script.contains("browser_profiles()"));
    assert!(script.contains("browser_use_profile(profile_id)"));
    assert!(script.contains("First navigation is `new_tab(url)`, not `goto_url(url)`"));
}

#[test]
fn dataset_prompt_uses_raw_browser_harness_and_budgeted_finalization() {
    let prompt = include_str!("../../../../prompts/dataset-case-user.md");

    assert!(prompt.contains("Use `browser_script` for browser work"));
    assert!(
        prompt.contains("Browser-harness owns connection, launch, profiles, cloud, and lifecycle")
    );
    assert!(prompt.contains("When the turn budget is nearly exhausted"));
    assert!(prompt
        .contains("Return the final answer with the done tool only when the task is complete"));
}

/// Plan mode was removed. The compatibility enum value now renders the Default
/// asset so stale configs do not re-enable planning behavior.
#[test]
fn deprecated_plan_mode_renders_default_asset() {
    let default = collaboration_mode_prompt(CollaborationModeKind::Default);
    let plan = collaboration_mode_prompt(CollaborationModeKind::Plan);

    assert_eq!(default, plan, "plan mode must resolve to default");

    for rendered in [&default, &plan] {
        assert!(rendered.starts_with(COLLABORATION_MODE_OPEN_TAG));
        assert!(rendered.ends_with(COLLABORATION_MODE_CLOSE_TAG));
        // The placeholder must have been substituted.
        assert!(!rendered.contains("{{KNOWN_MODE_NAMES}}"));
    }

    assert!(default.contains("Collaboration Mode: Default"));
    assert!(default.contains(KNOWN_MODE_NAMES));
    assert!(!default.contains("Plan Mode"));
}

/// The browser tool descriptions preserve their interaction-skills content,
/// including the control-plane / data-plane split and the screenshot / image
/// (view-image) workflow notes that drive page interaction.
#[test]
fn browser_tool_descriptions_preserve_interaction_skills() {
    // Compatibility/status tool description.
    let browser = browser_tool_description();
    assert!(
        browser.contains("raw browser-harness MVP"),
        "browser tool description lost its raw-harness framing"
    );
    assert!(
        browser.contains("old Rust browser control plane") && browser.contains("disabled"),
        "browser tool description lost its legacy-control-plane warning"
    );

    // Page-interaction tool description, including the screenshot/image
    // interaction skills that back view-image workflows.
    let script = browser_script_tool_description();
    assert!(
        script.contains("browser-harness"),
        "browser_script description lost its raw browser-harness framing"
    );
    let script_lower = script.to_ascii_lowercase();
    assert!(
        script_lower.contains("screenshot"),
        "browser_script description lost its screenshot/image interaction skill"
    );
    assert!(
        script.contains("js(...)"),
        "browser_script description lost js argument helper guidance"
    );
    assert!(
        script.contains("browser_new(\"private\")")
            && script.contains("browser_new(\"cloud\")")
            && script.contains("browser(id)"),
        "browser_script description lost managed-browser id guidance"
    );

    // The base system prompt enumerates the page-interaction helpers, including
    // the screenshot/image helpers used for visual inspection.
    assert!(
        BASE_SYSTEM_PROMPT.contains("capture_screenshot")
            && BASE_SYSTEM_PROMPT.contains("click_at_xy")
            && BASE_SYSTEM_PROMPT.contains("js(...)")
            && BASE_SYSTEM_PROMPT.contains("cdp(\"Domain.method\""),
        "base system prompt lost its screenshot/image interaction helpers"
    );
}

/// The connection interaction-skills guidance preserves the tab-visibility
/// workflow content.
#[test]
fn connection_guidance_preserves_tab_visibility() {
    assert!(
        BROWSER_CONNECTION_GUIDANCE.contains("ensure_real_tab"),
        "connection guidance lost its tab-visibility workflow"
    );
}

/// `render_prompt_template` trims the template and applies replacements in
/// order, mirroring the legacy helper.
#[test]
fn render_prompt_template_trims_and_substitutes() {
    let rendered = render_prompt_template("  hello {{name}}  ", &[("{{name}}", "browser-use")]);
    assert_eq!(rendered, "hello browser-use");
}

/// `compacted_context_system_message` renders both placeholders from the
/// compacted-context asset.
#[test]
fn compacted_context_message_renders_placeholders() {
    let context = serde_json::json!({ "step": 1, "note": "resume" });
    let rendered = compacted_context_system_message(&context, "the active contract").unwrap();

    assert!(!rendered.contains("{{browser_agent_contract}}"));
    assert!(!rendered.contains("{{context_json}}"));
    assert!(rendered.contains("the active contract"));
    assert!(rendered.contains("\"step\": 1"));
    assert!(rendered.contains("\"note\": \"resume\""));
}
