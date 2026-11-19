//! Skill & third-party tool security vetting (8 binary gates + 4 scored gates).
//! Supports BOTH JSON vetting dossier validation AND automated static inspection of skill directories.
use crate::{Result, RuntimeError, budget, common, validation};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const BINARY: [&str; 8] = [
    "license",
    "data_egress",
    "pinning",
    "uninstall",
    "collisions",
    "auto_update",
    "maintenance_ownership",
    "measurability",
];

pub const SCORED: [&str; 4] = [
    "automation_depth",
    "token_efficiency",
    "integration_coverage",
    "trust_signals",
];

const REQUIRED: [&str; 16] = [
    "schema_version",
    "id",
    "catalog_id",
    "name",
    "url",
    "reviewed_date",
    "reviewed_by",
    "status",
    "status_summary",
    "tier",
    "default_state",
    "install",
    "binary_gates",
    "scored_gates",
    "intersection_map",
    "lifecycle",
];

pub fn stated(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.trim().is_empty())
}

pub fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn computed_status(gates: &Value) -> &'static str {
    if gates
        .as_object()
        .into_iter()
        .flat_map(|v| v.values())
        .any(|g| g["status"] == "fail")
    {
        "known_risks"
    } else if gates
        .as_object()
        .into_iter()
        .flat_map(|v| v.values())
        .any(|g| g["status"] == "unknown")
    {
        "open_questions"
    } else {
        "clear"
    }
}

