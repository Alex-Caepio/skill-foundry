//! Generalized SKILL.md frontmatter, layout, and link validator.
use crate::{Result, common};
use regex::Regex;
use serde_json::{Value, json};
use serde_yaml::{Mapping, Value as Yaml};
use std::path::{Path, PathBuf};

const ALLOWED: [&str; 14] = [
    "allowed-tools",
    "argument-hint",
    "compatibility",
    "description",
    "disable-model-invocation",
    "flow-alternatives",
    "flow-next",
    "license",
    "metadata",
    "model",
    "name",
    "phase",
    "user-invocable",
    "version",
];

pub fn is_valid_skill_name(name: &str) -> bool {
    name.chars().count() <= 64
        && !name.is_empty()
        && name.split('-').all(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn value<'a>(map: &'a Mapping, key: &str) -> Option<&'a Yaml> {
    map.get(Yaml::String(key.into()))
}

fn kind(v: &Yaml) -> &'static str {
    match v {
        Yaml::Null => "NoneType",
        Yaml::Bool(_) => "bool",
        Yaml::Number(n) if n.as_i64().is_some() || n.as_u64().is_some() => "int",
        Yaml::Number(_) => "float",
        Yaml::String(_) => "str",
        Yaml::Sequence(_) => "list",
        Yaml::Mapping(_) => "dict",
        Yaml::Tagged(t) => kind(&t.value),
    }
}

fn truthy(v: &Yaml) -> bool {
    match v {
        Yaml::Null => false,
        Yaml::Bool(b) => *b,
        Yaml::String(s) => !s.is_empty(),
        Yaml::Sequence(s) => !s.is_empty(),
        Yaml::Mapping(s) => !s.is_empty(),
        Yaml::Number(n) => n.as_f64() != Some(0.0),
        Yaml::Tagged(t) => truthy(&t.value),
    }
}

pub fn parse_frontmatter(text: &str) -> std::result::Result<Yaml, serde_yaml::Error> {
    serde_yaml::from_str(text)
}

#[derive(Clone, Debug)]
pub struct SkillValidation {
    pub path: PathBuf,
    pub name: String,
    pub valid: bool,
    pub message: String,
    pub warnings: Vec<String>,
    pub lines: usize,
    pub bytes: usize,
}

impl SkillValidation {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "name": self.name,
            "valid": self.valid,
            "message": self.message,
            "warnings": self.warnings,
            "lines": self.lines,
            "bytes": self.bytes,
        })
    }
}

