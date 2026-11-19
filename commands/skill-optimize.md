---
description: Evaluate and iteratively optimize a skill description
argument-hint: "<skill-dir> <eval-set.json>"
allowed-tools: [mcp__skill-foundry__skill_eval_triggers, mcp__skill-foundry__skill_optimize_description, Read]
---

Run trigger evaluation and/or iterative description optimization for `$ARGUMENTS`:

1. Ensure an eval set JSON (`[{query, should_trigger}]`) is specified or help the user generate one using the `skill-eval-optimize` skill.
2. Call `skill_eval_triggers` first for baseline trigger precision/recall.
3. If failures occur, run `skill_optimize_description` with `holdout: 0.4` and report the train/test improvement summary.
