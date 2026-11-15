# Native review evidence — 2026-11-13

Screenshots captured from the debug Windows application with an isolated config
and the local sample PostgreSQL project. Input messages targeted the preview's
exact process/window; no global keyboard or mouse input was used.

All five images were recaptured after the `not-bad-query` rename. The native
window title, title bar, and quit confirmation show the current application name.
The renamed app passed all 97 workspace tests and all five native startup cases.

- `results-before.png`: a real 10,000-row PostgreSQL execution paused on demand;
  background result previews render IDs, text, numeric and timestamp values.
- `connection-before.png`: new connections default to Verify full with public CA import.
- `connection-unverified-before.png`: legacy Prefer mode identifies unverified server identity.
- `quit-before.png`: native window-close guard with unsaved SQL and a paused execution.
- `workbench-before.png`: initial project/sidebar state.

The native Keep working action preserved the app. Confirming Quit closed the
preview process. Automated checks additionally cover server cancellation on tab
close, transaction recovery, TLS hostname/chain checks, quota reclamation, safe
exports and script-save conflicts.

## Application icon

- `icon-workbench.png`: the shared database/bolt design in the native title bar.
- `windows-app-icon.png`: the actual 40px Windows window-class icon read from the
  preview process. The executable's icon group resource 1 and embedded upstream
  license resource 2 were verified directly.
- `taskbar-app-icon.png`: the actual Windows taskbar button, cropped to this app
  using its UI Automation bounds. The custom icon is visible in the taskbar.

The debug application was rebuilt; all six app tests and five native startup
regressions pass with the icon resources. Source and license provenance, DPI sizes,
and regeneration instructions are in `../../crates/nbq-app/assets/README.md`.

## Claude review status: pending access

### Result-cell clipboard focus — 2026-11-14

- `results-cell-copy.png`: selecting a result cell now explicitly focuses the
  table so its existing Ctrl+C binding reaches the CopyCell action. The SQL
  editor keeps its own copy behavior when focused.

All 11 app tests pass. Native input targeted only the isolated preview window;
Ctrl+C passed clipboard assertions for the full 492-character multiline value
(beyond the displayed preview), a numeric cell, NULL replacing previous content,
editor selection copying, and copying again after returning to the results.
The first clipboard-helper attempt hit an OLE restore error; the successful
recheck used direct Win32 clipboard reads/writes. No clipboard contents were
saved to disk or printed. The initial clipboard restore did not succeed.

A fresh read-only Claude screenshot review was attempted and failed before image
inspection with the same organization-access error below. Codex inspected the
selected-cell/focus appearance; Claude visual sign-off remains pending access.

### Tree column alignment — 2026-11-14

- `tree-columns-before.png`: reproduced uneven leaf-icon indentation on the
  sample project's `shop.order_summary` view, especially its longer timestamp type.
- `tree-columns-normal.png`, `tree-columns-narrow.png`,
  `tree-columns-minimum.png`, `tree-columns-wide.png`: the same expanded view at
  four panel widths after the fix. Chevron slots and icons retain their widths;
  column names share a fixed left edge. Secondary types yield first, and names
  ellipsize when necessary. The widest capture shows complete type descriptions.

All input, resize drags, and captures targeted the isolated development preview.
All 11 app tests pass; the preview was closed after verification. A fresh
read-only Claude design review was attempted but failed before image inspection
with the same organization-access error below. Codex inspected all four widths;
Claude visual sign-off remains pending access.

### Transaction inactivity and cleanup — 2026-11-14

- `transaction-idle-expired.png`: the local sample-project preview used a
  three-second test limit. An uncommitted INSERT RETURNING was rolled back; its
  returned row remains visible alongside the warning and persistent status text.
  The production default is 900 seconds (15 minutes).
- `transaction-disconnect-confirmation.png`: the pending-write indicator remains
  visible behind the explicit Disconnect and roll back confirmation.
