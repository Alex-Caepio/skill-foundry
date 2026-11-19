---
description: Audit a skill or plugin directory against 8+4 security gates
argument-hint: "[path-to-skill-or-plugin]"
allowed-tools: [mcp__skill-foundry__skill_vet, Read]
---

Audit the target skill or plugin directory (`$ARGUMENTS`, passing the explicit project/workspace directory path rather than `.` under Codex) for security and quality risks:

1. Resolve the explicit target directory path from `$ARGUMENTS` or the active workspace root.
2. Call `skill_vet` with `path` set to that explicit directory.
3. Report overall `status` (`clear`, `open_questions`, or `known_risks`), highlight every `fail`/`unknown` binary gate and its `resolves_by` remediation step, and list the 4 quantitative gate scores.
