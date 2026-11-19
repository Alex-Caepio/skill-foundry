---
name: session-cost-audit
description: Use this skill when analyzing Claude Code or Codex CLI session token consumption, cache efficiency, subagent vs main cost split, or BTE attribution by project, skill, MCP server, tool, or plugin.
license: Apache-2.0
---

# Dual-Host Session Cost & Attribution Audit

Measure billed token equivalent (BTE) usage across `~/.claude/projects/**/*.jsonl` and `~/.codex/sessions/**/*.jsonl`.

## BTE Weights & Method

- `input`: `1.0x` (for Codex `total_token_usage`, `cached_input_tokens` is subtracted from `input_tokens` so cached tokens are not double-counted)
- `cache_write_5m`: `1.25x`, `cache_write_1h`: `2.0x`
- `cache_read`: `0.1x`
- `output`: `5.0x`
- Claude streaming chunks are deduplicated by `message.id` (retaining final usage snapshot per call) and attributed across `skill`, `mcp`, `tool`, `plugin`, and `agent` dimensions as well as `main` vs `subagent` (`isSidechain`).

## Usage

- MCP: call `skill_cost_audit` with optional `host` (`all|claude|codex`), `project`, and `since_days`.
- CLI: `skill-foundry cost-audit --host all --since-days 7`