pub fn validate_entry(data: &Value, catalog: Option<&BTreeMap<String, Value>>) -> Vec<String> {
    let Some(map) = data.as_object() else {
        return vec!["registry entry must contain a JSON object".into()];
    };
    let id = data["id"].as_str().unwrap_or("<no id>");
    let mut errors = Vec::new();
    let mut push = |message: String| errors.push(format!("{id}: {message}"));
    for field in &REQUIRED {
        if !map.contains_key(*field) {
            push(format!("missing required field '{field}'"));
        }
    }
    if !errors.is_empty() {
        return errors;
    }
    let mut raw = Vec::<String>::new();
    for field in ["id", "catalog_id"] {
        if data[field].as_str().is_none_or(|v| !safe_id(v)) {
            raw.push(format!("{field} must be a filename-safe lowercase slug"));
        }
    }
    if data["schema_version"].as_u64() != Some(1) {
        raw.push(format!("unsupported schema_version {}", data["schema_version"]));
    }
    if !matches!(data["tier"].as_u64(), Some(1..=3)) {
        raw.push("tier must be one of [1, 2, 3]".into());
    }
    if !matches!(data["default_state"].as_str(), Some("enabled" | "disabled")) {
        raw.push("default_state must be enabled or disabled".into());
    }
    if !matches!(
        data["status"].as_str(),
        Some("clear" | "open_questions" | "known_risks")
    ) {
        raw.push("status must be clear, open_questions or known_risks".into());
    }
    if let Some(catalog) = catalog {
        if let Some(expected) = data["catalog_id"].as_str().and_then(|k| catalog.get(k)) {
            if data["id"] != data["catalog_id"] {
                raw.push("id does not match catalog_id".into());
            }
            for field in ["name", "url"] {
                if data[field] != expected[field] {
                    raw.push(format!("{field} does not match catalog value"));
                }
            }
        } else {
            raw.push("catalog_id is not present in provided catalog".into());
        }
    }
    for block in ["install", "binary_gates", "scored_gates", "lifecycle"] {
        if !data[block].is_object() {
            raw.push(format!("'{block}' must be an object"));
        }
    }
    if !raw.is_empty() {
        return raw.into_iter().map(|m| format!("{id}: {m}")).collect();
    }
    for field in ["method", "pinned_ref", "namespace_prefix", "command"] {
        if data["install"].get(field).is_none() {
            raw.push(format!("install block missing '{field}'"));
        }
    }
    for field in ["uninstall_command", "residue", "manifest_tracked"] {
        if data["lifecycle"].get(field).is_none() {
            raw.push(format!("lifecycle block missing '{field}'"));
        }
    }
    let binary = data["binary_gates"].as_object().cloned().unwrap_or_default();
    let scored = data["scored_gates"].as_object().cloned().unwrap_or_default();
    for (name, expected, actual) in [
        ("binary_gates", BINARY.as_slice(), &binary),
        ("scored_gates", SCORED.as_slice(), &scored),
    ] {
        let wanted: BTreeSet<_> = expected.iter().copied().collect();
        let found: BTreeSet<_> = actual.keys().map(String::as_str).collect();
        let missing: Vec<_> = wanted.difference(&found).copied().collect();
        let extra: Vec<_> = found.difference(&wanted).copied().collect();
        if !missing.is_empty() {
            raw.push(format!("{name} missing: {missing:?}"));
        }
        if !extra.is_empty() {
            raw.push(format!("{name} has unknown gates: {extra:?}"));
        }
    }
    for (name, gate) in &binary {
        if !BINARY.contains(&name.as_str()) {
            continue;
        }
        let status = gate["status"].as_str().unwrap_or("");
        if !["pass", "fail", "unknown"].contains(&status) {
            raw.push(format!("gate '{name}' status must be pass, fail or unknown"));
            continue;
        }
        if !stated(&gate["evidence"]) {
            raw.push(format!("gate '{name}' has no evidence"));
        }
        if ["fail", "unknown"].contains(&status) && !stated(&gate["resolves_by"]) {
            raw.push(format!("gate '{name}' is {status} but has no resolves_by"));
        }
    }
    for (name, gate) in &scored {
        if !SCORED.contains(&name.as_str()) {
            continue;
        }
        if !matches!(gate["score"].as_u64(), Some(0..=5)) {
            raw.push(format!("scored gate '{name}' score must be an int 0-5"));
        }
        if !stated(&gate["evidence"]) {
            raw.push(format!("scored gate '{name}' has no evidence"));
        }
    }
    if let Some(intersections) = data["intersection_map"].as_array().filter(|r| !r.is_empty()) {
        for (index, row) in intersections.iter().enumerate() {
            for field in ["ours", "relation", "detail"] {
                if !stated(&row[field]) {
                    raw.push(format!("intersection_map[{index}] missing '{field}'"));
                }
            }
            if !matches!(
                row["relation"].as_str(),
                Some("duplicates" | "replaces" | "conflicts" | "complements")
            ) {
                raw.push(format!("intersection_map[{index}] has invalid relation"));
            }
        }
    } else {
        raw.push("intersection_map must be a non-empty list".into());
    }
    if (data["scored_gates"]["token_efficiency"]["score"].as_u64() == Some(0)
        || data["scored_gates"]["token_efficiency"]["score"] == false)
        && data["binary_gates"]["measurability"]["status"] == "pass"
    {
        raw.push(
            "token_efficiency scores 0 (unmeasured) but measurability passes - contradictory".into(),
        );
    }
    if raw.is_empty() {
        let status = computed_status(&data["binary_gates"]);
        if data["status"] != status {
            raw.push(format!(
                "stored status disagrees with gates (expected '{status}')"
            ));
        }
    }
    raw.into_iter().map(|m| format!("{id}: {m}")).collect()
}

