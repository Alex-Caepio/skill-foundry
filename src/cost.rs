//! Dual-host BTE token and attribution cost auditor across Claude Code and Codex CLI sessions.
use crate::{Result, RuntimeError, common};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub const TIERS: [(&str, f64); 5] = [
    ("input", 1.0),
    ("cache_write_5m", 1.25),
    ("cache_write_1h", 2.0),
    ("cache_read", 0.1),
    ("output", 5.0),
];

pub const DIMENSIONS: [(&str, &str); 5] = [
    ("skill", "attributionSkill"),
    ("mcp", "attributionMcpServer"),
    ("tool", "attributionMcpTool"),
    ("plugin", "attributionPlugin"),
    ("agent", "attributionAgent"),
];

pub fn tiers_from_claude_usage(usage: &Value) -> BTreeMap<String, u64> {
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    let mut c5 = usage["cache_creation"]["ephemeral_5m_input_tokens"]
        .as_u64()
        .unwrap_or(0);
    let c1h = usage["cache_creation"]["ephemeral_1h_input_tokens"]
        .as_u64()
        .unwrap_or(0);
    if c5 == 0 && c1h == 0 {
        c5 = n("cache_creation_input_tokens");
    }
    BTreeMap::from([
        ("input".into(), n("input_tokens")),
        ("cache_write_5m".into(), c5),
        ("cache_write_1h".into(), c1h),
        ("cache_read".into(), n("cache_read_input_tokens")),
        ("output".into(), n("output_tokens")),
    ])
}

pub fn bte(tiers: &BTreeMap<String, u64>) -> f64 {
    TIERS
        .iter()
        .map(|(k, w)| tiers.get(*k).copied().unwrap_or(0) as f64 * w)
        .sum()
}

fn collect_jsonl(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in common::sorted(root)? {
        let meta = std::fs::symlink_metadata(&entry)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_jsonl(&entry, out)?;
        } else if entry.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(entry);
        }
    }
    Ok(())
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path)?;
    let mut rows = Vec::new();
    for line in BufReader::new(file).split(b'\n') {
        let Ok(line) = line else { continue };
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if let Ok(val) = serde_json::from_slice::<Value>(&line)
            && val.is_object()
        {
            rows.push(val);
        }
    }
    Ok(rows)
}

fn timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[derive(Default)]
struct Accumulator {
    calls: u64,
    sessions: usize,
    total_tiers: BTreeMap<String, u64>,
    strata: BTreeMap<String, f64>,
    stratum_calls: BTreeMap<String, u64>,
    projects: BTreeMap<String, f64>,
    project_calls: BTreeMap<String, u64>,
    dimensions: BTreeMap<String, BTreeMap<String, f64>>,
    dimension_calls: BTreeMap<String, BTreeMap<String, u64>>,
}

impl Accumulator {
    fn new() -> Self {
        let mut acc = Self::default();
        for (name, _) in DIMENSIONS {
            acc.dimensions.insert(name.into(), BTreeMap::new());
            acc.dimension_calls.insert(name.into(), BTreeMap::new());
        }
        acc
    }

    fn record(
        &mut self,
        project: &str,
        stratum: &str,
        t: &BTreeMap<String, u64>,
        dims: &[(String, String)],
    ) {
        let cost = bte(t);
        self.calls += 1;
        *self.projects.entry(project.to_owned()).or_default() += cost;
        *self.project_calls.entry(project.to_owned()).or_default() += 1;
        *self.strata.entry(stratum.to_owned()).or_default() += cost;
        *self.stratum_calls.entry(stratum.to_owned()).or_default() += 1;
        for (k, n) in t {
            *self.total_tiers.entry(k.clone()).or_default() += n;
        }
        for (dim, label) in dims {
            if let Some(m) = self.dimensions.get_mut(dim) {
                *m.entry(label.clone()).or_default() += cost;
            }
            if let Some(c) = self.dimension_calls.get_mut(dim) {
                *c.entry(label.clone()).or_default() += 1;
            }
        }
    }

