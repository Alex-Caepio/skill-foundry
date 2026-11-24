//! Benchmark result aggregation across eval runs -> JSON & Markdown reports.
use crate::{
    Result, RuntimeError,
    common::{self, FileStatStamp, RACY_FS_NS},
};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

pub const BENCHMARK_RACY_NS: i128 = RACY_FS_NS;
const BENCHMARK_CACHE_CAP: usize = 64;
pub type BenchFileStamp = FileStatStamp;

type BenchStampEntry = (String, bool, BenchFileStamp);

struct BenchmarkStampSnapshot {
    canon: PathBuf,
    stamps: Vec<BenchStampEntry>,
    saw_symlink: bool,
}

#[derive(Clone)]
struct CachedBenchmarkRunResults {
    stamps: Vec<BenchStampEntry>,
    results: Map<String, Value>,
    warnings: String,
    last_used: u64,
}

#[derive(Default)]
struct BenchmarkResultsCache {
    seq: u64,
    entries: HashMap<PathBuf, CachedBenchmarkRunResults>,
}

fn benchmark_results_cache() -> &'static Mutex<BenchmarkResultsCache> {
    static CACHE: OnceLock<Mutex<BenchmarkResultsCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BenchmarkResultsCache::default()))
}

#[cfg(test)]
thread_local! {
    static TEST_BENCHMARK_JSON_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn bump_benchmark_parses() {
    TEST_BENCHMARK_JSON_PARSES.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn benchmark_parse_count() -> usize {
    TEST_BENCHMARK_JSON_PARSES.with(std::cell::Cell::get)
}

pub fn is_baseline(configuration: &str) -> bool {
    ["without_skill", "old_skill", "cold", "baseline"].contains(&configuration)
        || configuration.starts_with("baseline_")
}

fn stat_bench_entry(
    p: &Path,
    label: String,
    stamps: &mut Vec<BenchStampEntry>,
    saw_symlink: &mut bool,
) {
    if let Ok(m) = std::fs::symlink_metadata(p) {
        if m.file_type().is_symlink() {
            *saw_symlink = true;
        } else {
            stamps.push((label, m.is_dir(), BenchFileStamp::from_metadata(&m)));
        }
    }
}

fn collect_benchmark_stamps(directory: &Path) -> Result<BenchmarkStampSnapshot> {
    let canon = common::safe_canonicalize(directory)?;
    let mut stamps = Vec::new();
    let mut saw_symlink = false;

    stat_bench_entry(&canon, ".".into(), &mut stamps, &mut saw_symlink);
    let runs_sub = canon.join("runs");
    stat_bench_entry(&runs_sub, "runs".into(), &mut stamps, &mut saw_symlink);

    let search = if runs_sub.exists() {
        runs_sub
    } else if common::sorted(&canon)?.iter().any(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("eval-"))
    }) {
        canon.clone()
    } else {
        stamps.sort_by(|a, b| a.0.cmp(&b.0));
        return Ok(BenchmarkStampSnapshot {
            canon,
            stamps,
            saw_symlink,
        });
    };

    for eval in common::sorted(&search)? {
        let eval_name = eval.file_name().unwrap_or_default().to_string_lossy();
        if !eval_name.starts_with("eval-") {
            continue;
        }
        stat_bench_entry(
            &eval,
            format!("eval/{eval_name}"),
            &mut stamps,
            &mut saw_symlink,
        );
        stat_bench_entry(
            &eval.join("eval_metadata.json"),
            format!("eval/{eval_name}/eval_metadata.json"),
            &mut stamps,
            &mut saw_symlink,
        );
        if eval.is_dir() {
            for config in common::sorted(&eval)? {
                if let Ok(cm) = std::fs::symlink_metadata(&config) {
                    if cm.file_type().is_symlink() {
                        saw_symlink = true;
                        continue;
                    }
                    if !cm.is_dir() {
                        continue;
                    }
                } else {
                    continue;
                }
                let cfg_name = config.file_name().unwrap_or_default().to_string_lossy();
                stat_bench_entry(
                    &config,
                    format!("eval/{eval_name}/{cfg_name}"),
                    &mut stamps,
                    &mut saw_symlink,
                );
                for run in common::sorted(&config)? {
                    let run_name = run.file_name().unwrap_or_default().to_string_lossy();
                    if !run_name.starts_with("run-") {
                        continue;
                    }
                    stat_bench_entry(
                        &run,
                        format!("eval/{eval_name}/{cfg_name}/{run_name}"),
                        &mut stamps,
                        &mut saw_symlink,
                    );
                    for fname in ["grading.json", "timing.json", "metrics.json"] {
                        stat_bench_entry(
                            &run.join(fname),
                            format!("eval/{eval_name}/{cfg_name}/{run_name}/{fname}"),
                            &mut stamps,
                            &mut saw_symlink,
                        );
                    }
                }
            }
        }
    }
    stamps.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(BenchmarkStampSnapshot {
        canon,
        stamps,
        saw_symlink,
    })
}

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

