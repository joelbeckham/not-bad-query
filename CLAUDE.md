# good-query

A fast, minimal, keyboard-driven SQL client (a light DBeaver alternative) in Rust + GPUI.
See `docs/ARCHITECTURE.md` for the design.

## Layout

| Crate | Role |
|---|---|
| `gq-sql` | Pure text utils: statement splitting, statement-at-cursor, statement classification. No IO. |
| `gq-driver` | `Session`/`RowStream`/`CancelHandle` traits; Postgres/Redshift driver (`pg/`); Arrow cell display. |
| `gq-results` | Disk spool of Arrow batches (bounded memory), windowed reads, CSV export. |
| `gq-engine` | Headless app core: per-tab session actors, fetch-on-demand, transactions, cancel. Commands in, `Event`s out. |
| `gq-core` | Projects (folder of connection files + scripts, file watcher), app settings/workspace TOML, OS keyring credentials. |
| `gq-app` | GPUI UI only (`good-query` binary). Never touches the DB or files directly; goes through `gq-engine`. |
| `gq-testkit` | Test fixtures (dev Postgres). |

## Commands

Rust lives in `~/.cargo/bin`; in Git Bash prefix with `export PATH="$HOME/.cargo/bin:$PATH";`.

- All tests: `cargo test --workspace` (needs Docker; integration tests start `dev/docker-compose.yml`).
- Memory test meaningful in release: `cargo test -p gq-results --release -- --nocapture`.
- Run the app: `cargo run -p gq-app` (the dev Postgres on port 55432 has a `shop` demo schema, 1M orders).
- Dev DB by hand: `docker compose -f dev/docker-compose.yml up -d --wait`.

## Rules

- If Docker isn't running, stop and ask the user to start it. Never skip or stub out DB tests.
- UI code must not block: all DB/IO goes through `gq-engine`; the UI only reads spools (cheap, cached).
- Keep `gq-sql` pure and panic-free (proptests guard it).
- gpui-kit is pre-1.0: never guess an API — check the source in `~/.cargo/registry/src/*/gpui-component-0.9.1`
  (and `gpui-pre-0.3.12`) before using it.

## Driving the real app (visual checks)

GPUI's headless screenshots don't work on Windows, so drive the real window:

- `dev/restart.sh`: kill, rebuild and launch with a throwaway config in `target/dev-config` (git-ignored)
  that opens `dev/sample-project` (set `GQ_CONFIG_DIR` to use another). Debug builds read `GQ_DEV_USER`/`GQ_DEV_PASSWORD` (default `postgres`)
  when the OS credential store has no entry, so tests never touch Credential Manager.
- Keep test data in the repo (`dev/sample-project`, or `target/` for throwaway files), never in temp dirs.
- `powershell -File dev/keys.ps1 -Keys "select 1^{ENTER}"`: SendKeys syntax (`^` Ctrl, `%` Alt, `+` Shift).
  `~` means Enter in SendKeys, so avoid tilde paths (8.3 short names like `JOEL~1`).
- `powershell -File dev/click.ps1 -X 60 -Y 95 [-Double] [-Right]`: window-relative pixels.
- `powershell -File dev/screenshot.ps1 -Out shot.png`, then read the PNG.
- Don't move the mouse/keyboard focus while a scripted run is in progress (ask the user).
