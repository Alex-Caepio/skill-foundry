//! Benchmark result aggregation across eval runs -> JSON & Markdown reports.
use crate::{Result, RuntimeError, common};
use serde_json::{Map, Value, json};
use std::path::Path;

pub fn calculate_stats(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"mean": 0.0, "stddev": 0.0, "min": 0.0, "max": 0.0});
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let sd = if values.len() > 1 {
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
    } else {
        0.0
    };
    json!({
        "mean": common::rounded(mean),
        "stddev": common::rounded(sd),
        "min": common::rounded(values.iter().copied().fold(f64::INFINITY, f64::min)),
        "max": common::rounded(values.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
    })
}

fn number(v: &Value, key: &str) -> f64 {
    v[key].as_f64().unwrap_or(0.0)
}

fn fallback(v: &Value, key: &str, default: Value) -> Value {
    v.get(key).cloned().unwrap_or(default)
}

pub fn load_run_results(directory: &Path, log: &mut String) -> Result<Map<String, Value>> {
    let search = if directory.join("runs").exists() {
        directory.join("runs")
    } else if common::sorted(directory)?
        .iter()
        .any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("eval-")))
    {
        directory.into()
    } else {
        log.push_str(&format!(
            "No eval directories found in {}\n",
            directory.display()
        ));
        return Ok(Map::new());
    };
    let evals: Vec<_> = common::sorted(&search)?
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("eval-")))
        .collect();
    let mut results = Map::new();
    for (index, eval) in evals.iter().enumerate() {
        let metadata = eval.join("eval_metadata.json");
        let id = if metadata.exists() {
            common::json_file(&metadata)
                .ok()
                .and_then(|v| v.get("eval_id").cloned())
                .unwrap_or(json!(index))
        } else {
            eval.file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split('-').nth(1))
                .and_then(|s| s.parse::<i64>().ok())
                .map(Value::from)
                .unwrap_or(json!(index))
        };
        for config in common::sorted(eval)? {
            if !config.is_dir() {
                continue;
            }
            let runs: Vec<_> = common::sorted(&config)?
                .into_iter()
                .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("run-")))
                .collect();
            if runs.is_empty() {
                continue;
            }
            let name = config.file_name().unwrap().to_string_lossy().into_owned();
            results.entry(name.clone()).or_insert_with(|| json!([]));
            for run in runs {
                let run_number = run
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.split('-').nth(1))
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        RuntimeError::invalid(format!("Invalid run number in {}", run.display()))
                    })?;
                let path = run.join("grading.json");
                if !path.exists() {
                    log.push_str(&format!("Warning: grading.json not found in {}\n", run.display()));
                    continue;
                }
                let grading = common::json_file(&path)?;
                let summary = &grading["summary"];
                let metrics = &grading["execution_metrics"];
                let mut time = fallback(&grading["timing"], "total_duration_seconds", json!(0.0));
                let mut tokens = json!(0);
                if time.as_f64() == Some(0.0)
                    && run.join("timing.json").exists()
                    && let Ok(t) = common::json_file(&run.join("timing.json"))
                {
                    time = fallback(&t, "total_duration_seconds", json!(0.0));
                    tokens = fallback(&t, "total_tokens", json!(0));
                }
                if tokens.as_f64().is_none_or(|n| n == 0.0) {
                    tokens = fallback(metrics, "output_chars", json!(0));
                }
                let expectations = fallback(&grading, "expectations", json!([]));
                let mut notes = vec![];
                for field in ["uncertainties", "needs_review", "workarounds"] {
                    if let Some(a) = grading["user_notes_summary"][field].as_array() {
                        notes.extend(a.clone());
                    }
                }
                results.get_mut(&name).unwrap().as_array_mut().unwrap().push(json!({
                    "eval_id": id,
                    "run_number": run_number,
                    "pass_rate": fallback(summary, "pass_rate", json!(0.0)),
                    "passed": fallback(summary, "passed", json!(0)),
                    "failed": fallback(summary, "failed", json!(0)),
                    "total": fallback(summary, "total", json!(0)),
                    "time_seconds": time,
                    "tokens": tokens,
                    "tool_calls": fallback(metrics, "total_tool_calls", json!(0)),
                    "errors": fallback(metrics, "errors_encountered", json!(0)),
                    "expectations": expectations,
                    "notes": notes
                }));
            }
        }
    }
    Ok(results)
}

