//! Skill directory initializer and `.skill` archive packager.
use crate::{Result, RuntimeError, common, validation};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

fn template(text: &str, name: &str) -> String {
    text.replace("{skill_name}", name)
        .replace("{skill_title}", &common::title(name))
}

pub fn init_skill(name: &str, parent: &Path, log: &mut String) -> Result<PathBuf> {
    if !validation::is_valid_skill_name(name) {
        return Err(RuntimeError::invalid(format!(
            "Skill name '{name}' must be kebab-case (lowercase ASCII letters, digits, hyphens, <= 64 chars)"
        )));
    }
    let directory = common::absolute(parent)?.join(name);
    if directory.exists() {
        return Err(RuntimeError::conflict(format!(
            "Skill directory already exists: {}",
            directory.display()
        )));
    }
    std::fs::create_dir_all(&directory)?;
    log.push_str(&format!(
        "Created skill directory: {}\n",
        directory.display()
    ));
    std::fs::write(
        directory.join("SKILL.md"),
        template(include_str!("../resources/skill-template.md"), name),
    )?;
    log.push_str("Created SKILL.md\n");
    for sub in ["scripts", "references", "assets"] {
        std::fs::create_dir_all(directory.join(sub))?;
    }
    let literal = serde_json::to_string(&format!("This is an example script for {name}"))?;
    let script = format!(
        "//! Example helper for {name}. Replace or delete.\nfn main() {{ println!(\"{{}}\", {literal}); }}\n"
    );
    std::fs::write(directory.join("scripts/example.rs"), script)?;
    log.push_str("Created scripts/example.rs\n");
    std::fs::write(
        directory.join("references/api_reference.md"),
        template(include_str!("../resources/reference-template.md"), name),
    )?;
    log.push_str("Created references/api_reference.md\n");
    std::fs::write(
        directory.join("assets/example_asset.txt"),
        include_str!("../resources/asset-template.txt"),
    )?;
    log.push_str("Created assets/example_asset.txt\n");
    Ok(directory)
}

pub fn should_exclude(path: &Path, skill: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(skill) else {
        return true;
    };
    let components: Vec<_> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    components
        .iter()
        .any(|c| c == "__pycache__" || c == "node_modules" || c == ".git")
        || components.first().is_some_and(|c| c == "evals")
        || path
            .file_name()
            .is_some_and(|s| s == ".DS_Store" || s.to_string_lossy().ends_with(".pyc"))
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for path in common::sorted(directory)? {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            if path.is_file() {
                files.push(path);
            }
            continue;
        }
        if meta.is_dir() {
            collect(&path, files)?;
        } else if meta.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

pub fn package_skill(
    skill: &Path,
    output: Option<&Path>,
    log: &mut String,
) -> Result<Option<PathBuf>> {
    let skill = common::absolute(skill)?;
    if !skill.exists() || !skill.is_dir() {
        log.push_str(&format!("Error: Skill folder not found: {}\n", skill.display()));
        return Ok(None);
    }
    if !skill.join("SKILL.md").exists() {
        log.push_str(&format!(
            "Error: SKILL.md not found in {}\n",
            skill.display()
        ));
        return Ok(None);
    }
    let (valid, message) = validation::validate_skill(&skill)?;
    if !valid {
        log.push_str(&format!("Validation failed: {message}\n"));
        return Ok(None);
    }
    let out = common::absolute(output.unwrap_or_else(|| Path::new(".")))?;
    std::fs::create_dir_all(&out)?;
    let name = skill
        .file_name()
        .ok_or_else(|| RuntimeError::invalid("Skill directory needs a name"))?
        .to_string_lossy();
    let destination = out.join(format!("{name}.skill"));
    let file = std::fs::File::create(&destination)?;
    let mut zip = zip::ZipWriter::new(file);
    let mut files = vec![];
    collect(&skill, &mut files)?;
    let canonical = common::safe_canonicalize(&skill)?;
    for path in files {
        if should_exclude(&path, &skill) {
            continue;
        }
        if !common::safe_canonicalize(&path)?.starts_with(&canonical) {
            return Err(RuntimeError::policy("Archive member escapes skill directory"));
        }
        let member = path
            .strip_prefix(skill.parent().unwrap_or(&skill))
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        #[cfg(unix)]
        let options = {
            use std::os::unix::fs::PermissionsExt;
            options.unix_permissions(std::fs::metadata(&path)?.permissions().mode())
        };
        zip.start_file(&member, options)
            .map_err(|e| RuntimeError::invalid(e.to_string()))?;
        zip.write_all(&std::fs::read(&path)?)?;
        log.push_str(&format!("Added: {member}\n"));
    }
    zip.finish()
        .map_err(|e| RuntimeError::invalid(e.to_string()))?;
    log.push_str(&format!("Packaged: {}\n", destination.display()));
    Ok(Some(destination))
}

pub fn execute(operation: &str, v: &Value) -> Result<Value> {
    let mut log = String::new();
    match operation {
        "init-skill" => {
            let name = common::positional(v, "skill_name", 0)?;
            let parent = Path::new(common::required(v, "path")?);
            match init_skill(&name, parent, &mut log) {
                Ok(path) => Ok(common::cli(0, log, "", json!({"path": path}))),
                Err(e) => Ok(common::cli(1, log, format!("Error: {e}\n"), Value::Null)),
            }
        }
        "package-skill" => {
            let skill = common::positional(v, "skill_path", 0)?;
            let output = common::text(v, "output_dir")
                .or_else(|| v["_positionals"].get(1).and_then(Value::as_str));
            let result = package_skill(Path::new(&skill), output.map(Path::new), &mut log)?;
            Ok(common::cli(
                if result.is_some() { 0 } else { 1 },
                log,
                "",
                json!({"path": result}),
            ))
        }
        _ => Err(RuntimeError::invalid("Unknown scaffold operation")),
    }
}
