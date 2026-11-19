//! Generalized provider CLI integration for Claude Code, Codex, and Cursor.
use crate::{Result, RuntimeError, common, runner};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct PlatformConfig {
    pub key: &'static str,
    pub display_name: &'static str,
    pub project_marker: &'static str,
    pub skills_path: &'static str,
    pub executable: &'static str,
    pub executable_env: &'static str,
    pub compat_env: &'static str,
}

pub fn platform_config(request: &Value) -> Result<PlatformConfig> {
    let key = common::text(request, "platform").unwrap_or("claude");
    Ok(match key {
        "codex" => PlatformConfig {
            key: "codex",
            display_name: "Codex",
            project_marker: ".agents",
            skills_path: ".agents/skills",
            executable: "codex",
            executable_env: "SKILL_FOUNDRY_CODEX_CLI",
            compat_env: "SKILL_CREATOR_CODEX_CLI",
        },
        "cursor" => PlatformConfig {
            key: "cursor",
            display_name: "Cursor",
            project_marker: ".cursor",
            skills_path: ".cursor/skills",
            executable: "cursor-agent",
            executable_env: "SKILL_FOUNDRY_CURSOR_CLI",
            compat_env: "SKILL_CREATOR_CURSOR_CLI",
        },
        "claude" => PlatformConfig {
            key: "claude",
            display_name: "Claude",
            project_marker: ".claude",
            skills_path: ".claude/commands",
            executable: "claude",
            executable_env: "SKILL_FOUNDRY_CLAUDE_CLI",
            compat_env: "SKILL_CREATOR_CLAUDE_CLI",
        },
        _ => {
            return Err(RuntimeError::invalid(
                "platform must be claude, codex, or cursor",
            ));
        }
    })
}

pub fn find_project_root(config: &PlatformConfig, start: &Path) -> Result<PathBuf> {
    let current = common::absolute(start)?;
    if common::inside_plugin_dir(&current) {
        return Err(RuntimeError::invalid(format!(
            "Refusing to resolve project root from plugin directory ({}); pass explicit 'project_root'",
            current.display()
        )));
    }
    for candidate in current.ancestors() {
        if candidate.join(config.project_marker).is_dir() {
            return Ok(candidate.into());
        }
    }
    Ok(current)
}

pub fn executable(
    config: &PlatformConfig,
    environment: &BTreeMap<String, String>,
) -> Result<String> {
    let name = environment
        .get(config.executable_env)
        .or_else(|| environment.get(config.compat_env))
        .map(String::as_str)
        .unwrap_or(config.executable);
    let available = if name.contains(std::path::MAIN_SEPARATOR) {
        Path::new(name).is_file()
    } else {
        std::env::split_paths(environment.get("PATH").map(String::as_str).unwrap_or(""))
            .any(|p| p.join(name).is_file())
    };
    if !available {
        return Err(RuntimeError::unavailable(format!(
            "{} CLI executable ({name}) not found on PATH",
            config.display_name
        )));
    }
    Ok(name.into())
}

