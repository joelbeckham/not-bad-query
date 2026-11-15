# Architecture

## Goals

Snappy, memory-efficient, keyboard-driven, minimal. Results of any size (10GB+) must
not grow memory, and a UI action must never wait on the network.

## Shape

The application/executable is `not-bad-query`; Rust crates and internal namespaces
use `nbq-*` / `nbq_*`. Development environment variables use the `NBQ_` prefix.
User settings live in the OS application-data directory for `not-bad-query`, and
credentials use the `not-bad-query` OS credential-store service. Projects are
identified by `not-bad-query.toml`, with local state under `.not-bad-query/`.

The application icon is an adaptation of Lucide's ISC-licensed `database-zap`.
Windows builds embed the multi-size ICO as resource 1 (loaded by GPUI for the
native window class and taskbar) and its license as resource 2. The custom title
bar renders the same design from embedded PNG bytes. Assets and regeneration
instructions are in `crates/nbq-app/assets/`.

```
nbq-app (GPUI views)  ──commands──▶  nbq-engine (tokio runtime)
        ▲                               │  one actor per tab: owns its DB session
        └────────── Event channel ──────┘  one fetch task per result: stream → spool
        │
        └── loads bounded row/column previews on a background executor
```

* **Commands** are non-blocking methods on `Engine` (`execute`, `cancel`, `ensure_rows`,
  `commit`, `rollback`, `export_csv`, ...). **Events** (`StatementStarted`,
  `ResultOpened`, `StatementFinished`, `Fetch`, `Txn`, `Conn`, `QueryDone`, `Export`)
  flow back on an unbounded channel the UI drains.
* Each **tab owns its own session**, so transactions are per tab.
* A sidebar profile has **Connect / Disconnect** actions and a status icon:
  disconnected, connecting, connected, or connection error. Status combines the
  object-browser session and all tabs using that profile; a failed session stays
  visible even if another is connected. Status is the last observed connection
  state, updated on connection attempts and database operations.
  Connect opens the browser session and retries disconnected/failed tabs.
  Disconnect releases all of the profile's sessions and cancels pending connects,
  retaining scripts, their selected profiles, and already-fetched result rows.
  Running queries and open transactions must be resolved first. Running SQL again
  reconnects the tab. Metadata retirement cancels queued/in-flight introspection
  and suppresses stale events so disconnected caches cannot reappear.

## Results: the spool

