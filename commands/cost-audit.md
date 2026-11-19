---
description: Audit Claude Code & Codex BTE token usage by project and skill
argument-hint: "[--host all|claude|codex] [--project substr] [--since-days N]"
allowed-tools: [mcp__skill-foundry__skill_cost_audit]
---

Audit local session logs (`$ARGUMENTS`):

1. Call `skill_cost_audit` with requested filters (defaulting to `host: "all"`).
2. Summarize total BTE spend, Claude vs Codex split, main vs subagent share, and the top projects, skills, MCP servers, and tools consuming tokens.
