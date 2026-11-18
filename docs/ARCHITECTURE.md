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
  The status bar uses this same combined profile state. Its tooltip reports the
  active query session separately, which may have expired while the object
  browser remains connected. Metadata state changes invalidate both displays.
  Connect opens the browser session and retries disconnected/failed tabs.
  Disconnect releases all of the profile's sessions and cancels pending connects,
  retaining scripts, their selected profiles, and already-fetched result rows.
  Disconnect asks before cancelling running queries and rolling back open
  transactions. Running SQL again
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
The grid waits until the viewport reaches the last 100 fetched rows before
requesting another batch (20% for initial batches smaller than 500 rows). Its
500-row demand margin and the engine's 2,000-row lookahead are separate from
that trigger, so displaying the first page does not immediately fetch more.
For Postgres an unpolled `tokio_postgres::RowStream` makes the connection stop reading
the socket, so the backend blocks in `ClientWrite`: the result is paused **on the server**
without an explicit cursor. Even in auto-commit, an unfinished SELECT can retain
its implicit statement transaction until completion or cancellation.

* Statements that write (incl. `UPDATE ... RETURNING`) and all but the last statement
  of a script are always drained to completion.
* **Export** sets demand to "everything" and streams the spool to a same-directory
  temporary CSV while it fills. Only a complete result is published, and an existing
  destination is never overwritten. Failure/cancel removes the temporary file.
  The export is the exact result the user saw, never a re-run.
  The completed-export notification links the filename to the system's default
  application. The T3 **Open exports folder** command opens `test-project/exports`
  in Explorer; `dev/open-exports.ps1 -ProjectPath <project>` opens another project's
  exports directory, creating it if needed.
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
* Open transactions expire after **15 minutes without database activity**.
  Configure `transaction_idle_timeout_secs = 900` in the app's `settings.toml`
  (restart to apply; values below one second are clamped to one second). Running
  statements and active result fetching are not idle; new statements and actual
  fetch demand reset the timer. Editing SQL or reading cached rows does not.
  An abandoned paused auto-commit result also expires, releasing its implicit
  statement transaction while keeping already-fetched rows available.
