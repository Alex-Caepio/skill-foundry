use crate::{Result, RuntimeError};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::{Builder, NamedTempFile, TempDir};

pub struct Scratch(pub PathBuf, #[allow(dead_code)] TempDir);

impl Scratch {
    pub fn new(base: &Path, prefix: &str) -> Result<Self> {
        let dir = Builder::new().prefix(&format!("{prefix}-")).tempdir_in(base)?;
        let path = dir.path().to_path_buf();
        Ok(Self(path, dir))
    }
}

pub fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    text(v, key)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RuntimeError::invalid(format!("Missing {key}")))
}

pub fn flag(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(|x| x == true || x == "true")
}

pub fn number(v: &Value, key: &str, default: f64) -> Result<f64> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(x) => x
            .as_f64()
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            .filter(|n| n.is_finite())
            .ok_or_else(|| RuntimeError::invalid(format!("{key} must be finite"))),
    }
}

pub fn positive(v: &Value, key: &str, default: usize) -> Result<usize> {
    let n = number(v, key, default as f64)?;
    if n < 1.0 || n.fract() != 0.0 || n > usize::MAX as f64 {
        return Err(RuntimeError::invalid(format!(
            "{key} must be a positive integer"
        )));
    }
    Ok(n as usize)
}

pub fn positional(v: &Value, key: &str, index: usize) -> Result<String> {
    text(v, key)
        .or_else(|| v["_positionals"].get(index).and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| RuntimeError::invalid(format!("Missing {key}")))
}

pub fn absolute(path: impl AsRef<Path>) -> Result<PathBuf> {
    let p = path.as_ref();
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}

pub fn safe_canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(RuntimeError::from)
}

/// Detect whether `dir` is a packaged plugin root (Claude/Codex manifest + plugin binary)
/// so MCP/root discovery never walks up out of a plugin cache directory into `$HOME` or a marketplace root.
pub fn is_plugin_dir(dir: &Path) -> bool {
    (dir.join(".codex-plugin/plugin.json").is_file()
        || dir.join(".claude-plugin/plugin.json").is_file())
        && dir.join("bin/skill-foundry").exists()
}

pub fn inside_plugin_dir(dir: &Path) -> bool {
    dir.ancestors().any(is_plugin_dir)
}

/// Ensure a relative path resolves strictly inside `root` without escaping via `..` or symlinks.
pub fn under(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err(RuntimeError::policy("Relative path escapes root"));
    }
    let candidate = root.join(rel);
    let canon_root = safe_canonicalize(root)?;
    let canon_target = safe_canonicalize(&candidate)?;
    if !canon_target.starts_with(&canon_root) {
        return Err(RuntimeError::policy("Resolved path escapes root"));
    }
    Ok(canon_target)
}

pub fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn read(path: &Path) -> Result<String> {
    let raw = fs::read(path)?;
    let text = std::str::from_utf8(&raw)
        .map_err(|_| RuntimeError::invalid(format!("{} is not valid UTF-8", path.display())))?;
    Ok(normalize(text))
}

pub fn read_regular(path: &Path) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(RuntimeError::policy(format!(
            "Refusing non-regular file {}",
            path.display()
        )));
    }
    read(path)
}

pub fn walk(base: &Path) -> Result<Vec<String>> {
    fn visit(root: &Path, cur: &Path, out: &mut Vec<String>) -> Result<()> {
        for p in sorted(cur)? {
            let meta = fs::symlink_metadata(&p)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                visit(root, &p, out)?;
            } else if meta.is_file()
                && let Ok(rel) = p.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    if !base.is_dir() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    visit(base, base, &mut out)?;
    out.sort();
    Ok(out)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| RuntimeError::from(e.error))?;
    Ok(())
}

pub fn json_file(path: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&read(path)?)?)
}

pub fn pretty(v: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)?)
}

pub fn write_json(path: &Path, value: &Value, newline: bool) -> Result<()> {
    let body = pretty(value)? + if newline { "\n" } else { "" };
    atomic_write(path, body.as_bytes())
}

pub fn cli(
    code: i32,
    stdout: impl Into<String>,
    stderr: impl Into<String>,
    result: Value,
) -> Value {
    json!({"_cli": {"code": code, "stdout": stdout.into(), "stderr": stderr.into()}, "result": result})
}

pub fn json_cli(result: Value, stderr: String) -> Result<Value> {
    Ok(cli(0, format!("{}\n", pretty(&result)?), stderr, result))
}

pub fn rounded(n: f64) -> f64 {
    format!("{n:.4}").parse().unwrap_or(n)
}

pub fn env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

pub fn claude_env() -> BTreeMap<String, String> {
    env()
        .into_iter()
        .filter(|(k, _)| k != "CLAUDECODE")
        .collect()
}

pub fn sorted(path: &Path) -> Result<Vec<PathBuf>> {
    let mut p = fs::read_dir(path)?
        .map(|x| x.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    p.sort();
    Ok(p)
}

pub fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub fn display(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
}

pub fn chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn title(s: &str) -> String {
    s.split(['-', '_', ' '])
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|a| a.to_uppercase().collect::<String>() + &c.as_str().to_lowercase())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn stdin_json() -> Result<Value> {
    const LIMIT: usize = 32 * 1024 * 1024;
    let mut input = Vec::new();
    std::io::stdin()
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut input)?;
    if input.len() > LIMIT {
        return Err(RuntimeError::invalid(
            "Standard input exceeds the 32 MiB JSON limit",
        ));
    }
    Ok(serde_json::from_slice(&input)?)
}

pub fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    fn rec(p: &[char], v: &[char]) -> bool {
        match (p.first(), v.first()) {
            (None, None) => true,
            (Some('*'), _) => rec(&p[1..], v) || (!v.is_empty() && rec(p, &v[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &v[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &v[1..]),
            _ => false,
        }
    }
    rec(&p, &v)
}

pub fn browser(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    let (cmd, args) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(target_os = "macos")]
    let (cmd, args) = ("open", vec![url]);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let (cmd, args) = ("xdg-open", vec![url]);

    let status = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| RuntimeError::unavailable(format!("browser opener failed: {e}")))?;
    if !status.success() {
        return Err(RuntimeError::unavailable(format!(
            "browser opener exited {}",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}
