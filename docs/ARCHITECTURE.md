# Architecture

## Goals

Snappy, memory-efficient, keyboard-driven, minimal. Results of any size (10GB+) must
not grow memory, and a UI action must never wait on the network.

## Shape

```
gq-app (GPUI views)  ──commands──▶  gq-engine (tokio runtime)
        ▲                               │  one actor per tab: owns its DB session
        └────────── Event channel ──────┘  one fetch task per result: stream → spool
        │
        └── reads row windows directly from Arc<Spool> (disk + small LRU)
```

* **Commands** are non-blocking methods on `Engine` (`execute`, `cancel`, `ensure_rows`,
  `commit`, `rollback`, `export_csv`, ...). **Events** (`StatementStarted`,
  `ResultOpened`, `StatementFinished`, `Fetch`, `Txn`, `Conn`, `QueryDone`, `Export`)
  flow back on an unbounded channel the UI drains.
* Each **tab owns its own session**, so transactions are per tab.

## Results: the spool

Drivers produce Arrow `RecordBatch`es. A fetch task appends each batch to a temp file
as an LZ4-compressed Arrow IPC stream and records `(offset, len, first_row)` in an
in-memory index. The grid asks `spool.read(rows)` for its visible window and gets
zero-copy slices of the covering batches (decoded on demand, 16-batch LRU).

Measured (`gq-results/tests/memory.rs`): 289MB of row data → 33MB on disk, 29MB peak heap
while scrolling randomly and exporting everything.

## Fetch policy: pause on the server

A result opens with `initial_rows` (500) fetched. After that the fetch task stops
polling the driver stream until the grid asks for more (`ensure_rows` + lookahead).
For Postgres an unpolled `tokio_postgres::RowStream` makes the connection stop reading
the socket, so the backend blocks in `ClientWrite`: the result is paused **on the server**
with no cursor and no transaction (verified in `gq-driver/tests/pg.rs`).

* Statements that write (incl. `UPDATE ... RETURNING`) and all but the last statement
  of a script are always drained to completion.
* **Export** sets demand to "everything" and streams the spool to CSV while it fills,
  so the export is the exact result the user saw, never a re-run.
* Closing a paused result (new query, stop, commit) sends a server-side cancel.

## Transactions

* Auto-commit is the per-connection default, toggleable per tab.
* Manual mode issues `BEGIN` lazily before the first statement. On Postgres every
  statement in a transaction is wrapped in `SAVEPOINT gq_sp`: an error rolls back to the
  savepoint so the transaction survives (no "current transaction is aborted"), and a
  paused result can be cancelled without losing pending changes.
* Redshift has no savepoints: there, closing a paused result inside a transaction drains
  it instead of cancelling.
* The tab shows pending writes and transaction age (`Event::Txn`).

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
good-query.toml                    id + name (marks the folder as a project)
connections/<folder>/<Name>.toml   one file per connection; name and folder come from the path
scripts/...                        SQL files
exports/                           CSV export default (git-ignored)
.good-query/local.toml             per-user state, e.g. script -> last connection (git-ignored)
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
| `gq-sql` | table tests + proptests (never panics, ranges valid) |
| `gq-driver` | real Postgres: type rendering oracle, pause (`ClientWrite`), cancel, errors |
| `gq-results` | windows across batch boundaries, live export, CSV quoting, bounded memory |
| `gq-engine` | headless E2E: scroll/fetch, export, cancel, txn commit/rollback, savepoint recovery, reconnect |
| `gq-app` | GPUI `TestAppContext` keystroke tests + screenshots of the running app |
