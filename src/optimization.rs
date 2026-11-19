//! Iterative skill description optimization with deterministic CPython MT19937 holdout split.
use crate::{Result, RuntimeError, cancel, common, evaluation, platform_cli, reports, rng, utils};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const SPLIT_PROFILE: &str = "cpython-mt19937-integer-v1-shuffle-seed42";

pub fn split_eval_set(
    eval: &[Value],
    holdout: f64,
    native: bool,
) -> Result<(Vec<Value>, Vec<Value>)> {
    if native && !(0.0 < holdout && holdout < 1.0) {
        return Err(RuntimeError::invalid(
            "holdout must be greater than 0 and less than 1",
        ));
    }
    let (mut positive, mut negative): (Vec<_>, Vec<_>) =
        eval.iter().cloned().partition(|e| e["should_trigger"] == true);
    if native && (positive.len() < 2 || negative.len() < 2) {
        return Err(RuntimeError::invalid(
            "A holdout split requires at least two positive and two negative queries",
        ));
    }
    let mut prng = rng::Random::new(42);
    for list in [&mut positive, &mut negative] {
        for i in (1..list.len()).rev() {
            let j = prng.below(i + 1);
            list.swap(i, j);
        }
    }
    let split = |n: usize| {
        if n == 0 {
            return 0;
        }
        let k = ((n as f64 * holdout).floor().max(0.0) as usize).max(1);
        if native { k.min(n - 1) } else { k.min(n) }
    };
    let a = split(positive.len());
    let b = split(negative.len());
    let test = positive[..a].iter().chain(&negative[..b]).cloned().collect();
    let train = positive[a..].iter().chain(&negative[b..]).cloned().collect();
    Ok((train, test))
}

pub fn parse_description(text: &str) -> String {
    let found = text
        .find("<new_description>")
        .and_then(|a| {
            let rest = &text[a + "<new_description>".len()..];
            rest.find("</new_description>").map(|b| &rest[..b])
        })
        .unwrap_or(text);
    found.trim().trim_matches('"').into()
}

fn score(summary: &Value) -> String {
    format!("{}/{}", summary["passed"], summary["total"])
}

pub fn improvement_prompt(
    config: &platform_cli::PlatformConfig,
    name: &str,
    content: &str,
    current: &str,
    eval: &Value,
    history: &[Value],
    test: Option<&Value>,
) -> String {
    let scores = format!(
        "Train: {}{}",
        score(&eval["summary"]),
        test.map(|t| format!(", Test: {}", score(&t["summary"])))
            .unwrap_or_default()
    );
    let (prefix, suffix) = if config.key == "claude" {
        (
            include_str!("../resources/improve-claude-prefix.txt"),
            include_str!("../resources/improve-claude-suffix.txt"),
        )
    } else {
        (
            include_str!("../resources/improve-native-prefix.txt"),
            include_str!("../resources/improve-native-suffix.txt"),
        )
    };
    let mut prompt = prefix
        .replace("{skill_name}", name)
        .replace("{current_description}", current)
        .replace("{scores_summary}", &scores)
        .replace("{platform.display_name}", config.display_name);
    if let Some(results) = eval["results"].as_array() {
        for (expected, heading) in [
            (true, "FAILED TO TRIGGER (should have triggered but didn't):"),
            (false, "FALSE TRIGGERS (triggered but shouldn't have):"),
        ] {
            let failures = results
                .iter()
                .filter(|r| r["should_trigger"] == expected && r["pass"] != true)
                .collect::<Vec<_>>();
            if !failures.is_empty() {
                prompt.push_str(heading);
                prompt.push('\n');
                for r in failures {
                    prompt.push_str(&format!(
                        "  - \"{}\" (triggered {}/{} times)\n",
                        r["query"].as_str().unwrap_or(""),
                        r["triggers"],
                        r["runs"]
                    ));
                }
                prompt.push('\n');
            }
        }
    }
    if !history.is_empty() {
        prompt.push_str(
            "PREVIOUS ATTEMPTS (do NOT repeat these — try something structurally different):\n\n",
        );
        for h in history {
            let passed = h.get("train_passed").unwrap_or(&h["passed"]);
            let total = h.get("train_total").unwrap_or(&h["total"]);
            let mut scores = format!("train={passed}/{total}");
            if !h["test_passed"].is_null() {
                scores.push_str(&format!(", test={}/{}", h["test_passed"], h["test_total"]));
            }
            prompt.push_str(&format!(
                "<attempt {scores}>\nDescription: \"{}\"\n",
                h["description"].as_str().unwrap_or("")
            ));
            if let Some(results) = h["results"].as_array() {
                prompt.push_str("Train results:\n");
                for r in results {
                    prompt.push_str(&format!(
                        "  [{}] \"{}\" (triggered {}/{})\n",
                        if r["pass"] == true { "PASS" } else { "FAIL" },
                        common::chars(r["query"].as_str().unwrap_or(""), 80),
                        r["triggers"],
                        r["runs"]
                    ));
                }
            }
            prompt.push_str("</attempt>\n\n");
        }
    }
    prompt.push_str(
        &suffix
            .replace("{skill_content}", content)
            .replace("{platform.display_name}", config.display_name),
    );
    prompt
}

