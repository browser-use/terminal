# Public Launch Checklist

This is the launch gate for making this repo public and posting about it. The goal is simple: a stranger should be able to understand what this is, install it, run it, trust the repo shape, and know where the edges are.

## Must Do Before The Twitter Post

### 1. Ship a real first release

- [x] Move the public repo to `browser-use/terminal` and update every default install/update URL.
- [ ] Create the first GitHub release tag, probably `v0.1.0`.
- [ ] Confirm `.github/workflows/release.yml` publishes release assets for the supported platforms.
- [ ] Confirm the release contains working binaries plus the Python worker files.
- [ ] Run the public install command against the real release from a clean machine or clean user account:

```bash
curl -fsSL https://browser-use.com/terminal/install.sh | sh
browser-use
```

- [ ] Confirm every launch alias opens the terminal: `browser`, `browser-use`, `browser-use-terminal`, and `but`.
- [ ] Confirm installed launch aliases check for a newer release before opening the TUI and show Update now / Skip choices.

- [ ] Run update against the real release path:

```bash
browser-use-terminal update --check
browser-use-terminal update
```

- [ ] Confirm update works through launch aliases: `browser update --check`, `browser-use update --check`, and `browser-use-terminal update --check`.
- [ ] Confirm `/update` in the TUI installs the latest release and gives a clear restart message.
- [ ] Decide whether Linux arm64 is supported for launch. If yes, add it to the release workflow before posting.

### 2. Add the missing open-source files

- [x] Add `LICENSE`. `Cargo.toml` says MIT, but the actual license file is missing.
- [ ] Add `CONTRIBUTING.md` with local setup, test commands, coding expectations, and how to run the TUI verification loop.
- [ ] Add `SECURITY.md` with vulnerability reporting instructions and a clear statement about secrets/API keys.
- [ ] Add `CODE_OF_CONDUCT.md` or decide intentionally not to include one.
- [ ] Add `CHANGELOG.md` or a `RELEASES.md` that explains how versions and GitHub releases work.

### 3. Make the README launch-grade

- [ ] Rewrite the top of `README.md` around the product, not the rewrite history.
- [ ] Add a sharp one-sentence positioning line.
- [ ] Add a screenshot or GIF above the fold.
- [ ] Keep the install command extremely visible.
- [ ] Add a 60-second quickstart:

```bash
curl -fsSL https://browser-use.com/terminal/install.sh | sh
browser-use
```

- [ ] Explain provider setup in the smallest possible way: OpenAI, Claude Code, Anthropic, OpenRouter.
- [ ] Add a short "What works today" section.
- [ ] Add a short "Known limitations" section so early users know what they are trying.
- [ ] Move deep architecture and rewrite notes out of the main README flow.
- [ ] Remove or rewrite local/private references, especially:
  - personal browser-harness checkout paths
  - personal worktree paths in docs
  - rewrite-era notes that do not help first-time users

### 4. Clean the repository surface

- [x] Move or delete root-level design/prototype artifacts before going public:
  - `reagan_*.html`
  - `reagan_*.md`
  - any one-off planning files that are not part of the product story
- [x] Decide whether `spec.md` is public-facing. It currently contains private/local path references and should not ship as-is.
- [x] Make sure generated/runtime directories stay ignored:
  - `.browser-use/`
  - `.browser-use-terminal/`
  - `.venv/`
  - `target/`
  - `dist/`
  - `build/`
- [x] Remove `.DS_Store` if tracked or staged.
- [x] Run a final `rg` pass for local paths and private names:

```bash
rg -n "/Users/|/home/|Developer/browser-harness|Downloads/tmp|hackathons|sk-|api[_-]?key|secret|token" .
```

### 5. Verify security and secrets handling

- [ ] Confirm no real credentials are committed.
- [ ] Confirm `.env.example` contains only placeholders.
- [ ] Confirm `config show` redacts stored secrets.
- [ ] Confirm screenshots/GIFs do not show tokens, local usernames, private paths, or account ids.
- [ ] Confirm logs, traces, artifacts, and terminal dumps are not committed.
- [x] Decide and document telemetry policy.
  Decision: anonymous product analytics are enabled by default, fail open, and can be disabled with `BUT_TELEMETRY=0`.
- [ ] Document where local state is stored:

```text
~/.browser-use-terminal/
```

### 6. Prove the beginner path

- [ ] On a clean machine/account, install with the public install command.
- [ ] Launch `browser-use`.
- [ ] Complete first-run setup.
- [ ] Add at least one provider credential.
- [ ] Run a simple task:

```text
Open example.com and report the title.
```

- [ ] Confirm the TUI shows a readable result.
- [ ] Confirm quitting and restarting preserves state.
- [ ] Confirm `browser-use-terminal diagnostics` gives useful output.
- [ ] Confirm uninstall/manual cleanup instructions are documented.

### 7. Run the release verification suite

- [ ] Run Rust formatting:

```bash
cargo fmt --check
```

- [ ] Run Rust tests:

```bash
cargo test
```

- [ ] Run Python tests:

```bash
uv run --with pytest python -m pytest -q
```

- [ ] Run installer wrapper smoke tests:

```bash
scripts/install/test-wrappers.sh
```

- [ ] Run the full terminal UI verification:

```bash
scripts/verify-terminal-ui.sh
```

- [ ] Inspect `/tmp/but-design-loop/` after the TUI run.
- [ ] Capture one clean screenshot/GIF from a real terminal session for the README and tweet.

### 8. Decide the support promise

- [ ] State supported platforms clearly.
- [ ] State whether Windows is unsupported for now.
- [ ] State whether the project expects `curl`, `tar`, and a POSIX shell for install.
- [ ] State whether users need Chrome/Chromium installed, or whether the tool manages browser setup.
- [ ] State whether browser-use cloud is optional, required, or experimental.
- [ ] State that the project is early if the API/UX may change.

### 9. Prepare the launch post assets

- [ ] Final README screenshot/GIF.
- [ ] One short demo command.
- [ ] One sentence explaining why this exists.
- [ ] One sentence explaining what makes it different.
- [ ] Link to the repo.
- [ ] Install command in the post or first reply.
- [ ] Known caveat ready, especially if only macOS/Linux are supported.

### 10. Final pre-public GitHub settings

- [ ] Confirm the default branch is the branch referenced by the install URL.
- [ ] Enable GitHub Discussions or decide to use Issues only.
- [ ] Add issue templates for bug reports and feature requests.
- [ ] Add a PR template.
- [ ] Protect the release workflow enough that tags cannot be published accidentally.
- [ ] Confirm repository description, topics, website link, and social preview image.
- [ ] Confirm Actions permissions allow release publishing.

## Nice Immediately After Launch

- [ ] Add `brew install` support if demand appears.
- [ ] Add package-manager distribution only after the release asset path is stable.
- [ ] Add signed release artifacts or stronger checksum publishing.
- [ ] Add a formal uninstall command.
- [ ] Add a short architecture page with diagrams.
- [ ] Add a public roadmap.
- [ ] Add example tasks and small demo recordings.

## Launch Readiness Definition

Do not post until these are true:

- [ ] A clean user can install from the public command.
- [ ] The first TUI launch works without repo-local files.
- [ ] At least one real provider path works.
- [ ] The README has a compelling first screen.
- [ ] The repo has `LICENSE`, `CONTRIBUTING.md`, and `SECURITY.md`.
- [ ] No private paths, secrets, generated artifacts, or internal-only planning files are visible in the public surface.
- [ ] The release workflow has produced real assets.
- [ ] Full verification passes.