pub fn write_probe_skill(
    config: &PlatformConfig,
    workspace: &Path,
    name: &str,
    description: &str,
) -> Result<(PathBuf, String)> {
    let directory = workspace.join(config.skills_path).join(name);
    std::fs::create_dir_all(&directory)?;
    let marker = format!("TRIGGER_PROBE_{}", uuid::Uuid::new_v4().simple());
    std::fs::write(
        directory.join("SKILL.md"),
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n# {}\n\nReply with exact token `{}` and stop.\n",
            serde_json::to_string(name)?,
            serde_json::to_string(description)?,
            name,
            marker,
        ),
    )?;
    Ok((directory, marker))
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_trigger(
    config: &PlatformConfig,
    query: &str,
    name: &str,
    description: &str,
    timeout: Duration,
    _project: &Path,
    model: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Result<bool> {
    let exe = executable(config, environment)?;
    let workspace = common::Scratch::new(&std::env::temp_dir(), "skill-trigger-probe")?;
    let (_, marker) = write_probe_skill(config, &workspace.0, name, description)?;
    let mut argv = vec![exe];
    if config.key == "codex" {
        argv.extend(
            [
                "exec",
                "--ephemeral",
                "--json",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--cd",
            ]
            .map(str::to_owned),
        );
        argv.push(workspace.0.to_string_lossy().into_owned());
    } else {
        argv.extend(["--print", "--output-format", "json"].map(str::to_owned));
    }
    if let Some(m) = model {
        argv.extend(["--model".into(), m.into()]);
    }
    argv.push(query.into());
    let output = runner::invoke(
        argv,
        &workspace.0,
        environment.clone(),
        vec![],
        timeout,
        |_| None,
    )?;
    let text = runner::success(output, &format!("{} trigger probe", config.display_name))?;
    Ok(text.contains(&marker))
}

pub fn call_model(
    config: &PlatformConfig,
    prompt: &str,
    model: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    let environment = if config.key == "claude" {
        common::claude_env()
    } else {
        common::env()
    };
    let exe = executable(config, &environment)?;
    if config.key == "claude" {
        let mut argv = vec![exe, "-p".into(), "--output-format".into(), "text".into()];
        if let Some(m) = model {
            argv.extend(["--model".into(), m.into()]);
        }
        return runner::success(
            runner::invoke(
                argv,
                &std::env::current_dir()?,
                environment,
                prompt.as_bytes().to_vec(),
                timeout,
                |_| None,
            )?,
            "claude -p",
        );
    }
    let workspace = common::Scratch::new(&std::env::temp_dir(), "skill-description-improvement")?;
    let output_path = workspace.0.join("last-message.txt");
    let mut argv = vec![exe];
    if config.key == "codex" {
        argv.extend(
            [
                "exec",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--cd",
            ]
            .map(str::to_owned),
        );
        argv.push(workspace.0.to_string_lossy().into_owned());
        argv.extend([
            "--output-last-message".into(),
            output_path.to_string_lossy().into_owned(),
        ]);
    } else {
        argv.extend(["--print", "--output-format", "json"].map(str::to_owned));
    }
    if let Some(m) = model {
        argv.extend(["--model".into(), m.into()]);
    }
    argv.push(if config.key == "codex" {
        "-".into()
    } else {
        prompt.into()
    });
    let output = runner::invoke(
        argv,
        &workspace.0,
        environment,
        if config.key == "codex" {
            prompt.as_bytes().to_vec()
        } else {
            vec![]
        },
        timeout,
        |_| None,
    )?;
    let stdout = runner::success(
        output,
        &format!("{} description improvement", config.display_name),
    )?;
    if config.key == "codex" {
        let last = if output_path.is_file() {
            common::read(&output_path)?.trim().to_owned()
        } else {
            String::new()
        };
        return Ok(if last.is_empty() {
            stdout.trim().into()
        } else {
            last
        });
    }
    let payload: Value = serde_json::from_str(&stdout)
        .map_err(|_| RuntimeError::invalid("Cursor CLI returned invalid JSON output."))?;
    payload["result"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().into())
        .ok_or_else(|| {
            RuntimeError::invalid("Cursor CLI JSON output did not contain a non-empty result.")
        })
}

pub fn execute(v: &Value) -> Result<Value> {
    let config = platform_config(v)?;
    match common::text(v, "action").unwrap_or("detect") {
        "detect" => common::json_cli(
            json!({
                "key": config.key,
                "display_name": config.display_name,
                "project_marker": config.project_marker,
                "skills_path": config.skills_path,
                "executable": config.executable,
                "project_root": find_project_root(&config, Path::new(common::text(v, "start").unwrap_or(".")))?
            }),
            String::new(),
        ),
        "evaluate-trigger" => {
            let found = evaluate_trigger(
                &config,
                common::required(v, "query")?,
                common::required(v, "skill_name")?,
                common::required(v, "description")?,
                Duration::from_secs(common::positive(v, "timeout", 60)? as u64),
                Path::new("."),
                common::text(v, "model"),
                &common::env(),
            )?;
            common::json_cli(json!({"triggered": found}), String::new())
        }
        "call-model" => common::json_cli(
            json!({
                "response": call_model(
                    &config,
                    common::required(v, "prompt")?,
                    common::text(v, "model"),
                    Duration::from_secs(common::positive(v, "timeout", 300)? as u64),
                )?
            }),
            String::new(),
        ),
        _ => Err(RuntimeError::invalid("Unknown platform action")),
    }
}