#[allow(clippy::too_many_arguments)]
pub fn improve_description(
    config: &platform_cli::PlatformConfig,
    name: &str,
    content: &str,
    current: &str,
    eval: &Value,
    history: &[Value],
    model: Option<&str>,
    test: Option<&Value>,
    log_dir: Option<&Path>,
    iteration: Option<usize>,
) -> Result<String> {
    let prompt = improvement_prompt(config, name, content, current, eval, history, test);
    let response = platform_cli::call_model(config, &prompt, model, Duration::from_secs(300))?;
    let mut description = parse_description(&response);
    let count = description.chars().count();
    let mut transcript = json!({
        "iteration": iteration,
        "prompt": prompt,
        "response": response,
        "parsed_description": description,
        "char_count": count,
        "over_limit": count > 1024
    });
    if count > 1024 {
        let shorten = format!(
            "{prompt}\n\n---\n\nA previous attempt produced {count} characters (> 1024 limit):\n\n\"{description}\"\n\nRewrite it under 1024 characters preserving key intent coverage in <new_description> tags."
        );
        let response = platform_cli::call_model(config, &shorten, model, Duration::from_secs(300))?;
        description = parse_description(&response);
        transcript["rewrite_description"] = json!(description);
    }
    transcript["final_description"] = json!(description);
    if let Some(path) = log_dir {
        std::fs::create_dir_all(path)?;
        common::write_json(
            &path.join(format!(
                "improve_iter_{}.json",
                iteration.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
            )),
            &transcript,
            false,
        )?;
    }
    Ok(description)
}

pub fn improve_execute(v: &Value) -> Result<Value> {
    let skill = utils::parse_skill_md(Path::new(common::required(v, "skill_path")?))?;
    let eval = common::json_file(Path::new(common::required(v, "eval_results")?))?;
    let mut history = common::text(v, "history")
        .map(|p| common::json_file(Path::new(p)))
        .transpose()?
        .unwrap_or(json!([]))
        .as_array()
        .cloned()
        .ok_or_else(|| RuntimeError::invalid("History must be an array"))?;
    let config = platform_cli::platform_config(v)?;
    let model = common::text(v, "model");
    if config.key == "claude" && model.is_none() {
        return Err(RuntimeError::invalid(
            "model is required for Claude description improvement",
        ));
    }
    let current = eval["description"]
        .as_str()
        .ok_or_else(|| RuntimeError::invalid("Eval results lack description"))?;
    let next = improve_description(
        &config,
        &skill.name,
        &skill.content,
        current,
        &eval,
        &history,
        model,
        None,
        None,
        None,
    )?;
    history.push(json!({
        "description": current,
        "passed": eval["summary"]["passed"],
        "failed": eval["summary"]["failed"],
        "total": eval["summary"]["total"],
        "results": eval["results"]
    }));
    common::json_cli(json!({"description": next, "history": history}), String::new())
}

fn summary(results: &[Value]) -> Value {
    let passed = results.iter().filter(|r| r["pass"] == true).count();
    json!({"total": results.len(), "passed": passed, "failed": results.len() - passed})
}

fn score_count(h: &Value, key: &str) -> u64 {
    h[key].as_u64().unwrap_or(0)
}

fn loop_output(
    original: &str,
    current: &str,
    history: &[Value],
    holdout: f64,
    train: usize,
    test: usize,
    reason: &str,
) -> Value {
    let key = if test > 0 { "test_passed" } else { "train_passed" };
    let mut best = history.first();
    for h in history.iter().skip(1) {
        if best.is_none_or(|b| score_count(h, key) > score_count(b, key)) {
            best = Some(h);
        }
    }
    let empty = Value::Null;
    let best = best.unwrap_or(&empty);
    let train_score = format!("{}/{}", best["train_passed"], best["train_total"]);
    let test_score = format!("{}/{}", best["test_passed"], best["test_total"]);
    json!({
        "exit_reason": reason,
        "original_description": original,
        "best_description": best.get("description").and_then(Value::as_str).unwrap_or(current),
        "best_score": if test > 0 { test_score.clone() } else { train_score.clone() },
        "best_train_score": train_score,
        "best_test_score": if test > 0 { json!(test_score) } else { Value::Null },
        "final_description": current,
        "iterations_run": history.len(),
        "holdout": holdout,
        "train_size": train,
        "test_size": test,
        "split_profile": SPLIT_PROFILE,
        "history": history
    })
}

