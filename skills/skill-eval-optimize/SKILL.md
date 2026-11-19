---
name: skill-eval-optimize
description: Use this skill when testing whether a skill triggers accurately on positive and negative queries, iterating on SKILL.md description wording with train/holdout splits, aggregating benchmark runs, or inspecting outputs in an interactive review HTML viewer.
license: Apache-2.0
---

# Skill Trigger Evaluation, Optimization & Review

Use `skill-foundry` tools to measure real CLI skill routing behavior without overfitting descriptions.

## Workflow

1. **Prepare an eval set**: JSON array of `{"query": "...", "should_trigger": true|false}`. Optionally export an interactive editor with `skill-foundry eval-set-review --skill <dir> --eval-set <json> --output <out.html>`.
2. **Baseline trigger probe**: Run `skill_eval_triggers` (`skill-foundry eval-triggers --skill <dir> --eval-set <json> --platform claude|codex|cursor`).
3. **Description optimization loop**: Run `skill_optimize_description` (`skill-foundry optimize-description --skill <dir> --eval-set <json> --holdout 0.4 --max-iterations 5 --report report.html`). Held-out queries are withheld from improvement prompts using deterministic CPython MT19937 seed 42 splitting.
4. **Benchmark & qualitative review**:
   - Aggregate grading/timing runs with `skill-foundry benchmark <workspace> --output benchmark.json --markdown benchmark.md`.
   - Launch the local feedback/OOXML/PDF previewer with `skill-foundry review-ui <workspace> --port 3117` (or `--static review.html`).
