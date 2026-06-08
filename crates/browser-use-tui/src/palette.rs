#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    NewTask,
    PreviousWork,
    ChangeBrowser,
    DefaultProfile,
    Context,
    Goal,
    ChooseModel,
    Authenticate,
    SyncCookies,
    ManageSecrets,
    ImportPasswords,
    ManageDomains,
    ConfigureEmail,
    Reload,
    Update,
    Exit,
    Feedback,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PaletteItem {
    pub(crate) command: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: PaletteAction,
}

const VISIBLE_ITEMS: [PaletteItem; 13] = [
    PaletteItem {
        command: "/task",
        description: "start a new task",
        action: PaletteAction::NewTask,
    },
    PaletteItem {
        command: "/history",
        description: "browse previous tasks",
        action: PaletteAction::PreviousWork,
    },
    PaletteItem {
        command: "/browser",
        description: "change browser backend",
        action: PaletteAction::ChangeBrowser,
    },
    PaletteItem {
        command: "/profile",
        description: "set default Chrome profile",
        action: PaletteAction::DefaultProfile,
    },
    PaletteItem {
        command: "/context",
        description: "inspect context window attribution",
        action: PaletteAction::Context,
    },
    PaletteItem {
        command: "/model",
        description: "choose model and provider",
        action: PaletteAction::ChooseModel,
    },
    PaletteItem {
        command: "/goal",
        description: "set or view the goal for a long-running task",
        action: PaletteAction::Goal,
    },
    PaletteItem {
        command: "/sync-cookies",
        description: "sync local cookies",
        action: PaletteAction::SyncCookies,
    },
    PaletteItem {
        command: "/secrets",
        description: "save passwords & 2FA for logins",
        action: PaletteAction::ManageSecrets,
    },
    PaletteItem {
        command: "/import-passwords",
        description: "import logins from 1Password",
        action: PaletteAction::ImportPasswords,
    },
    PaletteItem {
        command: "/domains",
        description: "allow/block which sites the agent can visit",
        action: PaletteAction::ManageDomains,
    },
    PaletteItem {
        command: "/email",
        description: "give the agent a disposable inbox for sign-ups, links & codes",
        action: PaletteAction::ConfigureEmail,
    },
    PaletteItem {
        command: "/feedback",
        description: "report a bug or share feedback",
        action: PaletteAction::Feedback,
    },
];

const HIDDEN_ITEMS: [PaletteItem; 4] = [
    PaletteItem {
        command: "/auth",
        description: "sign in to a provider",
        action: PaletteAction::Authenticate,
    },
    PaletteItem {
        command: "/reload",
        description: "restart the UI in this terminal",
        action: PaletteAction::Reload,
    },
    PaletteItem {
        command: "/update",
        description: "install the latest release",
        action: PaletteAction::Update,
    },
    PaletteItem {
        command: "/exit",
        description: "quit browser-use terminal",
        action: PaletteAction::Exit,
    },
];

pub(crate) const fn max_item_count() -> usize {
    VISIBLE_ITEMS.len()
}

pub(crate) fn items_filtered(filter: &str) -> Vec<PaletteItem> {
    let trimmed = filter.trim_start_matches('/').to_ascii_lowercase();
    if trimmed.is_empty() {
        return VISIBLE_ITEMS.to_vec();
    }
    VISIBLE_ITEMS
        .iter()
        .copied()
        .chain(HIDDEN_ITEMS.iter().copied())
        .filter(|item| item.command[1..].to_ascii_lowercase().contains(&trimmed))
        .collect()
}

pub(crate) fn selected_action(filter: &str, selected_row: usize) -> Option<PaletteAction> {
    items_filtered(filter)
        .get(selected_row)
        .map(|item| item.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_is_available_as_hidden_command() {
        assert_eq!(selected_action("/reload", 0), Some(PaletteAction::Reload));
    }

    #[test]
    fn sync_cookies_is_available_from_short_filter() {
        assert_eq!(
            selected_action("/sync", 0),
            Some(PaletteAction::SyncCookies)
        );
    }

    #[test]
    fn goal_is_available_from_palette() {
        assert_eq!(selected_action("/goal", 0), Some(PaletteAction::Goal));
    }

    #[test]
    fn secrets_and_domains_are_visible_commands() {
        // Both appear in the default (unfiltered) palette so users discover them.
        let defaults = items_filtered("");
        assert!(defaults.iter().any(|item| item.command == "/secrets"));
        assert!(defaults.iter().any(|item| item.command == "/domains"));
        // And resolve from a short filter.
        assert_eq!(
            selected_action("/sec", 0),
            Some(PaletteAction::ManageSecrets)
        );
        assert_eq!(
            selected_action("/dom", 0),
            Some(PaletteAction::ManageDomains)
        );
    }

    #[test]
    fn context_is_available_from_palette() {
        assert_eq!(selected_action("/context", 0), Some(PaletteAction::Context));
    }

    #[test]
    fn profile_is_available_from_palette() {
        assert_eq!(
            selected_action("/profile", 0),
            Some(PaletteAction::DefaultProfile)
        );
    }
}