pub fn loop_execute(v: &Value) -> Result<Value> {
    let skill_path = Path::new(common::required(v, "skill_path")?);
    let skill = utils::parse_skill_md(skill_path)?;
    let eval = common::json_file(Path::new(common::required(v, "eval_set")?))?;
    let eval = eval
        .as_array()
        .ok_or_else(|| RuntimeError::invalid("Eval set must be a JSON array"))?;
    let options = evaluation::options(v)?;
    let native = options.config.key != "claude";
    let iterations = common::positive(v, "max_iterations", 5)?;
    let holdout = common::number(v, "holdout", 0.4)?;
    if native && !(0.0..1.0).contains(&holdout) {
        return Err(RuntimeError::invalid(
            "holdout must be at least 0 and less than 1",
        ));
    }
    if options.model.is_none() && !native {
        return Err(RuntimeError::invalid(
            "model is required for Claude description improvement",
        ));
    }
    let (train, test) = if holdout > 0.0 {
        split_eval_set(eval, holdout, native)?
    } else {
        (eval.clone(), vec![])
    };
    let project = platform_cli::find_project_root(
        &options.config,
        Path::new(common::text(v, "project_root").unwrap_or(".")),
    )?;
    let mut current = common::text(v, "description")
        .filter(|s| !s.is_empty())
        .unwrap_or(&skill.description)
        .to_owned();
    let mut history = vec![];
    let mut log = String::new();
    let verbose = common::flag(v, "verbose");
    let report = match common::text(v, "report").unwrap_or("none") {
        "none" => None,
        "auto" => Some(std::env::temp_dir().join(format!(
            "skill_description_report_{}_{}.html",
            skill_path.file_name().unwrap_or_default().to_string_lossy(),
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        ))),
        path => Some(PathBuf::from(path)),
    };
    let results_dir = common::text(v, "results_dir")
        .map(|p| Path::new(p).join(chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string()));
    if let Some(path) = &results_dir {
        std::fs::create_dir_all(path)?;
    }
    let logs = results_dir.as_ref().map(|p| p.join("logs"));
    let mut reason = String::new();

    for iteration in 1..=iterations {
        if cancel::interrupted() {
            reason = "interrupted".into();
            break;
        }
        let all = train.iter().chain(&test).cloned().collect::<Vec<_>>();
        let started = Instant::now();
        let all_results = evaluation::run_eval(&all, &skill.name, &current, &project, &options, &mut log)?;
        let elapsed = started.elapsed().as_secs_f64();
        let train_queries = train
            .iter()
            .map(|q| q["query"].as_str().unwrap_or(""))
            .collect::<std::collections::BTreeSet<_>>();
        let (a, b): (Vec<_>, Vec<_>) = all_results["results"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .partition(|r| train_queries.contains(r["query"].as_str().unwrap_or("")));
        let train_summary = summary(&a);
        let test_summary = if test.is_empty() { Value::Null } else { summary(&b) };
        let train_results = json!({"results": a, "summary": train_summary});
        history.push(json!({
            "iteration": iteration,
            "description": current,
            "train_passed": train_summary["passed"],
            "train_failed": train_summary["failed"],
            "train_total": train_summary["total"],
            "train_results": a,
            "test_passed": test_summary["passed"],
            "test_failed": test_summary["failed"],
            "test_total": test_summary["total"],
            "test_results": if test.is_empty() { Value::Null } else { json!(b) },
            "passed": train_summary["passed"],
            "failed": train_summary["failed"],
            "total": train_summary["total"],
            "results": a
        }));
        if let Some(path) = &report {
            let partial = json!({
                "original_description": skill.description,
                "best_description": current,
                "best_score": "in progress",
                "iterations_run": history.len(),
                "holdout": holdout,
                "train_size": train.len(),
                "test_size": test.len(),
                "history": history
            });
            std::fs::write(path, reports::generate_html(&partial, true, &skill.name))?;
        }
        if verbose {
            log.push_str(&evaluation::stats("Train", &a, elapsed));
        }
        if train_summary["failed"] == 0 {
            reason = format!("all_passed (iteration {iteration})");
            break;
        }
        if iteration == iterations {
            reason = format!("max_iterations ({iterations})");
            break;
        }
        let blind: Vec<Value> = history
            .iter()
            .map(|h| {
                Value::Object(
                    h.as_object()
                        .unwrap()
                        .iter()
                        .filter(|(k, _)| !k.starts_with("test_"))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            })
            .collect();
        current = improve_description(
            &options.config,
            &skill.name,
            &skill.content,
            &current,
            &train_results,
            &blind,
            options.model.as_deref(),
            None,
            logs.as_deref(),
            Some(iteration),
        )?;
    }

    let result = loop_output(
        &skill.description,
        &current,
        &history,
        holdout,
        train.len(),
        test.len(),
        &reason,
    );
    if let Some(path) = &report {
        std::fs::write(path, reports::generate_html(&result, false, &skill.name))?;
        log.push_str(&format!("\nReport: {}\n", path.display()));
    }
    if let Some(path) = &results_dir {
        common::write_json(&path.join("results.json"), &result, false)?;
        std::fs::write(path.join("log.txt"), &log)?;
        if report.is_some() {
            std::fs::write(
                path.join("report.html"),
                reports::generate_html(&result, false, &skill.name),
            )?;
        }
    }
    common::json_cli(result, log)
}
