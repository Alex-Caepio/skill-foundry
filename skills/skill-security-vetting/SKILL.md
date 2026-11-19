---
name: skill-security-vetting
description: Use this skill when auditing third-party skills, plugins, or MCP toolpacks for security risks, unpinned downloads, data egress, hidden secrets, auto-update behavior, naming collisions, or validating structured vetting dossiers against 8 binary and 4 scored gates.
license: Apache-2.0
---

# Skill Security & Risk Vetting

Evaluate untrusted or third-party skills and plugins before adopting them into a workspace.

## 8 Binary + 4 Scored Gate Model

1. Run `skill_vet` (`skill-foundry vet-skill <path>`) against a skill folder or plugin directory (pass `--existing <comma-list>` or `existing_skills` to check name collisions).
2. Inspect the 8 binary gates (`license`, `data_egress`, `pinning`, `uninstall`, `collisions`, `auto_update`, `maintenance_ownership`, `measurability`):
   - `fail` on embedded tokens/secrets, piped shell installers, unpinned `@latest` package tags, unpinned `pip` packages, destructive shell commands, implicit update scripts, or skill name collisions.
   - `unknown` on missing `LICENSE` or unreviewed external URLs (`resolves_by` explains what human verification is needed).
3. Inspect the 4 scored gates (`0..5`): `automation_depth`, `token_efficiency`, `integration_coverage`, `trust_signals`.
4. To validate an authored Kit-style JSON review dossier, pass `dossier` (`skill-foundry vet-skill --dossier <entry.json>`).