pub fn aggregate_results(results: &Map<String, Value>) -> Value {
    let mut summary = Map::new();
    for (name, runs) in results {
        let mut metrics = Map::new();
        for key in ["pass_rate", "time_seconds", "tokens"] {
            metrics.insert(
                key.into(),
                calculate_stats(
                    &runs
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| number(r, key))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        summary.insert(name.clone(), Value::Object(metrics));
    }
    let configs = summary.values().cloned().collect::<Vec<_>>();
    let zero = Value::Null;
    let a = configs.first().unwrap_or(&zero);
    let b = configs.get(1).unwrap_or(&zero);
    summary.insert(
        "delta".into(),
        json!({
            "pass_rate": format!("{:+.2}", number(&a["pass_rate"], "mean") - number(&b["pass_rate"], "mean")),
            "time_seconds": format!("{:+.1}", number(&a["time_seconds"], "mean") - number(&b["time_seconds"], "mean")),
            "tokens": format!("{:+.0}", number(&a["tokens"], "mean") - number(&b["tokens"], "mean")),
        }),
    );
    Value::Object(summary)
}

pub fn generate_benchmark(
    directory: &Path,
    name: &str,
    path: &str,
    log: &mut String,
) -> Result<Value> {
    let results = load_run_results(directory, log)?;
    let summary = aggregate_results(&results);
    let mut runs = vec![];
    let mut ids = vec![];
    for (config, list) in &results {
        for r in list.as_array().unwrap() {
            let mut metrics = Map::new();
            for key in [
                "pass_rate",
                "passed",
                "failed",
                "total",
                "time_seconds",
                "tokens",
                "tool_calls",
                "errors",
            ] {
                metrics.insert(key.into(), r[key].clone());
            }
            if !ids.contains(&r["eval_id"]) {
                ids.push(r["eval_id"].clone());
            }
            runs.push(json!({
                "eval_id": r["eval_id"],
                "configuration": config,
                "run_number": r["run_number"],
                "result": metrics,
                "expectations": r["expectations"],
                "notes": r["notes"]
            }));
        }
    }
    ids.sort_by(|a, b| match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => common::display(a).cmp(&common::display(b)),
    });
    Ok(json!({
        "metadata": {
            "skill_name": if name.is_empty() { "<skill-name>" } else { name },
            "skill_path": if path.is_empty() { "<path/to/skill>" } else { path },
            "timestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "evals_run": ids,
            "runs_per_configuration": 3
        },
        "runs": runs,
        "run_summary": summary,
        "notes": []
    }))
}

pub fn generate_markdown(v: &Value) -> String {
    let m = &v["metadata"];
    let s = &v["run_summary"];
    let configs = s
        .as_object()
        .map(|m| m.keys().filter(|k| *k != "delta").map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let a = configs.first().copied().unwrap_or("config_a");
    let b = configs.get(1).copied().unwrap_or("config_b");
    let ids = m["evals_run"]
        .as_array()
        .map(|arr| arr.iter().map(common::display).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let mut out = format!(
        "# Skill Benchmark: {}\n\n**Date**: {}\n**Evals**: {ids}\n\n## Summary\n\n| Metric | {} | {} | Delta |\n|--------|------------|---------------|-------|\n",
        common::display(&m["skill_name"]),
        common::display(&m["timestamp"]),
        common::title(a),
        common::title(b)
    );
    for (key, label, digits, suffix, scale) in [
        ("pass_rate", "Pass Rate", 0, "%", 100.0),
        ("time_seconds", "Time", 1, "s", 1.0),
        ("tokens", "Tokens", 0, "", 1.0),
    ] {
        let av = &s[a][key];
        let bv = &s[b][key];
        let delta = s["delta"][key].as_str().unwrap_or("—");
        out.push_str(&format!(
            "| {label} | {:.*}{suffix} ± {:.*}{suffix} | {:.*}{suffix} ± {:.*}{suffix} | {delta}{} |\n",
            digits,
            number(av, "mean") * scale,
            digits,
            number(av, "stddev") * scale,
            digits,
            number(bv, "mean") * scale,
            digits,
            number(bv, "stddev") * scale,
            if key == "time_seconds" { "s" } else { "" }
        ));
    }
    out
}

pub fn execute(v: &Value) -> Result<Value> {
    let dir = common::absolute(common::positional(v, "benchmark_dir", 0)?)?;
    if !dir.is_dir() {
        return Err(RuntimeError::not_found(format!(
            "Directory not found: {}",
            dir.display()
        )));
    }
    let name = common::text(v, "skill_name").unwrap_or("");
    let skill_path = common::text(v, "skill_path").unwrap_or("");
    let mut log = String::new();
    let benchmark = generate_benchmark(&dir, name, skill_path, &mut log)?;
    if let Some(out) = common::text(v, "output") {
        common::write_json(Path::new(out), &benchmark, true)?;
    }
    if let Some(md) = common::text(v, "markdown") {
        common::atomic_write(Path::new(md), generate_markdown(&benchmark).as_bytes())?;
    }
    common::json_cli(benchmark, log)
}