fn first_finite(candidates: &[Option<&Value>]) -> Option<f64> {
    candidates
        .iter()
        .flatten()
        .find_map(|v| v.as_f64().filter(|n| n.is_finite()))
}

pub fn load_run_results(directory: &Path, log: &mut String) -> Result<Map<String, Value>> {
    load_run_results_with_options(
        directory,
        log,
        common::now_unix_ns(),
        common::is_cache_bypassed(),
    )
}

pub fn load_run_results_with_options(
    directory: &Path,
    log: &mut String,
    now_ns: i128,
    bypass: bool,
) -> Result<Map<String, Value>> {
    let stamp_snapshot = if !bypass {
        collect_benchmark_stamps(directory).ok()
    } else {
        None
    };
    if let Some(snap) = stamp_snapshot.as_ref()
        && !snap.saw_symlink
        && snap.stamps.iter().all(|(_, _, s)| s.is_settled(now_ns))
        && let Ok(mut cache) = benchmark_results_cache().lock()
    {
        cache.seq = cache.seq.saturating_add(1);
        let seq = cache.seq;
        if let Some(hit) = cache.entries.get_mut(&snap.canon)
            && hit.stamps == snap.stamps
        {
            hit.last_used = seq;
            log.push_str(&hit.warnings);
            return Ok(hit.results.clone());
        }
    }

    #[cfg(test)]
    bump_benchmark_parses();
    let mut captured_warnings = String::new();
    let results = load_run_results_uncached(directory, &mut captured_warnings)?;
    log.push_str(&captured_warnings);

    if !bypass
        && let Some(snap) = stamp_snapshot
        && !snap.saw_symlink
        && snap.stamps.iter().all(|(_, _, s)| s.is_settled(now_ns))
        && let Ok(mut cache) = benchmark_results_cache().lock()
    {
        cache.seq = cache.seq.saturating_add(1);
        let seq = cache.seq;
        if cache.entries.len() >= BENCHMARK_CACHE_CAP
            && !cache.entries.contains_key(&snap.canon)
            && let Some(evict) = cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone())
        {
            cache.entries.remove(&evict);
        }
        cache.entries.insert(
            snap.canon,
            CachedBenchmarkRunResults {
                stamps: snap.stamps,
                results: results.clone(),
                warnings: captured_warnings,
                last_used: seq,
            },
        );
    }
    Ok(results)
}

fn load_run_results_uncached(directory: &Path, log: &mut String) -> Result<Map<String, Value>> {
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
                let timing_doc = if run.join("timing.json").exists() {
                    common::json_file(&run.join("timing.json")).ok()
                } else {
                    None
                };
                let metrics_doc = if run.join("metrics.json").exists() {
                    common::json_file(&run.join("metrics.json")).ok()
                } else {
                    None
                };
                let mref = metrics_doc.as_ref().unwrap_or(&Value::Null);
                let tref = timing_doc.as_ref().unwrap_or(&Value::Null);

                let summary = &grading["summary"];
                let metrics = &grading["execution_metrics"];
                let mut time = fallback(&grading["timing"], "total_duration_seconds", json!(0.0));
                let mut tokens = json!(0);
                if time.as_f64() == Some(0.0) && !tref.is_null() {
                    time = fallback(tref, "total_duration_seconds", json!(0.0));
                    tokens = fallback(tref, "total_tokens", json!(0));
                }
                if tokens.as_f64().is_none_or(|n| n == 0.0) {
                    tokens = fallback(metrics, "output_chars", json!(0));
                }
                if time.as_f64() == Some(0.0)
                    && let Some(t) = first_finite(&[mref.get("time_seconds"), mref.get("seconds")])
                {
                    time = json!(t);
                }
                if tokens.as_f64().is_none_or(|n| n == 0.0)
                    && let Some(tok) = first_finite(&[mref.get("tokens"), mref.get("total_tokens")])
                {
                    tokens = json!(tok);
                }

                let expectations = fallback(&grading, "expectations", json!([]));
                let mut notes = vec![];
                for field in ["uncertainties", "needs_review", "workarounds"] {
                    if let Some(a) = grading["user_notes_summary"][field].as_array() {
                        notes.extend(a.clone());
                    }
                }
                let mut entry = json!({
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
                });
                let obj = entry.as_object_mut().unwrap();
                if let Some(v) = first_finite(&[
                    mref.get("model_calls"),
                    metrics.get("model_calls"),
                    grading.get("model_calls"),
                    tref.get("model_calls"),
                ]) {
                    obj.insert("model_calls".into(), json!(v));
                }
                if let Some(v) = first_finite(&[
                    mref.get("deopt_rate"),
                    summary.get("deopt_rate"),
                    grading.get("deopt_rate"),
                ]) {
                    obj.insert("deopt_rate".into(), json!(v));
                }
                if let Some(v) = first_finite(&[
                    mref.get("list_price_usd"),
                    metrics.get("list_price_usd"),
                    grading.get("list_price_usd"),
                    tref.get("list_price_usd"),
                ]) {
                    obj.insert("list_price_usd".into(), json!(v));
                }
                if let Some(v) = first_finite(&[
                    mref.get("compile_usd"),
                    mref["preparation"].get("total_usd"),
                    mref["preparation"].get("compile_usd"),
                    grading.get("compile_usd"),
                    grading["preparation"].get("total_usd"),
                    grading["preparation"].get("compile_usd"),
                ]) {
                    obj.insert("compile_usd".into(), json!(v));
                }
                if let Some(v) = first_finite(&[
                    mref.get("breakeven_reuses"),
                    mref["against_cold"].get("breakeven_reuses"),
                    grading.get("breakeven_reuses"),
                    grading["against_cold"].get("breakeven_reuses"),
                ]) {
                    obj.insert("breakeven_reuses".into(), json!(v));
                }
                results.get_mut(&name).unwrap().as_array_mut().unwrap().push(entry);
            }
        }
    }
    Ok(results)
}

