---
name: skill-authoring
description: Use this skill when creating a new Agent Skill, validating SKILL.md frontmatter and internal links, linting startup and body token budgets, or packaging a skill into a portable .skill zip archive.
license: Apache-2.0
---

# Skill Authoring, Validation & Token Budgeting

Use `skill-foundry` MCP tools (`skill_scaffold`, `skill_validate`, `skill_budget_check`, `skill_package`) or CLI commands to build crisp, low-token Agent Skills for both Claude Code and Codex.

## Workflow

1. **Scaffold**: Call `skill_scaffold` (`skill-foundry init-skill <name> --path <dir>`) to generate `SKILL.md` plus starter `scripts/`, `references/`, and `assets/` folders.
2. **Keep startup surface small**: Put always-loaded trigger cues in `description:` (under 1024 chars, no `<` or `>`), keep `SKILL.md` under 500 lines, and move heavy schemas or API examples to `references/`.
3. **Validate**: Call `skill_validate` (`skill-foundry validate <path>`) to check YAML frontmatter, kebab-case naming, line limits, and relative `references/`/`scripts/`/`assets/` links.
4. **Budget check**: Call `skill_budget_check` (`skill-foundry budget-check <path>`) to measure calibrated `cl100k` startup tokens (`AGENTS.md`, descriptors, commands, agents) and optional JSON ceilings.
5. **Package**: Call `skill_package` (`skill-foundry package <skill-dir> --out <dir>`) to emit a clean `.skill` zip archive excluding `evals/` and temporary files.
