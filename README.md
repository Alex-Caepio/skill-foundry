# skill-foundry

Standalone Rust engineering workbench for Agent Skills on **Claude Code** and **Codex CLI**.

- Standalone repository: <https://github.com/Alex-Caepio/skill-foundry>
- Marketplace repository: <https://github.com/Alex-Caepio/ai-infra-plugins>

## Capabilities

- **Scaffold & Package**: `init-skill` (`skill_scaffold`), `package` (`skill_package`) -> portable `.skill` zip archives.
- **Generalized Skill Validator**: `validate` (`skill_validate`) -> YAML frontmatter schema, kebab-case naming, line budgets, and local `scripts/`/`references/`/`assets/` link verification across single skills or entire plugin trees.
- **Token Budget Linter**: `budget-check` (`skill_budget_check`) -> byte counts and calibrated `cl100k` token estimates for `AGENTS.md`, skill descriptors, commands, agents, and skill bodies, with optional JSON ceilings.
- **Trigger Evaluation & Description Optimizer**: `eval-triggers` (`skill_eval_triggers`), `optimize-description` (`skill_optimize_description`), `routing-eval`, `report` -> cross-CLI (`claude`, `codex`, `cursor`) trigger probing with deterministic CPython MT19937 seed-42 train/holdout splits and HTML reports.
- **Security & Risk Vetting (8 Binary + 4 Scored Gates)**: `vet-skill` (`skill_vet`) -> static risk scan (`license`, `data_egress`, `pinning`, `uninstall`, `collisions`, `auto_update`, `maintenance_ownership`, `measurability` + `0..5` quality scores) and Kit-style JSON dossier validation.
- **Dual-Host BTE Cost Auditor**: `cost-audit` (`skill_cost_audit`) -> scans `~/.claude/projects` and `~/.codex/sessions` JSONL transcripts using BTE weights (`1.0` / `1.25` / `2.0` / `0.1` / `5.0`) across `main` vs `subagent` strata and `skill`, `mcp`, `tool`, `plugin`, `agent` attribution.
- **Benchmark & Standalone Review UI**: `benchmark`, `review-ui`, `eval-set-review` -> built-in loopback HTML viewer & `--static` single-file exporter supporting text, image, PDF, and OOXML `.xlsx` previews via `ooxml-numfmt`.

## Installation

### Claude Code

```bash
# Install via GitHub marketplace
claude plugin marketplace add Alex-Caepio/ai-infra-plugins
claude plugin install skill-foundry@ai-infra-plugins --scope user

# Or validate / load from a local checkout
claude plugin validate --strict .
claude --plugin-dir .
```

### Codex CLI

```bash
codex plugin marketplace add https://github.com/Alex-Caepio/ai-infra-plugins.git
codex plugin add skill-foundry@ai-infra-plugins
codex plugin list --json
```

> **Note on Codex MCP calls**: `.codex-mcp.json` launches `./bin/skill-foundry` with `"cwd": "."`. Always pass explicit workspace/skill paths (`path`, `skill_path`, `output_dir`, `project_root`) rather than relying on process `cwd`.

## Build from Source

```bash
CARGO_TARGET_DIR=/tmp/skill-foundry-build CARGO_INCREMENTAL=0 cargo test --offline --locked
CARGO_TARGET_DIR=/tmp/skill-foundry-build CARGO_INCREMENTAL=0 cargo build --release --offline --locked
install -m 755 /tmp/skill-foundry-build/release/skill-foundry bin/skill-foundry
```

## CLI Quick Examples

```bash
./bin/skill-foundry validate ./skills
./bin/skill-foundry budget-check .
./bin/skill-foundry vet-skill .
./bin/skill-foundry cost-audit --host all --since-days 7
./bin/skill-foundry mcp
```