- `transaction-quit-confirmation.png`: the existing quit guard with a live,
  uncommitted transaction; the window now waits for engine cleanup before exit.

Native checks verified that Keep connected preserves the transaction, confirmed
Disconnect leaves zero application sessions and zero committed test rows, and
confirmed Quit exits successfully with zero sessions and zero committed test
rows. The fixture table was removed after verification. The three-second limit
was isolated under `target/transaction-review`; user settings were not changed.

All 116 workspace tests and five startup smoke cases pass. Coverage includes
session-local PostgreSQL timeout configuration, stricter server limits, idle
rollback and reconnect, activity resets, long-running queries, paused manual and
auto-commit results, Redshift-mode terminal cancellation, retained result streams,
running-query disconnect and shutdown of tabs plus metadata sessions.

A fresh read-only Claude screenshot review failed before image inspection with
the same organization-access error below. Codex inspected these native images;
Claude visual sign-off remains pending access.

### Results scrolling stability — 2026-11-14

- `grid-scroll-before.png`: reproduced repeated `…` placeholders after scrolling
  beyond the initial fetch in the isolated sample-project debug preview.
- `grid-scroll-after.png`: stable values around rows 793–804 after the same scroll.
- `grid-scroll-later-page.png`: stable values around rows 3443–3454, after demand
  fetched a second page (4514 rows available).

The preview ran `select i as id, 'Row ' || i as label from
generate_series(1, 20000) i`. Process-targeted wheel input reproduced the old
failure. After the fix, 12 successive stationary captures of the result-cell
region were pixel-identical at each of the two later viewports. Repeated runs
also populated the first page without needing a changed visible-range callback.

Cell rendering now reads cached values only; it cannot schedule IO for GPUI's
off-screen row-0 height measurement. Preview loads follow the actual viewport,
keep adjacent row/column tiles together within a 4 MiB/64-tile budget, and retain
immutable fetched cells when more rows arrive. All 106 workspace tests pass,
including off-screen measurement, append preservation, rapid viewport changes,
horizontal/vertical tile coverage, and preview-cache bounds.

A fresh read-only Claude review was attempted for these screenshots and failed
before image inspection with the same organization-access error below. Native
screenshots were inspected by Codex; Claude sign-off remains pending access.

### Connection status and session controls — 2026-11-14

The isolated debug preview opened `dev/sample-project`. These screenshots show
the connection menu and status icon beside the environment-colored database icon:

- `connection-status-connected.png`: green check, Connected label, Disconnect enabled.
- `connection-status-disconnected.png`: gray ring, Disconnected label, Connect enabled.
- `connection-status-connecting.png`: amber loader while a local TCP fixture holds the handshake open.
- `connection-status-error.png`: red alert after that fixture closes the connection.
- `connection-status-transaction-guard.png`: Disconnect refused while the tab has an open transaction.

The connecting/error fixture was temporary and removed after capture. Native
checks verified that Run reconnects a disconnected tab, rollback permits
Disconnect, both tab and metadata backends disappear from `pg_stat_activity`, and
the editor and already-fetched results remain. All 102 workspace tests pass,
including connect cancellation, metadata retirement, paused-result disconnect,
and aggregation of mixed session states.

A new read-only Claude screenshot review was attempted through T3 Code and
failed before inspecting any image with the organization-access error below.
These screenshots have been inspected by Codex; Claude visual sign-off remains
pending access.

The T3 Claude review delegate, including fresh rename and application-icon reviews, failed
before inspecting the screenshots:

> Your organization has disabled Claude subscription access for Claude Code · Use an Anthropic API key instead, or ask your admin to enable access

No Claude critique or approval has been obtained. Once access is restored, start
a new read-only review delegate as described in `../../AGENTS.md`, supply the four
changed-interface screenshots above, and apply substantive findings before final
visual sign-off. The `before` suffix records their pre-Claude-review status.
