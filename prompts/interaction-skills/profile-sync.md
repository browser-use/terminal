# Profiles And Cookies

Use explicit profile commands for login-sensitive browser work. Never dump raw cookie values by default.

What is available:

- `browser local profiles --json`: list local profiles with built-in Rust filesystem discovery. No external CLI is required.
- `browser local profiles inspect <profile-id-or-name> --domains-only`: copy the selected profile to a temporary profile, inspect cookies through CDP, and show domain-level cookie summaries only.
- `browser remote profiles --json`: list Browser Use cloud profiles with ID/name/domain summary/last-used metadata.
- `browser remote start --profile-id <uuid>`: start and connect a cloud browser using an existing cloud profile.
- `browser remote start --profile-name <name>`: resolve one exact cloud profile name and start/connect it.
- `browser remote stop`: stop the Rust-owned cloud browser so billing ends and cloud profile cookie changes can persist.

Chat-driven flow:

1. If the user wants to use an existing cloud login, show cloud profiles with `browser remote profiles --json`, then run `browser remote start --profile-id <uuid>` or `browser remote start --profile-name <name>`.
2. If the user wants a real local logged-in browser, use `browser connect local`; do not sync or copy profiles unless the user asks for cloud cookie sync.
3. If cookies need to be synced, nudge the user to use `/sync-cookies`. Do not construct site-specific cookie sync commands in chat.
4. After the user syncs cookies, show cloud profiles again and use the selected profile with `browser remote start --profile-id <uuid>` or `browser remote start --profile-name <name>`.

Important limits:

- Cookie sync is user-driven through `/sync-cookies`, including any Browser Use Cloud key setup.
- Local real profiles are used by attaching to an already-open browser with `browser connect local`.
- Do not launch the user's real default Chrome profile with remote-debugging flags.
- Raw cookie values are never returned by default. Profile inspection and sync output should expose only domain/count/expiry summaries.
