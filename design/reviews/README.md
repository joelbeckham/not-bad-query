# Native review evidence — 2026-11-13

Screenshots captured from the debug Windows application with an isolated config
and the local sample PostgreSQL project. Input messages targeted the preview's
exact process/window; no global keyboard or mouse input was used.

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

## Claude review status: pending access

The T3 Claude review delegate failed before inspecting the screenshots:

> Your organization has disabled Claude subscription access for Claude Code · Use an Anthropic API key instead, or ask your admin to enable access

No Claude critique or approval has been obtained. Once access is restored, start
a new read-only review delegate as described in `../../AGENTS.md`, supply the four
changed-interface screenshots above, and apply substantive findings before final
visual sign-off. The `before` suffix records their pre-Claude-review status.