    fn finish(self) -> Value {
        json!({
            "calls": self.calls,
            "sessions": self.sessions,
            "weights": TIERS.into_iter().collect::<BTreeMap<_, _>>(),
            "tiers": self.total_tiers,
            "total_bte": common::rounded(bte(&self.total_tiers)),
            "strata": self.strata.into_iter().map(|(k, v)| (k, json!(common::rounded(v)))).collect::<BTreeMap<_, _>>(),
            "stratum_calls": self.stratum_calls,
            "projects": self.projects.into_iter().map(|(k, v)| (k, json!(common::rounded(v)))).collect::<BTreeMap<_, _>>(),
            "project_calls": self.project_calls,
            "attribution": self.dimensions.into_iter().map(|(d, m)| {
                (d, json!(m.into_iter().map(|(k, v)| (k, json!(common::rounded(v)))).collect::<BTreeMap<_, _>>()))
            }).collect::<BTreeMap<_, _>>(),
            "attribution_calls": self.dimension_calls,
        })
    }
}

fn audit_claude(
    root: &Path,
    project_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
    acc: &mut Accumulator,
) -> Result<()> {
    let mut files = Vec::new();
    collect_jsonl(root, &mut files)?;
    for path in files {
        let project = path
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        if project_filter.is_some_and(|s| !project.contains(s)) {
            continue;
        }
        let Ok(rows) = read_jsonl(&path) else {
            continue;
        };
        let mut seen = BTreeMap::<String, usize>::new();
        let mut unique = Vec::new();
        let mut retained = Vec::new();
        for r in rows {
            if !r["message"]["usage"].is_object() {
                continue;
            }
            if let Some(cutoff) = since
                && r["timestamp"].as_str().and_then(timestamp).is_some_and(|ts| ts < cutoff)
            {
                continue;
            }
            if !r["message"]["id"].is_null() {
                let id = r["message"]["id"].to_string();
                if let Some(&idx) = seen.get(&id) {
                    unique[idx] = r;
                } else {
                    seen.insert(id, unique.len());
                    unique.push(r);
                }
            } else {
                retained.push(r);
            }
        }
        retained.extend(unique);
        if retained.is_empty() {
            continue;
        }
        acc.sessions += 1;
        for r in retained {
            let t = tiers_from_claude_usage(&r["message"]["usage"]);
            let stratum = if r["isSidechain"] == true { "subagent" } else { "main" };
            let mut dims = Vec::new();
            for (name, field) in DIMENSIONS {
                if let Some(val) = r.get(field).filter(|v| !v.is_null() && **v != false && **v != "") {
                    dims.push((name.to_string(), common::display(val)));
                }
            }
            // Fallback tool_use / Skill attribution from assistant content array
            if let Some(arr) = r["message"]["content"].as_array() {
                let mut seen_tools = BTreeSet::new();
                for block in arr {
                    if block["type"] == "tool_use"
                        && let Some(tname) = block["name"].as_str()
                        && seen_tools.insert(tname.to_string())
                    {
                        if tname == "Skill"
                            && let Some(sk) = block["input"]["skill"].as_str()
                        {
                            dims.push(("skill".into(), sk.to_owned()));
                        } else if tname.starts_with("mcp__") {
                            let mut parts = tname.split("__");
                            let _ = parts.next();
                            if let Some(srv) = parts.next() {
                                dims.push(("mcp".into(), srv.to_owned()));
                            }
                            dims.push(("tool".into(), tname.to_owned()));
                        } else {
                            dims.push(("tool".into(), tname.to_owned()));
                        }
                    }
                }
            }
            acc.record(&project, stratum, &t, &dims);
        }
    }
    Ok(())
}

