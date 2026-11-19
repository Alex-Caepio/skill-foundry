//! Generalized token and byte budget linter for skill packs and plugins.
use crate::{Result, RuntimeError, common, validation};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub const RATIOS: [(&str, f64); 6] = [
    ("agents_md_bytes", 4.76),
    ("descriptor_bytes", 4.91),
    ("command_bytes", 5.10),
    ("agent_bytes", 5.14),
    ("frontmatter_bytes", 4.59),
    ("body_bytes", 4.23),
];

pub fn split<'a>(text: &'a str, path: &str) -> Result<(&'a str, &'a str)> {
    let first = text.find('\n').unwrap_or(text.len());
    if text[..first].trim() != "---" || first == text.len() {
        return Err(RuntimeError::invalid(format!(
            "{path}: missing frontmatter"
        )));
    }
    let start = first + 1;
    let mut position = start;
    while position <= text.len() {
        let end = text[position..]
            .find('\n')
            .map(|offset| position + offset)
            .unwrap_or(text.len());
        if text[position..end].trim() == "---" {
            let front_end = if position > start && text.as_bytes()[position - 1] == b'\n' {
                position - 1
            } else {
                position
            };
            let body_start = if end < text.len() { end + 1 } else { end };
            return Ok((&text[start..front_end], &text[body_start..]));
        }
        if end == text.len() {
            break;
        }
        position = end + 1;
    }
    Err(RuntimeError::invalid(format!(
        "{path}: missing closing frontmatter"
    )))
}

pub fn field(front: &str, key: &str) -> Option<String> {
    let mut lines = front.lines();
    let prefix = format!("{key}:");
    while let Some(line) = lines.next() {
        let Some(first) = line.strip_prefix(&prefix) else {
            continue;
        };
        let mut parts = vec![first.trim().to_string()];
        for line in lines.by_ref() {
            if !line.is_empty() && !line.starts_with(char::is_whitespace) {
                break;
            }
            parts.push(line.trim().into());
        }
        let mut value = parts.join(" ").trim().to_string();
        if value.len() >= 2 {
            let quote = value.as_bytes()[0];
            if (quote == b'\'' || quote == b'"') && value.as_bytes().last() == Some(&quote) {
                value = value[1..value.len() - 1].to_string();
                if quote == b'"' {
                    for (from, to) in [("\\n", "\n"), ("\\t", "\t"), ("\\\"", "\""), ("\\\\", "\\")]
                    {
                        value = value.replace(from, to);
                    }
                }
            }
        }
        return Some(value);
    }
    None
}

fn paragraph(body: &str) -> String {
    let mut lines = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        lines.push(line);
    }
    lines.join(" ")
}

fn listing_bytes(base: &Path, pattern: &str) -> Result<usize> {
    if !base.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for relative in common::walk(base)? {
        if relative.contains('/') || !common::glob_match(pattern, &relative) {
            continue;
        }
        let text = common::read_regular(&common::under(base, &relative)?)?;
        let (front, body) = split(&text, &relative).unwrap_or(("", &text));
        let description = field(front, "description").unwrap_or_else(|| paragraph(body));
        let stem = Path::new(&relative)
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| RuntimeError::invalid("Listing path is not UTF-8"))?;
        count += stem.len() + 1 + description.len();
    }
    Ok(count)
}

