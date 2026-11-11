# Tworky

## What is Tworky

Tworky is a desktop app for Windows, macOS and Linux that runs several Claude Code sessions at once, each on its own piece of work, without them getting in each other's way.

You keep a work list of GitHub issues, pull requests and custom tasks. **Begin session** on one creates a **worker**: a folder with its own port slot, and Claude Code running in a terminal tab inside the app. From that session Claude can set up everything the work needs:

- check out the repos it needs as git worktrees on the work's branch, with config files such as `.env` pointing at the worker's ports
- start the worker's own database and other backing services, and run dependency installs and migrations
- run the project's dev servers and tests on the worker's ports
- open and comment on PRs, add subtasks, and add facts to the project's shared memory with your approval

Because every worker has shifted ports and its own compose project, any number of them can run side by side with the main checkout. The app shows each session's state (working, waiting for you, idle, ready for testing), the worker's worktrees, services and containers, and the branches and PRs for the work. On Windows, browser tabs signed in as the project's test accounts open beside the terminal, and Claude can drive them to test its change and hand it over to you. **Clean up** stops everything and removes the worktrees, branches and folder once the PRs are merged.

## Installation

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/tristanmenzel/Tworky/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/tristanmenzel/Tworky/main/scripts/install.ps1)))
```

The script downloads the latest release, installs it and starts Tworky. Where it goes:

| OS | Location |
|---|---|
| Windows | `%LOCALAPPDATA%\Tworky` (no admin needed) |
| macOS | `/Applications/Tworky.app`, or `~/Applications` if `/Applications` isn't writable |
| Linux | `~/.local/share/Tworky/Tworky.AppImage`, with a `tworky` link in `~/.local/bin` and a desktop entry |

If Tworky is already running, the script waits for you to close it before installing.

Tworky reads issues and pull requests through the [GitHub CLI](https://cli.github.com), logged in with `gh auth login`. Sessions also need [Claude Code](https://claude.com/claude-code) (`claude`), `git`, and Docker if your projects use compose stacks.

### Why there's no signing

Releases aren't code-signed. That's fine as long as you install with the script. Browsers mark downloaded files so that SmartScreen and Gatekeeper check them, and the script downloads with `curl` or `Invoke-WebRequest`, which add no such mark.

- **Downloaded in a browser instead?** Windows needs *More info › Run anyway*. On macOS, run `xattr -dr com.apple.quarantine Tworky.app`.
- **Windows Smart App Control**, if it's turned on, blocks unsigned apps however they arrive.
- **macOS Keychain.** After each update, macOS asks once more before Tworky can read the stored test-account passwords.

### Updating

A badge by your username shows **latest** or **N commits behind** the newest release. When you're behind:

1. Press **Update**. The new release downloads while you keep working.
2. Press **Restart to update**. Tworky closes, stopping any running Claude sessions (it asks first if any are open), installs the update and reopens.

If you quit instead, the update installs without reopening the app. Running the install one-liner again also updates.

A release is published for every push to `main` (`.github/workflows/release.yml`).

## Getting started

### Add your first item

1. Open **Work Items** and press **+** beside refresh.
2. Pick what to work on:
   - **From issue** searches GitHub, starting with your open assigned issues. Matches show under their epic and story.
   - **From pull request** lists your review requests, or takes any PR URL.
   - **Custom** takes a title and description, for work with no issue yet. You can link it to an issue later.
3. If no project covers the repo yet, Tworky asks whether to add it to an existing project or start a new one.

### Set up the project

A **project** tells Tworky which repos belong together and how to run them: base ports, the compose stack, setup hooks, dev servers, config templates and test accounts.

A new project starts with just its repo. Tworky then opens its **Setup** tab under **Projects**, where a Claude session finishes the config with you. It reads the repos' docs, compose files and scripts, asks about anything they leave open, proposes ports, hooks and services, and writes the config once you agree. It then tests the config on a throwaway worker, checking that each service listens on the worker's own port and talks to the worker's own stack. Reopen the Setup tab any time to change the config.

You can also make a project directly with **+** under **Projects**, or write the TOML yourself. `examples/` has example project configs: `acme` is a fictional project and `tworky` is the config for this repo.

### Run a session

1. Select the item and press **Begin session**. Claude Code opens in a terminal tab with the worker's instructions loaded.
2. Tell Claude what to do. It checks out the repos, starts the stack and runs dev servers as it needs them.
3. Follow progress from the item's **Overview**: local work (worktrees, services, containers) on one side, branches and PRs on the other. A dot on each item shows whether its session is working, waiting for you or idle.
4. When the PRs are merged, press **Clean up**.

Each project has shared **memory** (Projects › Memory): facts every session on the project reads when it starts, such as how to run the app or a known trap. Sessions propose additions and add them only after you agree.

## Configuration

Tworky works with no configuration. `tw paths` prints where config and data live. `CLAUDE.md` documents config locations, project TOML, ports, templates and the rest of the app's behaviour.

## Building from source

Prerequisites:
- Rust (stable)
- Node 24
- On Linux, `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libdbus-1-dev pkg-config`

```sh
npm ci
node scripts/build-cli.mjs   # the bundled tw CLI; tauri-build checks it exists, even for debug builds
npm run tauri dev            # on Windows, npm run dev:app stops Vite when the app closes
npm run tauri build          # installer for the current OS
```

A debug build keeps its own config and data (`Tworky-dev`), so it never touches the installed app's workers.

Checks, as run in CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run check
```