pub fn validate_skill_detailed(path: &Path) -> Result<SkillValidation> {
    let md = path.join("SKILL.md");
    let mut warnings = Vec::new();
    let dir_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();

    let fail = |msg: String, warnings: Vec<String>, lines: usize, bytes: usize| {
        Ok(SkillValidation {
            path: path.to_path_buf(),
            name: dir_name.clone(),
            valid: false,
            message: msg,
            warnings,
            lines,
            bytes,
        })
    };

    if !md.exists() {
        return fail("SKILL.md not found".into(), warnings, 0, 0);
    }
    let content = common::read(&md)?;
    let bytes = content.len();
    let lines = content.lines().count();

    if !content.starts_with("---") {
        return fail("No YAML frontmatter found".into(), warnings, lines, bytes);
    }
    let Some(rest) = content.strip_prefix("---\n") else {
        return fail("Invalid frontmatter format".into(), warnings, lines, bytes);
    };
    let Some(end) = rest.find("\n---") else {
        return fail("Invalid frontmatter format".into(), warnings, lines, bytes);
    };
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');

    let parsed = match parse_frontmatter(front) {
        Ok(p) => p,
        Err(e) => return fail(format!("Invalid YAML in frontmatter: {e}"), warnings, lines, bytes),
    };
    let Some(map) = parsed.as_mapping() else {
        return fail(
            "Frontmatter must be a YAML dictionary".into(),
            warnings,
            lines,
            bytes,
        );
    };

    let mut unexpected = map
        .keys()
        .filter_map(|k| match k.as_str() {
            Some(k) if ALLOWED.contains(&k) => None,
            Some(k) => Some(k.to_owned()),
            None => Some(format!("{k:?}")),
        })
        .collect::<Vec<_>>();
    unexpected.sort();
    if !unexpected.is_empty() {
        return fail(
            format!(
                "Unexpected key(s) in SKILL.md frontmatter: {}. Allowed properties are: {}",
                unexpected.join(", "),
                ALLOWED.join(", ")
            ),
            warnings,
            lines,
            bytes,
        );
    }

    for field in ["name", "description"] {
        if value(map, field).is_none() {
            return fail(
                format!("Missing '{field}' in frontmatter"),
                warnings,
                lines,
                bytes,
            );
        }
    }

    if let Some(p) = value(map, "phase")
        && !p
            .as_str()
            .is_some_and(|p| ["planning", "execution", "quality", "utility"].contains(&p))
    {
        return fail(
            "'phase' must be planning, execution, quality, or utility".into(),
            warnings,
            lines,
            bytes,
        );
    }
    if let Some(p) = value(map, "flow-next")
        && !p.is_null()
        && !p.as_str().is_some_and(is_valid_skill_name)
    {
        return fail(
            "'flow-next' must be null or a kebab-case skill name".into(),
            warnings,
            lines,
            bytes,
        );
    }
    if let Some(p) = value(map, "flow-alternatives")
        && !p
            .as_sequence()
            .is_some_and(|list| list.iter().all(|p| p.as_str().is_some_and(is_valid_skill_name)))
    {
        return fail(
            "'flow-alternatives' must be a list of kebab-case skill names".into(),
            warnings,
            lines,
            bytes,
        );
    }

    let n = value(map, "name").unwrap();
    let Some(name_raw) = n.as_str() else {
        return fail(
            format!("Name must be a string, got {}", kind(n)),
            warnings,
            lines,
            bytes,
        );
    };
    let name = name_raw.trim().to_owned();
    if name.is_empty() {
        return fail("Name cannot be empty".into(), warnings, lines, bytes);
    }
    if name.chars().count() > 64 {
        return fail(
            format!(
                "Name is too long ({} characters). Maximum is 64 characters.",
                name.chars().count()
            ),
            warnings,
            lines,
            bytes,
        );
    }
    if !is_valid_skill_name(&name) {
        return fail(
            format!("Name '{name}' should be kebab-case"),
            warnings,
            lines,
            bytes,
        );
    }
    if !dir_name.is_empty() && name != dir_name {
        warnings.push(format!(
            "Skill name '{name}' differs from directory name '{dir_name}'"
        ));
    }

    let d = value(map, "description").unwrap();
    let Some(desc_raw) = d.as_str() else {
        return fail(
            format!("Description must be a string, got {}", kind(d)),
            warnings,
            lines,
            bytes,
        );
    };
    let desc = desc_raw.trim();
    if desc.is_empty() {
        return fail("Description cannot be empty".into(), warnings, lines, bytes);
    }
    if desc.contains(['<', '>']) {
        return fail(
            "Description cannot contain angle brackets (< or >)".into(),
            warnings,
            lines,
            bytes,
        );
    }
    if desc.chars().count() > 1024 {
        return fail(
            format!(
                "Description is too long ({} characters). Maximum is 1024 characters.",
                desc.chars().count()
            ),
            warnings,
            lines,
            bytes,
        );
    }

    if let Some(c) = value(map, "compatibility").filter(|v| truthy(v)) {
        let Some(c) = c.as_str() else {
            return fail(
                format!("Compatibility must be a string, got {}", kind(c)),
                warnings,
                lines,
                bytes,
            );
        };
        if c.chars().count() > 500 {
            return fail(
                format!(
                    "Compatibility is too long ({} characters). Maximum is 500 characters.",
                    c.chars().count()
                ),
                warnings,
                lines,
                bytes,
            );
        }
    }

    if lines > 500 {
        warnings.push(format!(
            "SKILL.md has {lines} lines (recommended <= 500; move heavy detail to references/)"
        ));
    }

    // Check local relative links under scripts/, references/, assets/ inside skill dir.
    let link_re = Regex::new(r"\[[^\]]+\]\(([^)#\s]+)(?:#[^)]*)?\)")?;
    for cap in link_re.captures_iter(body) {
        let target = &cap[1];
        if target.contains("://") || target.starts_with("mailto:") {
            continue;
        }
        if target.starts_with("scripts/")
            || target.starts_with("references/")
            || target.starts_with("assets/")
        {
            let resolved = path.join(target);
            if !resolved.exists() {
                warnings.push(format!("Broken relative skill resource link: {target}"));
            }
        }
    }

    Ok(SkillValidation {
        path: path.to_path_buf(),
        name,
        valid: true,
        message: "Skill is valid!".into(),
        warnings,
        lines,
        bytes,
    })
}

pub fn validate_skill(path: &Path) -> Result<(bool, String)> {
    let report = validate_skill_detailed(path)?;
    Ok((report.valid, report.message))
}

/// Discover skill directories under an arbitrary skill directory, `skills/` pack, or plugin root.
pub fn discover_skill_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    if root.join("SKILL.md").is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut found = Vec::new();
    for candidate in [
        root.join(".agents/skills"),
        root.join(".claude/skills"),
        root.join("skills"),
        root.to_path_buf(),
    ] {
        if !candidate.is_dir() {
            continue;
        }
        for entry in common::sorted(&candidate)? {
            if entry.is_dir() && entry.join("SKILL.md").is_file() && !found.contains(&entry) {
                found.push(entry);
            }
        }
        if !found.is_empty() && candidate != root {
            break;
        }
    }
    Ok(found)
}

pub fn validate_target(root: &Path) -> Result<Value> {
    let dirs = discover_skill_dirs(root)?;
    if dirs.is_empty() {
        let report = validate_skill_detailed(root)?;
        return Ok(json!({
            "valid": report.valid,
            "count": 1,
            "passed": usize::from(report.valid),
            "failed": usize::from(!report.valid),
            "skills": [report.to_json()],
        }));
    }
    let mut reports = Vec::with_capacity(dirs.len());
    let mut passed = 0usize;
    let mut failed = 0usize;
    for dir in dirs {
        let rep = validate_skill_detailed(&dir)?;
        if rep.valid {
            passed += 1;
        } else {
            failed += 1;
        }
        reports.push(rep.to_json());
    }
    Ok(json!({
        "valid": failed == 0,
        "count": reports.len(),
        "passed": passed,
        "failed": failed,
        "skills": reports,
    }))
}
