//! Stdio JSON-RPC 2.0 MCP server for skill-foundry tools.
use crate::{
    Result, RuntimeError, budget, cost, evaluation, optimization, scaffold, validation, vetting,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn tools() -> Vec<Value> {
    let spec = |name: &str, description: &str, properties: Value, required: &[&str], read_only: bool| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": read_only,
                "destructiveHint": false,
                "openWorldHint": false
            }
        })
    };

    vec![
        spec(
            "skill_scaffold",
            "Initialize a new SKILL.md skeleton directory with scripts/, references/, and assets/.",
            json!({
                "name": {"type": "string", "description": "kebab-case skill name"},
                "path": {"type": "string", "description": "parent directory where the skill directory will be created"}
            }),
            &["name", "path"],
            false,
        ),
        spec(
            "skill_validate",
            "Validate a skill directory or discover and validate all skills under a plugin/skills root.",
            json!({
                "path": {"type": "string", "description": "path to a skill folder or directory containing skills"}
            }),
            &["path"],
            true,
        ),
        spec(
            "skill_package",
            "Validate and package a skill directory into a portable .skill zip archive.",
            json!({
                "skill_path": {"type": "string", "description": "path to the skill directory"},
                "output_dir": {"type": "string", "description": "destination directory for the .skill archive"}
            }),
            &["skill_path"],
            false,
        ),
        spec(
            "skill_eval_triggers",
            "Measure trigger precision/recall for a SKILL.md description against positive and negative queries using claude, codex, or cursor CLI.",
            json!({
                "skill_path": {"type": "string"},
                "eval_set": {"type": "string", "description": "path to eval_set JSON array [{query, should_trigger}]"},
                "platform": {"type": "string", "enum": ["claude", "codex", "cursor"]},
                "model": {"type": "string"},
                "project_root": {"type": "string"},
                "description": {"type": "string"},
                "runs_per_query": {"type": "integer", "minimum": 1},
                "num_workers": {"type": "integer", "minimum": 1},
                "trigger_threshold": {"type": "number"},
                "timeout": {"type": "integer", "minimum": 1}
            }),
            &["skill_path", "eval_set"],
            true,
        ),
        spec(
            "skill_optimize_description",
            "Run iterative prompt-description optimization with deterministic train/holdout split (CPython MT19937 seed 42).",
            json!({
                "skill_path": {"type": "string"},
                "eval_set": {"type": "string"},
                "platform": {"type": "string", "enum": ["claude", "codex", "cursor"]},
                "model": {"type": "string"},
                "project_root": {"type": "string"},
                "description": {"type": "string"},
                "max_iterations": {"type": "integer", "minimum": 1},
                "holdout": {"type": "number"},
                "runs_per_query": {"type": "integer", "minimum": 1},
                "timeout": {"type": "integer", "minimum": 1},
                "results_dir": {"type": "string"},
                "report": {"type": "string"}
            }),
            &["skill_path", "eval_set"],
            false,
        ),
        spec(
            "skill_budget_check",
            "Measure byte footprint and calibrated cl100k token overhead across skills, AGENTS.md, commands, and agents, optionally checking JSON ceilings.",
            json!({
                "path": {"type": "string", "description": "skill pack, plugin root, or directory path"},
                "ceiling": {"type": "string", "description": "optional path to JSON ceilings map"},
                "ceilings": {"type": "object", "description": "optional inline ceilings object"}
            }),
            &["path"],
            true,
        ),
        spec(
            "skill_vet",
            "Run 8 binary security/risk gates + 4 scored quality gates against a skill/plugin directory or validate a JSON vetting dossier.",
            json!({
                "path": {"type": "string", "description": "explicit target skill or plugin directory to scan (do not rely on process cwd under Codex)"},
                "dossier": {"type": "string", "description": "optional Kit-3 JSON dossier file to validate instead of directory scan"},
                "existing_skills": {"type": "array", "items": {"type": "string"}, "description": "existing skill names to check for collisions"}
            }),
            &[],
            true,
        ),
        spec(
            "skill_cost_audit",
            "Aggregate billed token equivalent (BTE) cost and skill/MCP/tool/plugin/agent attribution across Claude Code (~/.claude/projects) and Codex CLI (~/.codex/sessions) logs.",
            json!({
                "host": {"type": "string", "enum": ["all", "claude", "codex"]},
                "claude_dir": {"type": "string"},
                "codex_dir": {"type": "string"},
                "project": {"type": "string", "description": "substring filter on project path"},
                "since_days": {"type": "integer", "minimum": 1}
            }),
            &[],
            true,
        ),
    ]
}

fn ensure_explicit_path_outside_plugin_cwd(args: &Value, field: &str) -> Result<()> {
    let Ok(cwd) = std::env::current_dir() else {
        return Ok(());
    };
    if !crate::common::inside_plugin_dir(&cwd) {
        return Ok(());
    }
    let val = crate::common::text(args, field).map(str::trim);
    match val {
        None | Some("") | Some(".") | Some("./") => Err(RuntimeError::invalid(format!(
            "MCP server cwd is a plugin directory ({}); pass an explicit '{field}' path",
            cwd.display()
        ))),
        Some(p) if !std::path::Path::new(p).is_absolute() => {
            let rel = std::path::Path::new(p);
            let only_cur_dir = rel
                .components()
                .all(|c| matches!(c, std::path::Component::CurDir));
            if only_cur_dir || cwd.join(rel) == cwd {
                Err(RuntimeError::invalid(format!(
                    "MCP server cwd is a plugin directory ({}); pass an explicit '{field}' path",
                    cwd.display()
                )))
            } else {
                Ok(())
            }
        }
        Some(_) => Ok(()),
    }
}