/// Measure byte and estimated cl100k token overhead of any skill, skills dir, or plugin root.
pub fn measure_dir(root: &Path, ceilings: Option<&Value>) -> Result<Value> {
    let root = common::absolute(root)?;
    if !root.exists() {
        return Err(RuntimeError::not_found(format!(
            "Target directory not found: {}",
            root.display()
        )));
    }

    let mut agents_md_bytes = 0usize;
    let mut warnings = Vec::new();
    if root.join("AGENTS.md").is_file() {
        agents_md_bytes += common::read_regular(&root.join("AGENTS.md"))?.len();
    }
    if root.join("CLAUDE.md").is_file() {
        if root.join(".claude-plugin/plugin.json").is_file() {
            warnings.push(
                "Root CLAUDE.md inside a plugin fails `claude plugin validate --strict`; move context into skills/<name>/SKILL.md"
                    .to_string(),
            );
        }
        agents_md_bytes += common::read_regular(&root.join("CLAUDE.md"))?.len();
    }

    let mut descriptor_bytes = 0usize;
    let mut frontmatter_bytes = 0usize;
    let mut body_bytes = 0usize;
    let mut skills_count = 0usize;
    let mut per_skill = Vec::new();

    for skill_dir in validation::discover_skill_dirs(&root)? {
        let md_path = skill_dir.join("SKILL.md");
        let text = common::read_regular(&md_path)?;
        let rel = skill_dir
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| skill_dir.display().to_string());
        let (front, body) = split(&text, &rel)?;
        frontmatter_bytes += front.len();
        body_bytes += body.len();
        skills_count += 1;

        let name = field(front, "name").unwrap_or_else(|| {
            skill_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_owned()
        });
        let description = field(front, "description").unwrap_or_default();
        let startup_desc = if front.contains("disable-model-invocation: true") {
            0
        } else {
            name.len() + 1 + description.len()
        };
        descriptor_bytes += startup_desc;
        per_skill.push(json!({
            "name": name,
            "path": rel,
            "descriptor_bytes": startup_desc,
            "frontmatter_bytes": front.len(),
            "body_bytes": body.len(),
            "body_lines": body.lines().count(),
            "estimated_body_tokens": (body.len() as f64 / 4.23).round_ties_even() as u64,
        }));
    }

    let mut command_bytes = 0usize;
    for cmd_dir in [root.join("commands"), root.join(".claude/commands")] {
        command_bytes += listing_bytes(&cmd_dir, "*.md")?;
    }
    let mut agent_bytes = 0usize;
    for ag_dir in [root.join("agents"), root.join(".claude/agents")] {
        agent_bytes += listing_bytes(&ag_dir, "*.md")?;
    }

    let measured = BTreeMap::from([
        ("agents_md_bytes", agents_md_bytes as u64),
        ("descriptor_bytes", descriptor_bytes as u64),
        ("command_bytes", command_bytes as u64),
        ("agent_bytes", agent_bytes as u64),
        ("frontmatter_bytes", frontmatter_bytes as u64),
        ("body_bytes", body_bytes as u64),
        ("skills", skills_count as u64),
    ]);

    let mut token_estimates = serde_json::Map::new();
    let mut startup_tokens = 0u64;
    for (key, ratio) in RATIOS {
        let val = measured[key];
        let est = (val as f64 / ratio).round_ties_even() as u64;
        token_estimates.insert(key.into(), json!(est));
        if [
            "agents_md_bytes",
            "descriptor_bytes",
            "command_bytes",
            "agent_bytes",
        ]
        .contains(&key)
        {
            startup_tokens += est;
        }
    }

    let mut findings = Vec::new();
    let mut headroom = Vec::new();
    if let Some(ceiling_obj) = ceilings.and_then(|v| v.as_object()) {
        for (key, limit_val) in ceiling_obj {
            if let (Some(limit), Some(&actual)) = (limit_val.as_u64(), measured.get(key.as_str())) {
                if actual > limit {
                    findings.push(format!("{key}: {actual} exceeds ceiling {limit}"));
                }
                headroom.push(json!({
                    "category": key,
                    "measured": actual,
                    "ceiling": limit,
                    "remaining": i128::from(limit) - i128::from(actual),
                }));
            }
        }
    }

    Ok(json!({
        "root": root,
        "passed": findings.is_empty(),
        "measured": measured,
        "estimated_tokens": token_estimates,
        "startup_tokens": startup_tokens,
        "per_skill": per_skill,
        "findings": findings,
        "warnings": warnings,
        "headroom": headroom,
    }))
}

pub fn execute(v: &Value) -> Result<Value> {
    let target = Path::new(common::text(v, "path").unwrap_or("."));
    let ceiling_value = match common::text(v, "ceiling") {
        Some(path) => Some(common::json_file(Path::new(path))?),
        None => v.get("ceilings").cloned(),
    };
    let report = measure_dir(target, ceiling_value.as_ref())?;
    common::json_cli(report, String::new())
}