Drivers produce Arrow `RecordBatch`es. A fetch task appends each batch to a temp file
as an LZ4-compressed Arrow IPC stream and records `(offset, len, first_row)` in an
in-memory index. The grid asks `spool.read(rows)` for its visible window and gets
zero-copy slices of the covering batches. Decoded caches are limited to 16 MiB per
result and 64 MiB across an engine. The UI loads and formats previews off the render
thread (at most 32 columns and approximately 4 MiB of preview text per window).
Formatted previews use aligned 32-row by 16-column tiles, capped at approximately
4 MiB and 64 entries. Only the visible viewport schedules background preview IO;
off-screen layout measurements (including GPUI's row-0 height probe) read the
cache without changing demand. Neighboring tiles coexist, and appending immutable
spool rows retains already-rendered cells while a partial tile is extended.
Full-cell copying also reads on a background executor. Appends/compression run on
Tokio's blocking pool rather than its session workers.

Spools reject decoded batches over 32 MiB and cap each index at 262,144 entries.
A shared 20 GiB budget covers compressed spool bytes; leases last until the final
result/export owner drops. These limits do not cap total process memory: active
decodes, driver protocol buffers, preview windows and index allocations are additional.
The PostgreSQL driver targets 4 MiB batches, permits a final larger row (up to 16 MiB),
and rejects values over 8 MiB or results wider than 1,024 columns. The PostgreSQL
protocol necessarily receives an individual row before its value limits can be checked.

Measured (`nbq-results/tests/memory.rs`): 289 MiB of row data → 33 MiB on disk, 26 MiB peak tracked heap
while scrolling randomly and exporting everything.

## Fetch policy: pause on the server

A result opens with `initial_rows` (500) fetched. After that the fetch task stops
polling the driver stream until the grid asks for more (`ensure_rows` + lookahead).
For Postgres an unpolled `tokio_postgres::RowStream` makes the connection stop reading
the socket, so the backend blocks in `ClientWrite`: the result is paused **on the server**
with no cursor and no transaction (verified in `nbq-driver/tests/pg.rs`).

* Statements that write (incl. `UPDATE ... RETURNING`) and all but the last statement
  of a script are always drained to completion.
* **Export** sets demand to "everything" and streams the spool to a same-directory
  temporary CSV while it fills. Only a complete result is published, and an existing
  destination is never overwritten. Failure/cancel removes the temporary file.
  The export is the exact result the user saw, never a re-run.
* Closing a paused result (new query, stop, commit) sends a server-side cancel.

## Transactions

* Auto-commit is the per-connection default, toggleable per tab.
* Manual mode issues `BEGIN` lazily before the first statement. On Postgres every
  statement in a transaction is wrapped in `SAVEPOINT nbq_sp`: an error rolls back to the
  savepoint so the transaction survives (no "current transaction is aborted"), and a
  paused result can be cancelled without losing pending changes.
* Redshift has no savepoints: there, closing a paused result inside a transaction drains
  it instead of cancelling.
* The tab shows pending writes and transaction age (`Event::Txn`).
* The UI requires a query to finish and an open transaction to be resolved before
  changing connections or commit mode. Closing a window with unsaved scripts or
  pending work asks before discarding it. Closing a tab cancels execution before
  its session actor shuts down and rolls back.

## TLS and scripts

New connections default to `verify-full`: certificate-chain and hostname validation
use OS trust and optional public PEM roots. AWS public RDS roots supplement OS trust
for AWS RDS/Redshift endpoint names. `disable`, `prefer`, and `require` remain readable
for existing profiles; the latter two retain encryption-only semantics and the
connection dialog identifies their lack of certificate verification. A custom CA
bundle is portable connection metadata and must contain no private keys.

Script reads and saves run on a background executor, with an 8 MiB script limit.
An editor retains the opened/saved text revision. Saving rechecks that revision
before atomic replacement, including each Windows sharing-violation retry. Save As
creates a new file without clobbering an existing destination. Edits made during a
save remain dirty after the saved snapshot is published.

## Cancel

`CancelHandle` is used outside the actor queue (the actor may be awaiting the server).
Postgres/Redshift use the protocol cancel request on a side connection; the statement is
verifiably gone from `pg_stat_activity` (tests).

## Value rendering

Postgres returns binary values; `pg/decode.rs` renders every type (numeric, temporal,
intervals, arrays, ranges, records, enums, domains, inet, ...) to text identical to the
server's output function — checked by an oracle test against `format('%s', x)`.
Ints/floats/bools/bytea stay typed in Arrow; everything else is stored as display text.

## Projects

A project is a folder, often a subfolder of a git repo (`repo/queries/`). The layout is fixed:

```
not-bad-query.toml                    id + name (marks the folder as a project)
connections/<folder>/<Name>.toml   one file per connection; name and folder come from the path
scripts/...                        SQL files
exports/                           CSV export default (git-ignored)
.not-bad-query/local.toml             per-user state, e.g. script -> last connection (git-ignored)
```

- Connection files hold no user or password. Credentials are in the OS credential store under
  `<project id>/<connection id>`. Ids are stable across renames; copied or hand-written files
  without a unique id get one written back on load.
- Several projects can be open in one window (`workspace.toml` in the app config dir lists them).
  A tab belongs to one project: its script and its connection both come from it.
- `Project` keys (`u32`) are per process, and `Connection::key()` is `<project key>:<id>`. Engine
  caches, catalogs and tree ids use that key, so two copies of a project never collide.
- A `notify` watcher records changed paths; the workbench polls it, reloads projects whose
  `connections/` or `scripts/` changed, and reconnects tabs whose connection settings changed.

## Testing

| Layer | How |
|---|---|
| `nbq-sql` | table tests + proptests (never panics, ranges valid) |
| `nbq-driver` | real Postgres: type rendering oracle, pause (`ClientWrite`), cancel, errors |
| `nbq-results` | windows across batch boundaries, live export, CSV quoting, bounded memory |
| `nbq-engine` | headless E2E: scroll/fetch, export, cancel, txn commit/rollback, savepoint recovery, reconnect |
| `nbq-app` | Helper/completion unit tests; native screenshots reviewed by Claude via `AGENTS.md` |
