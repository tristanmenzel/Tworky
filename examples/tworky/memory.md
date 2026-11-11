# Tworky shared memory

## Running the app

- Start it with `tw service start app`. It runs on this worker's Vite port, with its own home (`Tworky-dev/workers/<worker id>` under the local data folder) and its own workers under `<workers root>-dev/<worker id>`. The installed app's workers and state are untouched.
- The first build in a worktree is a full cargo build with its own `target/`, so expect several minutes.
- Run `node scripts/build-cli.mjs` before the first cargo build, because `tauri-build` checks that the bundled `tw` exists. The setup hook does this.

## Learned
