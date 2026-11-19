---
description: Validate skills and measure calibrated token budgets
argument-hint: "[path-to-skill-or-plugin]"
allowed-tools: [mcp__skill-foundry__skill_validate, mcp__skill-foundry__skill_budget_check, Read]
---

Validate all skills and inspect startup/body token budgets for the target path (`$ARGUMENTS`, passing the explicit project/workspace directory path rather than `.` under Codex):

1. Resolve the explicit directory path from `$ARGUMENTS` or the active workspace root.
2. Call `skill_validate` with `path` set to that explicit directory.
3. Call `skill_budget_check` with `path` set to that explicit directory.
4. Summarize any frontmatter/link errors, warnings (such as >500 line `SKILL.md` files or plugin root `CLAUDE.md`), and the startup vs per-skill token breakdown.
