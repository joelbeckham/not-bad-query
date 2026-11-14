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

The T3 Claude review delegate, including fresh rename and application-icon reviews, failed
before inspecting the screenshots:

> Your organization has disabled Claude subscription access for Claude Code · Use an Anthropic API key instead, or ask your admin to enable access

No Claude critique or approval has been obtained. Once access is restored, start
a new read-only review delegate as described in `../../AGENTS.md`, supply the four
changed-interface screenshots above, and apply substantive findings before final
visual sign-off. The `before` suffix records their pre-Claude-review status.
