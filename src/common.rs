use crate::{Result, RuntimeError};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
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

/// Detect whether `dir` is a packaged plugin root (Claude/Codex/Antigravity manifest + plugin binary)
/// so MCP/root discovery never walks up out of a plugin cache directory into `$HOME` or a marketplace root.
pub fn is_plugin_dir(dir: &Path) -> bool {
    (dir.join(".codex-plugin/plugin.json").is_file()
        || dir.join(".claude-plugin/plugin.json").is_file()
        || dir.join(".antigravity-plugin/plugin.json").is_file()
        || dir.join(".gemini-plugin/plugin.json").is_file()
        || dir.join("plugin.json").is_file()
        || dir.join(".mcp.json").is_file()
        || dir.join("mcp_config.json").is_file())
        && dir.join("bin/skill-foundry").exists()
}

pub fn inside_plugin_dir(dir: &Path) -> bool {
    if dir.ancestors().any(is_plugin_dir) {
        return true;
    }
    let norm = dir.to_string_lossy().replace('\\', "/");
    norm.contains("/.gemini/config/plugins/")
        || norm.ends_with("/.gemini/config/plugins")
        || norm.contains("/.claude/plugins/cache/")
        || norm.ends_with("/.claude/plugins/cache")
        || norm.contains("/.codex/plugins/cache/")
        || norm.ends_with("/.codex/plugins/cache")
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

pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub const RACY_FS_NS: i128 = 2_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatStamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub mode: u32,
    pub nlink: u64,
}

impl FileStatStamp {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mtime_ns = (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128);
            let ctime_ns = (meta.ctime() as i128) * 1_000_000_000 + (meta.ctime_nsec() as i128);
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
                len: meta.len(),
                mtime_ns,
                ctime_ns,
                mode: meta.mode(),
                nlink: meta.nlink(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            Self {
                dev: 0,
                ino: 0,
                len: meta.len(),
                mtime_ns,
                ctime_ns: mtime_ns,
                mode: if meta.is_dir() { 0o40755 } else { 0o100644 },
                nlink: 1,
            }
        }
    }

    pub fn is_settled(&self, now_ns: i128) -> bool {
        self.mtime_ns > 0
            && self.ctime_ns > 0
            && now_ns >= self.mtime_ns + RACY_FS_NS
            && now_ns >= self.ctime_ns + RACY_FS_NS
    }

    pub fn same_file_identity(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.len == other.len
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.mode == other.mode
    }
}

pub fn now_unix_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

pub fn is_cache_bypassed() -> bool {
    std::env::var("SKILL_FOUNDRY_NO_CACHE").as_deref() == Ok("1")
        || std::env::var("INFRA_CHECK_NO_CACHE").as_deref() == Ok("1")
        || std::env::var("INFRA_NO_CACHE").as_deref() == Ok("1")
        || std::env::var("INFRA_TOOLING_NO_CACHE").as_deref() == Ok("1")
}

/// Detect common hardcoded secret/credential patterns before passing text to external probes or logs.
pub fn detect_sensitive_label(text: &str) -> Option<&'static str> {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    let list = PATTERNS.get_or_init(|| {
        [
            (
                "private key",
                r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----",
            ),
            (
                "GitHub token",
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b",
            ),
            ("GitLab token", r"\bglpat-[A-Za-z0-9\-_]{20,}\b"),
            ("Slack token", r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b"),
            (
                "AWS access key",
                r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
            ),
            ("Google API key", r"\bAIza[0-9A-Za-z\-_]{35}\b"),
            (
                "OpenAI-style token",
                r"\bsk-(?:ant-|proj-|svcacct-)?[A-Za-z0-9\-_]{20,}\b",
            ),
            ("Stripe secret key", r"\b[rs]k_(?:live|test)_[0-9A-Za-z]{16,}\b"),
            ("npm token", r"\bnpm_[A-Za-z0-9]{20,}\b"),
            ("PyPI token", r"\bpypi-[A-Za-z0-9\-_]{20,}\b"),
            ("Hugging Face token", r"\bhf_[A-Za-z0-9]{20,}\b"),
            ("credential in URL", r"https?://[^/\s:@]+:[^/\s@]+@"),
            ("bearer token", r"(?i)\bBearer\s+[A-Za-z0-9\-_.~+/]{20,}=*"),
        ]
        .into_iter()
        .map(|(label, pat)| (label, Regex::new(pat).expect("valid sensitivity regex")))
        .collect()
    });
    list.iter()
        .find_map(|(label, re)| re.is_match(text).then_some(*label))
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
