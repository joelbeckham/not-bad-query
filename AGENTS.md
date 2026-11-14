# Project guidance

Read `CLAUDE.md` and `docs/ARCHITECTURE.md` for the crate layout, commands, and development rules.

## Native UI visual review

For any task that creates or materially changes GPUI user interfaces:

1. Implement the change in Rust using Codex.
2. Build and run the native Windows application with isolated development configuration.
3. Capture a screenshot of the changed interface and save it under `design/reviews/`.
4. Use T3 Code's `delegate_task` MCP tool to invoke a Claude agent:
   - Target provider: `claudeAgent`
   - Role: `review`
   - Mode: `wait`
   - Task: Critique the screenshot as a professional desktop UI designer.
5. Give Claude the screenshot path, relevant design references, and a brief description of the intended behavior.
6. Ask for concrete, prioritized improvements to spacing, alignment, typography, color, hierarchy, and visual consistency.
7. Implement the substantive improvements with Codex.
8. Capture a new screenshot and perform one more review if necessary. Each recheck uses a new `delegate_task`.

Claude is a read-only design critic and must not modify application code.
Use the development sample project for screenshots. Target the preview process explicitly when capturing or sending input; never send test input to other applications.

This workflow is adapted from `../not-bad-jira/AGENTS.md`.
