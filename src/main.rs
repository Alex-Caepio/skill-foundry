use serde_json::{Map, Value, json};
use skill_foundry::{
    Result, RuntimeError, benchmark, budget, cancel, common, cost, evaluation, mcp, optimization,
    reports, review, scaffold, validation, vetting,
};
use std::path::Path;

fn help() -> &'static str {
    "\
skill-foundry — Standalone Agent Skills Workbench for Claude Code & Codex

USAGE:
  skill-foundry <COMMAND> [OPTIONS]

COMMANDS:
  init-skill <name> [--path <dir>]
      Initialize a SKILL.md template with scripts/, references/, and assets/.
  validate <path>
      Validate a SKILL.md directory or recursively validate a plugin/skills root.
  package <skill-dir> [--out <dir>]
      Validate and bundle a skill into a portable .skill zip package.
  eval-triggers --skill <dir> --eval-set <json> [--platform claude|codex|cursor]
                [--model <m>] [--runs <n>] [--workers <n>] [--threshold <f>] [--timeout <s>]
      Evaluate skill triggering precision and recall against an eval set.
  routing-eval --skills-dir <dir> --cases <json> [--runs <n>] [--dry-run]
               [--model <m>] [--timeout <s>] [--write-baseline <file>]
      Evaluate skill selection accuracy across a multi-skill directory.
  optimize-description --skill <dir> --eval-set <json> [--platform claude|codex|cursor]
                       [--model <m>] [--max-iterations <n>] [--holdout <f>]
                       [--runs <n>] [--results-dir <dir>] [--report <path|auto|none>]
      Iteratively optimize SKILL.md description using a deterministic MT19937 holdout split.
  benchmark <workspace-dir> [--skill-name <name>] [--output <json>] [--markdown <md>]
      Aggregate eval grading/timing results into benchmark.json and Markdown summary.
  report <results.json|-> [--skill-name <name>] [--output <html-out>] [--auto-refresh]
      Render an HTML trigger optimization report matrix.
  budget-check [<path>] [--ceiling <json>]
      Measure byte and calibrated cl100k token overhead across skills, AGENTS.md, commands, and agents.
  vet-skill [<path>] [--dossier <entry.json>] [--existing <a,b>] [--strict]
      Run 8 binary security gates + 4 scored quality gates on a skill/plugin directory or validate a JSON dossier.
  cost-audit [--host all|claude|codex] [--claude-dir <dir>] [--codex-dir <dir>]
             [--project <substr>] [--since-days <n>]
      Aggregate BTE token usage and skill/MCP/tool/plugin/agent attribution across Claude & Codex logs.
  review-ui <workspace-dir> [--port <p>] [--static <out.html>] [--skill-name <n>]
            [--previous-workspace <dir>] [--benchmark <json>] [--no-browser]
      Launch interactive loopback feedback server or export a standalone review HTML file.
  eval-set-review --skill <dir> --eval-set <json> --output <out.html>
      Generate an interactive standalone HTML editor for query eval sets.
  mcp
      Start the stdio JSON-RPC 2.0 MCP server.

GLOBAL OPTIONS:
  --json       Force JSON output on stdout
  -h, --help   Show this help message
  -V, --version Show version
"
}

fn parse_args(args: &[String]) -> Result<(Map<String, Value>, Vec<String>, bool)> {
    let mut map = Map::new();
    let mut positionals = Vec::new();
    let mut json_flag = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--json" {
            json_flag = true;
            i += 1;
            continue;
        }
        if let Some(flag) = arg.strip_prefix("--") {
            let (raw_key, inline_val) = match flag.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (flag, None),
            };
            let key = raw_key.replace('-', "_");
            let bool_flags = ["dry_run", "verbose", "auto_refresh", "no_browser", "strict"];
            if bool_flags.contains(&key.as_str()) && inline_val.is_none() {
                map.insert(key, json!(true));
                i += 1;
                continue;
            }
            let val = if let Some(v) = inline_val {
                v.to_owned()
            } else {
                i += 1;
                args.get(i)
                    .ok_or_else(|| RuntimeError::invalid(format!("Missing value for --{raw_key}")))?
                    .clone()
            };
            map.insert(key, json!(val));
        } else {
            positionals.push(arg.clone());
        }
        i += 1;
    }
    map.insert("_positionals".into(), json!(positionals));
    Ok((map, positionals, json_flag))
}