* PostgreSQL sessions set `idle_in_transaction_session_timeout` to this limit,
  preserving a shorter existing server/role/database limit. This is per session;
  no role, database or cluster defaults are changed. The server closes the expired
  connection and rolls back its transaction, including if the app is no longer
  responsive. `transaction_timeout` (PostgreSQL 17+) limits total transaction age,
  including active queries; it is deliberately not set for this inactivity policy.
  See [PostgreSQL timeout documentation](https://www.postgresql.org/docs/current/runtime-config-client.html).
* An engine timer also closes abandoned paused results (which can leave the
  server in `ClientWrite`, outside the server's idle-transaction timeout), and
  supplies protection for Redshift. It cancels a terminal result, attempts bounded
  rollback, closes the owning socket, clears transaction state and notifies the UI.
  Async connection errors are observed locally without heartbeat SQL, so polling
  cannot keep an idle transaction alive. Fatal timeout SQLSTATEs `25P03` / `25P04`
  invalidate the session. Reconnection does not replay discarded writes.
* Disconnect with pending work asks for **Disconnect and roll back**, then stops
  execution and releases all sessions for that profile. Confirmed window close
  waits for engine cleanup, including already-closing tabs and metadata sessions,
  before removing the window. Per-session cancellation/socket deadlines prevent
  a broken connection from hanging shutdown. Each PostgreSQL session owns its
  socket task, so a retained result stream cannot keep its transaction alive after
  session drop. PostgreSQL itself rolls back any open transaction when a session
  terminates ([protocol documentation](https://www.postgresql.org/docs/current/protocol-flow.html)).

## MySQL

Use a **MySQL** connection profile (`kind = "mysql"`, default port 3306).
The initial database is optional. Credentials use the same OS credential store
as the other server drivers. **Verify full** checks the certificate chain and
server name against system and public roots, with optional imported PEM roots.

The driver uses the MySQL text protocol to preserve exact numeric values,
including unsigned integers and decimals, and streams bounded Arrow batches
on demand. It displays the first result set of a statement and consumes any
additional sets before the session is reused. **Stop** uses `KILL QUERY` on a
separate connection authenticated as the same user. MySQL's implicit commits,
such as for ordinary DDL, update the tab's transaction state. The object tree
and autocomplete use a separate metadata connection to browse databases,
tables, views, columns, indexes, and triggers.

Integration tests use MySQL 8.4 via `dev/docker-compose.yml` on loopback port
33306. Start the `mysql` Compose service to use it in the app.

## File-backed databases

SQLite profiles use `kind = "sqlite"` and `file_path = "data/local.db"`.
DuckDB profiles use `kind = "duckdb"` and `file_path = "data/local.duckdb"`.
Relative paths resolve against the owning project's root, independently of the
connection file's folder or the app's working directory. The shared file-database
dialog offers **Open existing** and **Create new**, with native file pickers.
Creation happens explicitly on Save and atomically reserves a new filename;
opening and reconnecting never create missing files. File-based engines do not use the
credential store or server/TLS fields.

`DbKind::is_file_based`, `file_extensions`, `ConnectParams::file`, and the engine's
`prepare_database_file` share the file workflow between SQLite and DuckDB.
Each SQLite session owns a dedicated blocking worker, including its connection
and prepared statements. Arrow batches are requested on demand, bounded by the
same value/row/batch limits as server drivers. SQLite's dynamic storage classes
are rendered into stable Arrow text columns, preserving integers, floats, text,
NULLs, and hexadecimal blobs even when a column mixes types. Metadata queries
use SQLite's schema and pragma interfaces for tables, views, columns, indexes,
triggers, and autocomplete. Interrupt handles support cancellation; savepoints
preserve manual transactions after ordinary errors, and the engine reconciles
transaction state if SQLite rolls back the entire transaction after an error.

DuckDB also uses one blocking worker per session, with native Arrow streaming.
A process-wide weak cache shares each canonical file's database instance across
independent connections for tabs and metadata; the instance closes after its last
session closes. DuckDB's working-memory limit is 512 MiB per database instance,
with its normal disk spill behavior. Result batches honor fetch row limits and
the driver byte limits, and additional Arrow types (unsigned integers, decimals,
dates, lists and structs) have display formatters. Database creation initializes
a file in a same-directory staging folder and publishes it without overwriting
an existing destination. The browser lists schemas, tables, views, columns,
primary keys, indexes, sequences and user macros.

DuckDB has no savepoints. A paused result can be stopped without draining it or
discarding pending writes. Running queries use DuckDB's interrupt handle. An
execution error that aborts a manual transaction triggers rollback and clears the
tab's transaction state; errors that leave the transaction usable preserve it.

## CSV snapshots

CSV files in `scripts/` appear alongside SQL files. Double-click imports a new
snapshot or opens a query on the linked table. The link icon selects that table;
its refresh variant indicates a changed or missing source. The context menu
provides import, reload, and table navigation. The Import CSV command and desktop
file drops use the same dialog. Multiple files are reviewed in sequence, sharing
the destination chosen for the first file; dropping onto a file connection
preselects that database. Windows shell integration is not installed.

The dialog defaults to DuckDB, offers SQLite, detects the delimiter, previews
sample values, and allows header, column-name, and TEXT/BIGINT/DOUBLE overrides.
Leading-zero identifiers infer as text; empty fields import as NULL. New database
files go under `connections/`. Existing databases receive a new table without
overwriting a name collision.

The engine reads CSV rows on a blocking worker and sends bounded batches through
a channel to a dedicated session. Import and reload run in a transaction. Reload
deletes and replaces only the imported table's rows, retaining its schema and
indexes; parse, type, header, and cancellation failures roll back the operation.
The original CSV is never changed. Reload replaces any subsequent edits to the
imported rows. It never runs automatically.

Import recipes live in `.not-bad-query/local.toml`: source and destination paths
(project-relative where possible), engine, table, parsing settings, column types,
original headers, import time, and source size/modification time. Recipe writes
and source-change checks run through the engine off the UI thread.

SQLite (`.db`, `.sqlite`, `.sqlite3`) and DuckDB (`.db`, `.duckdb`, `.ddb`) files
under `connections/` are discovered recursively. File headers distinguish engines
when extensions overlap; active connections retain their known kind. Generated
connection ids are persisted in local state. Explicit profiles referencing the
same canonical file take precedence, avoiding duplicate tree entries.

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
| `nbq-driver` | real Postgres: type rendering oracle, pause (`ClientWrite`), cancel, errors; SQLite/DuckDB: file creation, values, cancel, transaction isolation and rollback |
| `nbq-results` | windows across batch boundaries, live export, CSV quoting, bounded memory |
| `nbq-engine` | headless E2E: scroll/fetch, export, cancel, txn commit/rollback, savepoint recovery, reconnect |
| `nbq-app` | Helper/completion unit tests; native screenshots reviewed by Claude via `AGENTS.md` |