/// Automated security and quality scan across a live skill directory or plugin tree.
pub fn vet_directory(path: &Path, existing_skills: &[String]) -> Result<Value> {
    let root = common::absolute(path)?;
    if !root.exists() {
        return Err(RuntimeError::not_found(format!(
            "Target path not found: {}",
            root.display()
        )));
    }
    let files = common::walk(&root)?;
    let skill_dirs = validation::discover_skill_dirs(&root)?;
    let budget_report = budget::measure_dir(&root, None).ok();

    let mut has_license_file = root.join("LICENSE").is_file() || root.join("LICENSE.txt").is_file();
    let mut frontmatter_license = None::<String>;
    let mut skill_names = Vec::new();
    let mut collisions = Vec::new();

    for sd in &skill_dirs {
        let val = validation::validate_skill_detailed(sd)?;
        if !val.name.is_empty() {
            if existing_skills.contains(&val.name) {
                collisions.push(format!("Collision with existing skill '{}'", val.name));
            }
            skill_names.push(val.name);
        }
        if let Ok(raw) = common::read_regular(&sd.join("SKILL.md"))
            && let Ok((front, _)) = budget::split(&raw, &sd.display().to_string())
            && let Some(lic) = budget::field(front, "license")
            && !lic.is_empty()
        {
            has_license_file = true;
            frontmatter_license = Some(lic);
        }
    }

    // Static risk patterns (pure standard regex without lookaround)
    let unpinned_re = Regex::new(
        r"(?i)(?:curl\s+[^\n]*\|\s*(?:sh|bash)|wget\s+[^\n]*\|\s*(?:sh|bash)|npx\s+(?:-y\s+)?@?[\w./-]+@latest)",
    )?;
    let pip_re = Regex::new(r"(?im)pip3?\s+install\s+([A-Za-z0-9_.\-\[\]]+)")?;
    let egress_re = Regex::new(
        r"(?i)(?:curl\s+[^\n]*(?:-d|--data|-F|-X\s*POST|https?://)|wget\s+[^\n]*https?://|\bnc\s+|\bscp\s+|webhook|requests\.post\(|fetch\(\s*[\x22']https?://)",
    )?;
    let autoupdate_re = Regex::new(
        r"(?i)(?:--auto-?update|auto_update\s*[:=]\s*true|\bself[-_]update\b|\bgit\s+pull\b|\bnpm\s+update\b|pip3?\s+install\s+(?:-U|--upgrade)\b)",
    )?;
    let secret_re = Regex::new(
        r"(?:ghp_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN (?:RSA|OPENSSH|EC) PRIVATE KEY-----)",
    )?;
    let destructive_re = Regex::new(r"(?i)(?:rm\s+-rf\s+/|chmod\s+777|DROP\s+DATABASE)")?;

    let mut unpinned_hits = Vec::new();
    let mut egress_hits = Vec::new();
    let mut autoupdate_hits = Vec::new();
    let mut secret_hits = Vec::new();
    let mut destructive_hits = Vec::new();
    let mut script_files = Vec::new();
    let mut has_hooks = false;
    let mut has_mcp = false;

    for rel in &files {
        if rel.starts_with("tests/")
            || rel.starts_with("vendor/")
            || rel.starts_with("src/")
            || rel.starts_with("LICENSE")
        {
            continue;
        }
        if rel.ends_with("hooks.json") || rel.contains("hooks/") {
            has_hooks = true;
        }
        if rel.ends_with(".mcp.json") || rel.ends_with(".codex-mcp.json") {
            has_mcp = true;
        }
        if rel.ends_with(".sh")
            || rel.ends_with(".py")
            || rel.ends_with(".js")
            || rel.ends_with(".mjs")
            || rel.ends_with(".ts")
        {
            script_files.push(rel.clone());
        }
        let scan_text = rel.ends_with(".md")
            || rel.ends_with(".sh")
            || rel.ends_with(".py")
            || rel.ends_with(".js")
            || rel.ends_with(".mjs")
            || rel.ends_with(".ts")
            || rel.ends_with(".json")
            || rel.ends_with(".yaml")
            || rel.ends_with(".yml")
            || rel.ends_with(".toml");
        if !scan_text {
            continue;
        }
        let full = root.join(rel);
        let Ok(text) = common::read_regular(&full) else {
            continue;
        };
        for m in unpinned_re.find_iter(&text).take(5) {
            unpinned_hits.push(format!("{rel}: {}", common::chars(m.as_str(), 80)));
        }
        for cap in pip_re.captures_iter(&text).take(5) {
            let spec = &cap[1];
            if !spec.starts_with('-') && !spec.contains("==") {
                unpinned_hits.push(format!("{rel}: {}", common::chars(&cap[0], 80)));
            }
        }
        for m in egress_re.find_iter(&text).take(8) {
            let s = m.as_str();
            if !s.contains("localhost") && !s.contains("127.0.0.1") && !s.contains("example.com") {
                egress_hits.push(format!("{rel}: {}", common::chars(s, 80)));
            }
        }
        if autoupdate_re.is_match(&text) {
            autoupdate_hits.push(rel.clone());
        }
        if secret_re.is_match(&text) {
            secret_hits.push(rel.clone());
        }
        if destructive_re.is_match(&text) {
            destructive_hits.push(rel.clone());
        }
    }

    let license_gate = if has_license_file {
        json!({
            "status": "pass",
            "evidence": format!("License declared ({})", frontmatter_license.as_deref().unwrap_or("LICENSE file present")),
        })
    } else {
        json!({
            "status": "unknown",
            "evidence": "No LICENSE file or SKILL.md license frontmatter detected",
            "resolves_by": "Add LICENSE file or SPDX 'license:' frontmatter field",
        })
    };

    let data_egress_gate = if !secret_hits.is_empty() {
        json!({
            "status": "fail",
            "evidence": format!("Potential secret/token material in: {}", secret_hits.join(", ")),
            "resolves_by": "Remove embedded secrets or move to environment variables",
        })
    } else if !egress_hits.is_empty() {
        json!({
            "status": "unknown",
            "evidence": format!("External URLs or HTTP egress references found ({} matches): {}", egress_hits.len(), egress_hits[..egress_hits.len().min(3)].join("; ")),
            "resolves_by": "Verify external hosts are documentation-only or user-consented endpoints",
        })
    } else {
        json!({
            "status": "pass",
            "evidence": "No outbound network transfer commands or non-local egress patterns found",
        })
    };

    let pinning_gate = if !unpinned_hits.is_empty() {
        json!({
            "status": "fail",
            "evidence": format!("Unpinned remote execution/install patterns: {}", unpinned_hits.join("; ")),
            "resolves_by": "Pin exact package versions or checksums; remove curl|sh patterns",
        })
    } else {
        json!({
            "status": "pass",
            "evidence": "No unpinned @latest/pip/curl-pipe-shell execution patterns found",
        })
    };

    let uninstall_gate = if !destructive_hits.is_empty() {
        json!({
            "status": "fail",
            "evidence": format!("Destructive shell commands detected in: {}", destructive_hits.join(", ")),
            "resolves_by": "Remove destructive shell operations or isolate to reviewed dry-runs",
        })
    } else {
        json!({
            "status": "pass",
            "evidence": if files.iter().any(|f| f == "README.md" || f.ends_with("/README.md")) {
                "Self-contained skill/plugin directory with README.md; cleanly removable as one unit"
            } else {
                "Self-contained files with no external side-effect installers detected"
            }
        })
    };

    let collisions_gate = if collisions.is_empty() {
        json!({
            "status": "pass",
            "evidence": format!("No name collisions across {} discovered skills ({:?})", skill_names.len(), skill_names),
        })
    } else {
        json!({
            "status": "fail",
            "evidence": collisions.join("; "),
            "resolves_by": "Rename conflicting skill(s) or namespace prefix",
        })
    };

    let auto_update_gate = if !autoupdate_hits.is_empty() {
        json!({
            "status": "fail",
            "evidence": format!("Self-update / git-pull / upgrade patterns found in: {}", autoupdate_hits.join(", ")),
            "resolves_by": "Remove implicit auto-update steps so upgrades stay explicit",
        })
    } else {
        json!({
            "status": "pass",
            "evidence": "No self-update or silent upgrade behavior detected",
        })
    };

    let has_author_meta = root.join(".claude-plugin/plugin.json").is_file()
        || root.join(".codex-plugin/plugin.json").is_file()
        || !skill_dirs.is_empty();
    let ownership_gate = if has_author_meta {
        json!({
            "status": "pass",
            "evidence": "Maintained manifest/skill metadata present on disk",
        })
    } else {
        json!({
            "status": "unknown",
            "evidence": "No plugin manifest or SKILL.md ownership metadata found",
            "resolves_by": "Add SKILL.md frontmatter or plugin.json author metadata",
        })
    };

    let (measurability_gate, token_efficiency_score) = if let Some(ref b) = budget_report {
        let startup = b["startup_tokens"].as_u64().unwrap_or(0);
        let eff = if startup <= 300 {
            5
        } else if startup <= 800 {
            4
        } else if startup <= 2000 {
            3
        } else if startup <= 4000 {
            2
        } else {
            1
        };
        (
            json!({
                "status": "pass",
                "evidence": format!("Measured startup_tokens={startup}, skills={}", b["measured"]["skills"]),
            }),
            json!({
                "score": eff,
                "evidence": format!("Estimated startup overhead is {startup} cl100k tokens"),
            }),
        )
    } else {
        (
            json!({
                "status": "unknown",
                "evidence": "Token footprint unmeasured",
                "resolves_by": "Run skill-foundry budget-check on target",
            }),
            json!({
                "score": 0,
                "evidence": "Token budget unavailable",
            }),
        )
    };

    let binary_gates = json!({
        "license": license_gate,
        "data_egress": data_egress_gate,
        "pinning": pinning_gate,
        "uninstall": uninstall_gate,
        "collisions": collisions_gate,
        "auto_update": auto_update_gate,
        "maintenance_ownership": ownership_gate,
        "measurability": measurability_gate,
    });

    let automation_score = if has_hooks { 4 } else if has_mcp || !script_files.is_empty() { 3 } else { 2 };
    let integration_score = {
        let claude = root.join(".claude-plugin/plugin.json").is_file() || root.join(".claude").is_dir();
        let codex = root.join(".codex-plugin/plugin.json").is_file() || root.join(".agents").is_dir() || !skill_dirs.is_empty();
        if claude && codex { 5 } else if claude || codex { 3 } else { 2 }
    };
    let trust_score = {
        let fails = BINARY
            .iter()
            .filter(|k| binary_gates[**k]["status"] == "fail")
            .count();
        let unknowns = BINARY
            .iter()
            .filter(|k| binary_gates[**k]["status"] == "unknown")
            .count();
        (5usize).saturating_sub(fails * 2 + unknowns).max(1)
    };

    let scored_gates = json!({
        "automation_depth": {
            "score": automation_score,
            "evidence": format!("hooks={has_hooks}, mcp={has_mcp}, scripts={}", script_files.len()),
        },
        "token_efficiency": token_efficiency_score,
        "integration_coverage": {
            "score": integration_score,
            "evidence": "Checked .claude-plugin, .codex-plugin, .agents, and skills/ compatibility",
        },
        "trust_signals": {
            "score": trust_score,
            "evidence": format!("Static scan over {} files", files.len()),
        }
    });

    let status = computed_status(&binary_gates);
    Ok(json!({
        "target": root,
        "status": status,
        "skills_scanned": skill_names,
        "files_scanned": files.len(),
        "binary_gates": binary_gates,
        "scored_gates": scored_gates,
    }))
}

pub fn execute(v: &Value) -> Result<Value> {
    if let Some(dossier_path) = common::text(v, "dossier") {
        let data = common::json_file(Path::new(dossier_path))?;
        let errors = validate_entry(&data, None);
        return common::json_cli(
            json!({
                "valid": errors.is_empty(),
                "computed_status": computed_status(&data["binary_gates"]),
                "errors": errors,
            }),
            String::new(),
        );
    }
    let target = Path::new(common::text(v, "path").unwrap_or("."));
    let existing: Vec<String> = v
        .get("existing_skills")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let result = vet_directory(target, &existing)?;
    common::json_cli(result, String::new())
}