pub fn call_tool(name: &str, args: &Value) -> Result<Value> {
    if !args.is_object() {
        return Err(RuntimeError::invalid("Tool arguments must be a JSON object"));
    }
    match name {
        "skill_scaffold" => {
            crate::common::required(args, "name")?;
            crate::common::required(args, "path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "path")?;
            let mut req = args.clone();
            if let Some(n) = args.get("name").cloned() {
                req["skill_name"] = n;
            }
            let out = scaffold::execute("init-skill", &req)?;
            if out["_cli"]["code"] != 0 {
                return Err(RuntimeError::invalid(
                    out["_cli"]["stderr"]
                        .as_str()
                        .unwrap_or("scaffold failed")
                        .trim()
                        .to_string(),
                ));
            }
            Ok(out["result"].clone())
        }
        "skill_validate" => {
            let path = crate::common::required(args, "path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "path")?;
            validation::validate_target(std::path::Path::new(path))
        }
        "skill_package" => {
            crate::common::required(args, "skill_path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "skill_path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "output_dir")?;
            let out = scaffold::execute("package-skill", args)?;
            if out["_cli"]["code"] != 0 {
                return Err(RuntimeError::invalid(
                    out["_cli"]["stdout"].as_str().unwrap_or("package failed"),
                ));
            }
            Ok(out["result"].clone())
        }
        "skill_eval_triggers" => {
            crate::common::required(args, "skill_path")?;
            crate::common::required(args, "eval_set")?;
            ensure_explicit_path_outside_plugin_cwd(args, "skill_path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "eval_set")?;
            ensure_explicit_path_outside_plugin_cwd(args, "project_root")?;
            let out = evaluation::execute(args)?;
            Ok(out["result"].clone())
        }
        "skill_optimize_description" => {
            crate::common::required(args, "skill_path")?;
            crate::common::required(args, "eval_set")?;
            ensure_explicit_path_outside_plugin_cwd(args, "skill_path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "eval_set")?;
            ensure_explicit_path_outside_plugin_cwd(args, "project_root")?;
            let out = optimization::loop_execute(args)?;
            Ok(out["result"].clone())
        }
        "skill_budget_check" => {
            crate::common::required(args, "path")?;
            ensure_explicit_path_outside_plugin_cwd(args, "path")?;
            let out = budget::execute(args)?;
            Ok(out["result"].clone())
        }
        "skill_vet" => {
            if crate::common::text(args, "dossier").is_none() {
                ensure_explicit_path_outside_plugin_cwd(args, "path")?;
            }
            if crate::common::text(args, "path").is_none()
                && crate::common::text(args, "dossier").is_none()
            {
                return Err(RuntimeError::invalid(
                    "Missing 'path' (explicit workspace/skill directory) or 'dossier'",
                ));
            }
            let out = vetting::execute(args)?;
            Ok(out["result"].clone())
        }
        "skill_cost_audit" => {
            let out = cost::execute(args)?;
            Ok(out["result"].clone())
        }
        _ => Err(RuntimeError::invalid(format!("Unknown tool: {name}"))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Framing {
    Newline,
    ContentLength,
}

fn read_frame<R: BufRead>(reader: &mut R) -> Result<Option<(String, Framing)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.to_ascii_lowercase().starts_with("content-length:") {
            let len: usize = trimmed
                .split_once(':')
                .map(|(_, v)| v.trim())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| RuntimeError::invalid("Invalid Content-Length header"))?;
            // Consume remaining headers until blank line
            loop {
                line.clear();
                let h = reader.read_line(&mut line)?;
                if h == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            let body = String::from_utf8(buf)
                .map_err(|_| RuntimeError::invalid("Non-UTF8 MCP frame"))?;
            return Ok(Some((body, Framing::ContentLength)));
        }
        return Ok(Some((trimmed.to_owned(), Framing::Newline)));
    }
}

fn write_frame<W: Write>(writer: &mut W, payload: &Value, framing: Framing) -> Result<()> {
    let body = serde_json::to_string(payload)?;
    match framing {
        Framing::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        }
        Framing::Newline => {
            writeln!(writer, "{body}")?;
        }
    }
    writer.flush()?;
    Ok(())
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut initialized = false;

    while let Some((raw, framing)) = read_frame(&mut reader)? {
        let req: Value = match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => {
                write_frame(
                    &mut writer,
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
                    framing,
                )?;
                continue;
            }
        };
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        if method == "notifications/initialized" && id.is_none() {
            continue;
        }
        let Some(id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => {
                initialized = true;
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "skill-foundry", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "Use skill_validate, skill_budget_check, skill_vet, skill_scaffold, skill_package, skill_eval_triggers, skill_optimize_description, and skill_cost_audit to develop and audit Agent Skills. Always pass explicit workspace/skill paths (under Codex, server cwd may be the plugin root)."
                    }
                })
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            _ if !initialized => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Server not initialized"}
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tools()}
            }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                match call_tool(name, &arguments) {
                    Ok(value) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": serde_json::to_string_pretty(&value)?}],
                            "structuredContent": value,
                            "isError": false
                        }
                    }),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": err.to_string()}],
                            "isError": true
                        }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            }),
        };

        write_frame(&mut writer, &response, framing)?;
    }
    Ok(())
}