const CORE_METRICS: [&str; 3] = ["pass_rate", "time_seconds", "tokens"];
const EXTENDED_METRICS: [(&str, usize); 5] = [
    ("model_calls", 1),
    ("deopt_rate", 2),
    ("list_price_usd", 4),
    ("compile_usd", 4),
    ("breakeven_reuses", 0),
];

fn ordered_configs(names: &[String]) -> Vec<String> {
    let mut baseline = Vec::new();
    let mut others = Vec::new();
    for name in names {
        if is_baseline(name) {
            baseline.push(name.clone());
        } else {
            others.push(name.clone());
        }
    }
    if baseline.len() + others.len() <= 2 && !baseline.is_empty() && !others.is_empty() {
        // Put arm first when comparing 2 configs for classic primary-vs-baseline reading.
        others.extend(baseline);
        others
    } else {
        baseline.extend(others);
        baseline
    }
}

fn delta_object(primary: &Value, base: &Value) -> Value {
    let mut delta = Map::new();
    delta.insert(
        "pass_rate".into(),
        json!(format!(
            "{:+.2}",
            number(&primary["pass_rate"], "mean") - number(&base["pass_rate"], "mean")
        )),
    );
    delta.insert(
        "time_seconds".into(),
        json!(format!(
            "{:+.1}",
            number(&primary["time_seconds"], "mean") - number(&base["time_seconds"], "mean")
        )),
    );
    delta.insert(
        "tokens".into(),
        json!(format!(
            "{:+.0}",
            number(&primary["tokens"], "mean") - number(&base["tokens"], "mean")
        )),
    );
    for (key, decimals) in EXTENDED_METRICS {
        if let (Some(a), Some(b)) = (
            primary.get(key).and_then(|v| v.get("mean")).and_then(Value::as_f64),
            base.get(key).and_then(|v| v.get("mean")).and_then(Value::as_f64),
        ) {
            delta.insert(key.into(), json!(format!("{:+.*}", decimals, a - b)));
        }
    }
    Value::Object(delta)
}

