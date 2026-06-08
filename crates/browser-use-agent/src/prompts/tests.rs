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
    assert!(prompt.contains("interaction-skills/forms.md"));
    assert!(prompt.contains("interaction-skills/screenshots.md"));
    assert!(prompt.contains("interaction-skills/profile-sync.md"));
    assert_eq!(browser_harness_interaction_skills().len(), 18);
}

#[test]
fn browser_mode_instruction_matches_main_local_connection_guidance() {
    let prompt = browser_mode_instruction("local");
    assert!(prompt.contains("Selected browser mode: Local Chrome"));
    assert!(prompt.contains("Use `browser connect local` before page work"));
    assert!(prompt.contains("browser local setup"));
    assert!(prompt.contains("no default profile is set"));
    assert!(prompt.contains("browser profile use <profile-id>"));
    assert!(prompt.contains("/profile"));
}

#[test]
fn browser_mode_instruction_guides_remote_cdp_to_direct_page_work() {
    let prompt = browser_mode_instruction("remote-cdp");
    assert!(prompt.contains("Selected browser mode: Remote CDP"));
    assert!(prompt.contains("already provides the browser endpoint"));
    assert!(prompt.contains("already open at the start URL"));
    assert!(prompt.contains("first inspect the current page"));
    assert!(prompt.contains("trust its `navigation_ready` page_info result"));
    assert!(prompt.contains("Do not call `browser connect managed`"));
}

#[test]
fn system_prompt_bounds_multi_item_collection_loops() {
    assert!(BASE_SYSTEM_PROMPT.contains("Multi-item collection rule"));
    assert!(BASE_SYSTEM_PROMPT.contains("maintain a checklist"));
    assert!(BASE_SYSTEM_PROMPT.contains("Do not keep varying one search term"));
    assert!(BASE_SYSTEM_PROMPT.contains("audit the checklist"));
}

#[test]
fn system_prompt_commits_single_site_collection_to_one_domain() {
    assert!(BASE_SYSTEM_PROMPT.contains("Single-site collection rule"));
    assert!(BASE_SYSTEM_PROMPT.contains("choose one viable domain early"));
    assert!(BASE_SYSTEM_PROMPT.contains("do not keep searching for a perfect domain"));
    assert!(BASE_SYSTEM_PROMPT.contains("Do not stitch rows from multiple domains"));
    assert!(BASE_SYSTEM_PROMPT.contains("mark it unavailable for that domain"));
}

#[test]
fn prompts_avoid_screenshots_for_text_heavy_extraction() {
    assert!(BASE_SYSTEM_PROMPT.contains(
        "For text-heavy research, document reading, search, pricing, tables, and list extraction"
    ));
    assert!(BASE_SYSTEM_PROMPT.contains("screenshots add latency"));
    assert!(BASE_SYSTEM_PROMPT.contains("If you have three or more independent URLs"));

    let script = browser_script_tool_description();
    assert!(script.contains(
        "For text-heavy research, document reading, search, pricing, tables, and list extraction"
    ));
    assert!(script.contains("screenshots add latency"));
    assert!(script.contains("navigation_ready"));
    assert!(script.contains("trust it and inspect/extract from the current page"));
}

#[test]
fn browser_script_prompt_guides_model_controlled_email_and_stable_selectors() {
    let script = browser_script_tool_description();

    assert!(script.contains("fill_input(selector, text, clear=True, timeout=3)"));
    assert!(script.contains("current_datetime()"));
    assert!(script.contains("email_inbox(limit=20, sent_after=None)"));
    assert!(script.contains("email_inbox(sent_after=...)"));
    assert!(script.contains("avoid brittle positional selectors"));
    assert!(script.contains("input:nth-of-type(2)"));
    assert!(script.contains("compare `timestamp`/`message_id` yourself"));
    assert!(script.contains("verify the message `timestamp`"));
}

#[test]
fn dataset_prompt_enforces_timeboxed_finalization() {
    let prompt = include_str!("../../../../prompts/dataset-case-user.md");

    assert!(prompt.contains("Timebox contract"));
    assert!(prompt.contains("soft deadline"));
    assert!(prompt.contains("hard deadline"));
    assert!(prompt.contains("Never keep running until the external runner timeout"));
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
    // Control-plane tool description.
    let browser = browser_tool_description();
    assert!(
        browser.contains("Browser runtime control tool"),
        "browser tool description lost its control-plane heading"
    );

    // Data-plane / page-interaction tool description, including the
    // screenshot/image interaction skills that back view-image workflows.
    let script = browser_script_tool_description();
    assert!(
        script.contains("browser interaction tool"),
        "browser_script description lost its interaction-tool framing"
    );
    let script_lower = script.to_ascii_lowercase();
    assert!(
        script_lower.contains("screenshot"),
        "browser_script description lost its screenshot/image interaction skill"
    );
    assert!(
        script.contains("js(function_source, *args)"),
        "browser_script description lost js argument helper guidance"
    );
    assert!(
        script.contains("http_get_many(urls, **kwargs)")
            && script.contains("browser_fetch_many(requests, **kwargs)"),
        "browser_script description lost batch/direct fetch helper guidance"
    );
    assert!(
        script.contains("Batch recipe after discovering stable links or endpoints")
            && script.contains("responses = http_get_many(urls, timeout=12, max_workers=8)")
            && script.contains("Fetched ${$.ok_count}/${$.total} independent URLs"),
        "browser_script description lost its concrete batch-fetch adoption recipe"
    );

    // The base system prompt enumerates the page-interaction helpers, including
    // the screenshot/image helpers used for visual inspection.
    assert!(
        BASE_SYSTEM_PROMPT.contains("capture_screenshot")
            && BASE_SYSTEM_PROMPT.contains("emit_image")
            && BASE_SYSTEM_PROMPT.contains("js(function_source, *args)")
            && BASE_SYSTEM_PROMPT.contains("http_get_many")
            && BASE_SYSTEM_PROMPT.contains("browser_fetch_many"),
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