fn audit_codex(
    root: &Path,
    project_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
    acc: &mut Accumulator,
) -> Result<()> {
    let mut files = Vec::new();
    collect_jsonl(root, &mut files)?;
    for path in files {
        let Ok(rows) = read_jsonl(&path) else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        let mut cwd = String::from("unknown");
        let mut is_subagent = false;
        let mut max_in = 0u64;
        let mut max_cached = 0u64;
        let mut max_write = 0u64;
        let mut max_out = 0u64;
        let mut session_ts: Option<DateTime<Utc>> = None;
        let mut tools_seen = BTreeSet::new();

        for r in &rows {
            if session_ts.is_none() {
                session_ts = r["timestamp"].as_str().and_then(timestamp);
            }
            let rtype = r["type"].as_str().unwrap_or("");
            let p = &r["payload"];
            if rtype == "session_meta" {
                if let Some(c) = p["cwd"].as_str() {
                    cwd = c.to_owned();
                }
                if p["source"].as_str() == Some("subagent") || p["thread_source"].as_str() == Some("subagent") {
                    is_subagent = true;
                }
            } else if rtype == "event_msg" && p["type"].as_str() == Some("token_count") {
                let u = &p["info"]["total_token_usage"];
                max_in = max_in.max(u["input_tokens"].as_u64().unwrap_or(0));
                max_cached = max_cached.max(u["cached_input_tokens"].as_u64().unwrap_or(0));
                max_write = max_write.max(u["cache_write_input_tokens"].as_u64().unwrap_or(0));
                max_out = max_out.max(u["output_tokens"].as_u64().unwrap_or(0));
            } else if rtype == "response_item"
                && matches!(p["type"].as_str(), Some("function_call" | "custom_tool_call"))
                && let Some(name) = p["name"].as_str()
            {
                tools_seen.insert(name.to_owned());
            }
        }
        if let Some(cutoff) = since
            && session_ts.is_some_and(|ts| ts < cutoff)
        {
            continue;
        }
        if project_filter.is_some_and(|s| !cwd.contains(s)) {
            continue;
        }
        if max_in == 0 && max_out == 0 {
            continue;
        }
        acc.sessions += 1;
        // Codex input_tokens includes cached_input_tokens; subtract cached to avoid double-weighting.
        let uncached_in = max_in.saturating_sub(max_cached);
        let tiers = BTreeMap::from([
            ("input".into(), uncached_in),
            ("cache_write_5m".into(), max_write),
            ("cache_write_1h".into(), 0),
            ("cache_read".into(), max_cached),
            ("output".into(), max_out),
        ]);
        let stratum = if is_subagent { "subagent" } else { "main" };
        let dims: Vec<(String, String)> = tools_seen
            .into_iter()
            .map(|t| {
                if t.starts_with("mcp__") {
                    ("mcp".into(), t)
                } else {
                    ("tool".into(), t)
                }
            })
            .collect();
        acc.record(&cwd, stratum, &tiers, &dims);
    }
    Ok(())
}

pub fn default_claude_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude/projects")
}

pub fn default_codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex")
        })
        .join("sessions")
}

pub fn execute(v: &Value) -> Result<Value> {
    let host = common::text(v, "host").unwrap_or("all");
    if !["all", "claude", "codex"].contains(&host) {
        return Err(RuntimeError::invalid("host must be all, claude, or codex"));
    }
    let claude_dir = common::text(v, "claude_dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_claude_dir);
    let codex_dir = common::text(v, "codex_dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_dir);
    let project_filter = common::text(v, "project");
    let since = v
        .get("since_days")
        .and_then(|d| d.as_u64().or_else(|| d.as_str().and_then(|s| s.parse().ok())))
        .map(|days| Utc::now() - ChronoDuration::days(days as i64));

    let mut claude_acc = Accumulator::new();
    let mut codex_acc = Accumulator::new();
    if host == "all" || host == "claude" {
        audit_claude(&claude_dir, project_filter, since, &mut claude_acc)?;
    }
    if host == "all" || host == "codex" {
        audit_codex(&codex_dir, project_filter, since, &mut codex_acc)?;
    }

    let claude_summary = claude_acc.finish();
    let codex_summary = codex_acc.finish();
    let total_bte = common::rounded(
        claude_summary["total_bte"].as_f64().unwrap_or(0.0)
            + codex_summary["total_bte"].as_f64().unwrap_or(0.0),
    );
    let total_calls =
        claude_summary["calls"].as_u64().unwrap_or(0) + codex_summary["calls"].as_u64().unwrap_or(0);
    let total_sessions = claude_summary["sessions"].as_u64().unwrap_or(0)
        + codex_summary["sessions"].as_u64().unwrap_or(0);

    common::json_cli(
        json!({
            "host": host,
            "total_calls": total_calls,
            "total_sessions": total_sessions,
            "total_bte": total_bte,
            "claude": claude_summary,
            "codex": codex_summary,
        }),
        String::new(),
    )
}