pub fn aggregate_results(results: &Map<String, Value>) -> Value {
    let mut summary = Map::new();
    for (name, runs) in results {
        let mut metrics = Map::new();
        let arr = runs.as_array().map(Vec::as_slice).unwrap_or(&[]);
        for key in CORE_METRICS {
            metrics.insert(
                key.into(),
                calculate_stats(&arr.iter().map(|r| number(r, key)).collect::<Vec<_>>()),
            );
        }
        for (key, _) in EXTENDED_METRICS {
            let vals: Vec<f64> = arr
                .iter()
                .filter_map(|r| r.get(key).and_then(Value::as_f64).filter(|n| n.is_finite()))
                .collect();
            if !vals.is_empty() {
                metrics.insert(key.into(), calculate_stats(&vals));
            }
        }
        summary.insert(name.clone(), Value::Object(metrics));
    }

    let names: Vec<String> = summary.keys().cloned().collect();
    let base_name = names.iter().find(|n| is_baseline(n)).cloned();
    let zero = Value::Null;

    let (primary_name, compare_base) = match base_name.as_deref() {
        Some(base) => {
            let primary = names.iter().find(|n| n.as_str() != base).cloned();
            (primary, Some(base.to_owned()))
        }
        None => (names.first().cloned(), names.get(1).cloned()),
    };

    let a = primary_name
        .as_deref()
        .and_then(|n| summary.get(n))
        .unwrap_or(&zero);
    let b = compare_base
        .as_deref()
        .and_then(|n| summary.get(n))
        .unwrap_or(&zero);
    summary.insert("delta".into(), delta_object(a, b));

    if let Some(base) = base_name {
        let non_base: Vec<&String> = names.iter().filter(|n| *n != &base).collect();
        if non_base.len() >= 2 {
            let mut deltas = Map::new();
            let base_val = summary.get(&base).unwrap_or(&zero);
            for cfg in non_base {
                let cfg_val = summary.get(cfg).unwrap_or(&zero);
                deltas.insert(cfg.clone(), delta_object(cfg_val, base_val));
            }
            summary.insert("deltas".into(), Value::Object(deltas));
        }
    }
    Value::Object(summary)
}

pub fn generate_benchmark(
    directory: &Path,
    name: &str,
    path: &str,
    log: &mut String,
) -> Result<Value> {
    generate_benchmark_with_options(
        directory,
        name,
        path,
        log,
        common::now_unix_ns(),
        common::is_cache_bypassed(),
    )
}

pub fn generate_benchmark_with_options(
    directory: &Path,
    name: &str,
    path: &str,
    log: &mut String,
    now_ns: i128,
    bypass: bool,
) -> Result<Value> {
    let results = load_run_results_with_options(directory, log, now_ns, bypass)?;
    let summary = aggregate_results(&results);
    let raw_configs: Vec<String> = results.keys().cloned().collect();
    let config_order = ordered_configs(&raw_configs);
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
            for (key, _) in EXTENDED_METRICS {
                if !r[key].is_null() {
                    metrics.insert(key.into(), r[key].clone());
                }
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
        "configuration_order": config_order,
        "runs": runs,
        "run_summary": summary,
        "notes": []
    }))
}