fn normalize_req(cmd: &str, mut map: Map<String, Value>, pos: &[String]) -> Value {
    let set_if = |m: &mut Map<String, Value>, dst: &str, src: &str| {
        if !m.contains_key(dst)
            && let Some(v) = m.get(src).cloned()
        {
            m.insert(dst.into(), v);
        }
    };
    set_if(&mut map, "skill_path", "skill");
    set_if(&mut map, "output_dir", "out");
    set_if(&mut map, "benchmark_dir", "workspace");

    match cmd {
        "init-skill" => {
            if !map.contains_key("skill_name")
                && let Some(n) = pos.first()
            {
                map.insert("skill_name".into(), json!(n));
            }
            if !map.contains_key("path") {
                map.insert("path".into(), json!("."));
            }
        }
        "validate" | "budget-check" | "vet-skill" => {
            if !map.contains_key("path") {
                let p = pos.first().map(String::as_str).unwrap_or(".");
                map.insert("path".into(), json!(p));
            }
            if let Some(ex) = map.get("existing").and_then(Value::as_str) {
                let list: Vec<&str> = ex.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                map.insert("existing_skills".into(), json!(list));
            }
        }
        "package" | "package-skill" => {
            if !map.contains_key("skill_path")
                && let Some(p) = pos.first()
            {
                map.insert("skill_path".into(), json!(p));
            }
            if !map.contains_key("output_dir")
                && let Some(p) = pos.get(1)
            {
                map.insert("output_dir".into(), json!(p));
            }
        }
        "benchmark" | "review-ui" => {
            if !map.contains_key("workspace")
                && let Some(p) = pos.first()
            {
                map.insert("workspace".into(), json!(p));
            }
            if !map.contains_key("benchmark_dir")
                && let Some(p) = pos.first()
            {
                map.insert("benchmark_dir".into(), json!(p));
            }
        }
        "report" => {
            if !map.contains_key("input")
                && let Some(p) = pos.first()
            {
                map.insert("input".into(), json!(p));
            }
        }
        _ => {}
    }
    Value::Object(map)
}

fn dispatch(cmd: &str, args: &[String]) -> Result<(i32, String, String)> {
    if matches!(cmd, "--help" | "-h" | "help") {
        return Ok((0, help().into(), String::new()));
    }
    if matches!(cmd, "--version" | "-V" | "version") {
        return Ok((0, format!("skill-foundry {}\n", env!("CARGO_PKG_VERSION")), String::new()));
    }
    if cmd == "mcp" {
        mcp::serve()?;
        return Ok((0, String::new(), String::new()));
    }

    let (map, pos, json_flag) = parse_args(args)?;
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok((0, help().into(), String::new()));
    }
    let req = normalize_req(cmd, map, &pos);

    match cmd {
        "init-skill" => {
            let out = scaffold::execute("init-skill", &req)?;
            let code = out["_cli"]["code"].as_i64().unwrap_or(1) as i32;
            let stdout = if json_flag {
                format!("{}\n", common::pretty(&out["result"])?)
            } else {
                out["_cli"]["stdout"].as_str().unwrap_or("").to_owned()
            };
            Ok((code, stdout, out["_cli"]["stderr"].as_str().unwrap_or("").into()))
        }
        "validate" | "quick-validate" => {
            let path = common::required(&req, "path")?;
            let res = validation::validate_target(Path::new(path))?;
            let ok = res["valid"] == true;
            let stdout = format!("{}\n", common::pretty(&res)?);
            Ok((if ok { 0 } else { 1 }, stdout, String::new()))
        }
        "package" | "package-skill" => {
            let out = scaffold::execute("package-skill", &req)?;
            let code = out["_cli"]["code"].as_i64().unwrap_or(1) as i32;
            let stdout = if json_flag {
                format!("{}\n", common::pretty(&out["result"])?)
            } else {
                out["_cli"]["stdout"].as_str().unwrap_or("").to_owned()
            };
            Ok((code, stdout, out["_cli"]["stderr"].as_str().unwrap_or("").into()))
        }
        "eval-triggers" | "run-eval" => {
            let out = evaluation::execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "routing-eval" => {
            let out = evaluation::routing_eval_execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "optimize-description" | "run-loop" => {
            let out = optimization::loop_execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "benchmark" | "aggregate-benchmark" => {
            let out = benchmark::execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "report" | "generate-report" => {
            let out = reports::execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "budget-check" => {
            let out = budget::execute(&req)?;
            let ok = out["result"]["passed"] != false;
            Ok((
                if ok { 0 } else { 1 },
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "vet-skill" => {
            let out = vetting::execute(&req)?;
            let res = &out["result"];
            let dossier_fail = res.get("valid") == Some(&json!(false));
            let strict_fail = common::flag(&req, "strict") && res.get("status") == Some(&json!("known_risks"));
            let code = if dossier_fail || strict_fail { 1 } else { 0 };
            Ok((
                code,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "cost-audit" => {
            let out = cost::execute(&req)?;
            Ok((
                0,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "review-ui" | "generate-review" => {
            let out = review::execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        "eval-set-review" => {
            let out = review::eval_set_execute(&req)?;
            Ok((
                out["_cli"]["code"].as_i64().unwrap_or(0) as i32,
                out["_cli"]["stdout"].as_str().unwrap_or("").into(),
                out["_cli"]["stderr"].as_str().unwrap_or("").into(),
            ))
        }
        other => Err(RuntimeError::invalid(format!(
            "Unknown command '{other}'. Run `skill-foundry --help`."
        ))),
    }
}

fn main() {
    let _signal = cancel::Scope::install();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first() else {
        print!("{}", help());
        std::process::exit(0);
    };
    match dispatch(cmd, &argv[1..]) {
        Ok((code, stdout, stderr)) => {
            if !stdout.is_empty() {
                print!("{stdout}");
            }
            if !stderr.is_empty() {
                eprint!("{stderr}");
            }
            std::process::exit(code);
        }
        Err(err) => {
            eprintln!("Error: {err}");
            let code = if cancel::interrupted() { 130 } else { 2 };
            std::process::exit(code);
        }
    }
}