pub fn generate_markdown(v: &Value) -> String {
    let m = &v["metadata"];
    let s = &v["run_summary"];
    let configs: Vec<String> = if let Some(order) = v["configuration_order"].as_array() {
        order
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .filter(|k| s.get(k).is_some())
            .collect()
    } else {
        s.as_object()
            .map(|obj| {
                obj.keys()
                    .filter(|k| !matches!(k.as_str(), "delta" | "deltas" | "delta_vs_baseline"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    let effective = if configs.is_empty() {
        vec!["config_a".to_owned(), "config_b".to_owned()]
    } else {
        configs
    };
    let a = effective[0].as_str();
    let b = effective.get(1).map(String::as_str).unwrap_or("config_b");
    let ids = m["evals_run"]
        .as_array()
        .map(|arr| arr.iter().map(common::display).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let mut header_cols = vec!["Metric".to_owned()];
    if effective.len() <= 2 {
        header_cols.push(common::title(a));
        header_cols.push(common::title(b));
        header_cols.push("Delta".to_owned());
    } else {
        for cfg in &effective {
            header_cols.push(common::title(cfg));
        }
        header_cols.push("Delta".to_owned());
    }
    let sep = header_cols
        .iter()
        .map(|c| "-".repeat(c.len().max(5)))
        .collect::<Vec<_>>()
        .join("|");
    let mut out = format!(
        "# Skill Benchmark: {}\n\n**Date**: {}\n**Evals**: {ids}\n\n## Summary\n\n| {} |\n|{}|\n",
        common::display(&m["skill_name"]),
        common::display(&m["timestamp"]),
        header_cols.join(" | "),
        sep
    );
    let metric_specs: [(&str, &str, usize, &str, f64, bool); 8] = [
        ("pass_rate", "Pass Rate", 0, "%", 100.0, true),
        ("time_seconds", "Time", 1, "s", 1.0, true),
        ("tokens", "Tokens", 0, "", 1.0, true),
        ("model_calls", "Model calls", 1, "", 1.0, false),
        ("deopt_rate", "Steps handed back", 0, "%", 100.0, false),
        ("list_price_usd", "List price (USD)", 4, "", 1.0, false),
        ("compile_usd", "Compilation (USD)", 4, "", 1.0, false),
        ("breakeven_reuses", "Break-even reuses", 0, "", 1.0, false),
    ];
    for (key, label, digits, suffix, scale, always) in metric_specs {
        if !always && effective.iter().all(|c| s[c.as_str()][key].is_null()) {
            continue;
        }
        let mut row = vec![label.to_owned()];
        let render_stat = |cfg: &str| {
            let val = &s[cfg][key];
            if val.is_null() {
                "—".to_owned()
            } else {
                format!(
                    "{:.*}{suffix} ± {:.*}{suffix}",
                    digits,
                    number(val, "mean") * scale,
                    digits,
                    number(val, "stddev") * scale,
                )
            }
        };
        if effective.len() <= 2 {
            row.push(render_stat(a));
            row.push(render_stat(b));
        } else {
            for cfg in &effective {
                row.push(render_stat(cfg));
            }
        }
        let delta = s["delta"][key].as_str().unwrap_or("—");
        let delta_suffix = if key == "time_seconds" && delta != "—" {
            "s"
        } else {
            ""
        };
        row.push(format!("{delta}{delta_suffix}"));
        out.push_str(&format!("| {} |\n", row.join(" | ")));
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
    let bypass = common::flag(v, "strict") || common::flag(v, "no_cache") || common::is_cache_bypassed();
    let name = common::text(v, "skill_name").unwrap_or("");
    let skill_path = common::text(v, "skill_path").unwrap_or("");
    let mut log = String::new();
    let benchmark = generate_benchmark_with_options(
        &dir,
        name,
        skill_path,
        &mut log,
        common::now_unix_ns(),
        bypass,
    )?;
    if let Some(out) = common::text(v, "output") {
        common::write_json(Path::new(out), &benchmark, true)?;
    }
    if let Some(md) = common::text(v, "markdown") {
        common::atomic_write(Path::new(md), generate_markdown(&benchmark).as_bytes())?;
    }
    common::json_cli(benchmark, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_run_results_caches_settled_json_reads_and_respects_racy_ns_and_bypass() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path().join("bench");
        let run1 = ws.join("runs/eval-1/with_skill/run-1");
        std::fs::create_dir_all(&run1).unwrap();
        let grading = run1.join("grading.json");
        std::fs::write(
            &grading,
            r#"{"summary":{"pass_rate":1.0,"passed":2,"failed":0,"total":2},"execution_metrics":{"total_tool_calls":3,"errors_encountered":0},"timing":{"total_duration_seconds":1.5}}"#,
        )
        .unwrap();

        let stamp = BenchFileStamp::from_metadata(&std::fs::symlink_metadata(&grading).unwrap());
        let racy_now = stamp.mtime_ns + BENCHMARK_RACY_NS - 1;
        let settled_now = stamp.mtime_ns + BENCHMARK_RACY_NS + 10_000;

        let parses_before = benchmark_parse_count();

        let mut log = String::new();
        let r_racy1 = load_run_results_with_options(&ws, &mut log, racy_now, false).unwrap();
        let r_racy2 = load_run_results_with_options(&ws, &mut log, racy_now, false).unwrap();
        assert_eq!(r_racy1, r_racy2);
        assert_eq!(benchmark_parse_count(), parses_before + 2);

        let r1 = load_run_results_with_options(&ws, &mut log, settled_now, false).unwrap();
        let r2 = load_run_results_with_options(&ws, &mut log, settled_now + 5, false).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(benchmark_parse_count(), parses_before + 3);

        let r_bypass =
            load_run_results_with_options(&ws, &mut log, settled_now + 10, true).unwrap();
        assert_eq!(r_bypass, r1);
        assert_eq!(benchmark_parse_count(), parses_before + 4);

        std::fs::write(
            &grading,
            r#"{"summary":{"pass_rate":0.5,"passed":1,"failed":1,"total":2},"execution_metrics":{"total_tool_calls":3,"errors_encountered":0},"timing":{"total_duration_seconds":1.5}}"#,
        )
        .unwrap();
        let updated_stamp =
            BenchFileStamp::from_metadata(&std::fs::symlink_metadata(&grading).unwrap());
        let r3 = load_run_results_with_options(
            &ws,
            &mut log,
            updated_stamp.mtime_ns + BENCHMARK_RACY_NS + 10_000_000,
            false,
        )
        .unwrap();
        assert_eq!(r3["with_skill"][0]["pass_rate"], json!(0.5));
        assert_eq!(benchmark_parse_count(), parses_before + 5);

        let cli_out = execute(&json!({
            "benchmark_dir": ws,
            "strict": true,
            "output": temp.path().join("out.json")
        }))
        .unwrap();
        assert_eq!(cli_out["_cli"]["code"], json!(0));
    }
}
