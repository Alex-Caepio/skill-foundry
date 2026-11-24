//! Generalized token and byte budget linter for skill packs and plugins.
use crate::{
    Result, RuntimeError,
    common::{self, FileStatStamp, RACY_FS_NS},
    validation,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const TREE_RECEIPT_SCHEMA_VERSION: u32 = 1;
pub const TREE_RACY_NS: i128 = RACY_FS_NS;
const MAX_TREE_RECEIPT_ENTRIES: usize = 256;
const MAX_FILE_DIGEST_ENTRIES: usize = 8192;
const MAX_RECEIPT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const BUDGET_RULES_TAG: &str = "skill-foundry-budget-v1";

pub const RATIOS: [(&str, f64); 6] = [
    ("agents_md_bytes", 4.76),
    ("descriptor_bytes", 4.91),
    ("command_bytes", 5.10),
    ("agent_bytes", 5.14),
    ("frontmatter_bytes", 4.59),
    ("body_bytes", 4.23),
];

#[cfg(test)]
fn budget_live_runs() -> &'static std::sync::atomic::AtomicUsize {
    static COUNT: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();
    COUNT.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedFileDigest {
    len: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    mode: u32,
    sha256: String,
    last_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TreeReceiptEntry {
    scope: String,
    canonical_tree_sha256: String,
    rules_sha256: String,
    target_tree_sha256: String,
    cached_at: String,
    last_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TreeReceiptStore {
    schema_version: u32,
    next_seq: u64,
    receipts: BTreeMap<String, TreeReceiptEntry>,
    file_digests: BTreeMap<String, CachedFileDigest>,
}

impl Default for TreeReceiptStore {
    fn default() -> Self {
        Self {
            schema_version: TREE_RECEIPT_SCHEMA_VERSION,
            next_seq: 1,
            receipts: BTreeMap::new(),
            file_digests: BTreeMap::new(),
        }
    }
}

impl TreeReceiptStore {
    fn receipt_key(
        scope: &str,
        canonical_tree_sha256: &str,
        rules_sha256: &str,
        target_tree_sha256: &str,
    ) -> String {
        common::sha256(
            format!(
                "{TREE_RECEIPT_SCHEMA_VERSION}\0{scope}\0{canonical_tree_sha256}\0{rules_sha256}\0{target_tree_sha256}"
            )
            .as_bytes(),
        )
    }

    fn matches_report(
        &mut self,
        scope: &str,
        canonical_tree_sha256: &str,
        rules_sha256: &str,
        target_tree_sha256: &str,
    ) -> Option<Value> {
        let key = Self::receipt_key(
            scope,
            canonical_tree_sha256,
            rules_sha256,
            target_tree_sha256,
        );
        let report = self.receipts.get(&key).and_then(|entry| {
            (entry.scope == scope
                && entry.canonical_tree_sha256 == canonical_tree_sha256
                && entry.rules_sha256 == rules_sha256
                && entry.target_tree_sha256 == target_tree_sha256)
                .then(|| entry.report.clone())
                .flatten()
        });
        if report.is_some() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);
            if let Some(entry) = self.receipts.get_mut(&key) {
                entry.last_used = seq;
            }
        }
        report
    }

    fn insert_report(
        &mut self,
        scope: &str,
        canonical_tree_sha256: String,
        rules_sha256: String,
        target_tree_sha256: String,
        report: Option<Value>,
    ) {
        let key = Self::receipt_key(
            scope,
            &canonical_tree_sha256,
            &rules_sha256,
            &target_tree_sha256,
        );
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.receipts.insert(
            key,
            TreeReceiptEntry {
                scope: scope.to_string(),
                canonical_tree_sha256,
                rules_sha256,
                target_tree_sha256,
                cached_at: chrono::Utc::now().to_rfc3339(),
                last_used: seq,
                report,
            },
        );
        self.evict_excess();
    }

    fn evict_excess(&mut self) {
        if self.receipts.len() > MAX_TREE_RECEIPT_ENTRIES {
            let mut by_age: Vec<(u64, String)> = self
                .receipts
                .iter()
                .map(|(k, v)| (v.last_used, k.clone()))
                .collect();
            by_age.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let excess = self.receipts.len().saturating_sub(MAX_TREE_RECEIPT_ENTRIES);
            for (_, key) in by_age.into_iter().take(excess) {
                self.receipts.remove(&key);
            }
        }
        if self.file_digests.len() > MAX_FILE_DIGEST_ENTRIES {
            let mut by_age: Vec<(u64, String)> = self
                .file_digests
                .iter()
                .map(|(k, v)| (v.last_used, k.clone()))
                .collect();
            by_age.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let excess = self
                .file_digests
                .len()
                .saturating_sub(MAX_FILE_DIGEST_ENTRIES);
            for (_, key) in by_age.into_iter().take(excess) {
                self.file_digests.remove(&key);
            }
        }
    }
}

fn default_receipt_path(root: &Path) -> PathBuf {
    if let Some(explicit) = std::env::var_os("SKILL_FOUNDRY_BUDGET_RECEIPT")
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    let canonical = common::safe_canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("skill-foundry-cache").join(format!(
        "budget-receipt-{}.json",
        common::sha256(canonical.to_string_lossy().as_bytes())
    ))
}

fn load_store(path: &Path) -> TreeReceiptStore {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return TreeReceiptStore::default();
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_RECEIPT_FILE_BYTES {
        return TreeReceiptStore::default();
    }
    let Ok(bytes) = fs::read(path) else {
        return TreeReceiptStore::default();
    };
    let Ok(mut store) = serde_json::from_slice::<TreeReceiptStore>(&bytes) else {
        return TreeReceiptStore::default();
    };
    if store.schema_version != TREE_RECEIPT_SCHEMA_VERSION {
        return TreeReceiptStore::default();
    }
    store.evict_excess();
    store
}

fn save_store(path: &Path, store: &TreeReceiptStore) {
    static SAVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = SAVE_LOCK.get_or_init(|| Mutex::new(())).lock();
    let mut merged = load_store(path);
    merged.next_seq = merged.next_seq.max(store.next_seq);
    for (key, entry) in &store.receipts {
        merged
            .receipts
            .entry(key.clone())
            .and_modify(|existing| {
                if entry.last_used > existing.last_used {
                    *existing = entry.clone();
                }
            })
            .or_insert_with(|| entry.clone());
    }
    for (key, entry) in &store.file_digests {
        merged
            .file_digests
            .entry(key.clone())
            .and_modify(|existing| {
                if entry.last_used > existing.last_used {
                    *existing = entry.clone();
                }
            })
            .or_insert_with(|| entry.clone());
    }
    merged.evict_excess();
    if let Ok(bytes) = serde_json::to_vec(&merged) {
        let _ = common::atomic_write(path, &bytes);
    }
}

fn hash_optional_regular_file(
    path: &Path,
    store: &mut TreeReceiptStore,
    now_ns: i128,
    dirty: &mut bool,
) -> Option<(String, bool)> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some(("absent".into(), true));
        }
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let stamp = FileStatStamp::from_metadata(&meta);
    let settled = now_ns > TREE_RACY_NS && stamp.is_settled(now_ns);
    let key = path.to_string_lossy().into_owned();
    if settled
        && let Some(cached) = store.file_digests.get(&key)
        && cached.len == stamp.len
        && cached.mtime_ns == stamp.mtime_ns
        && cached.ctime_ns == stamp.ctime_ns
        && cached.mode == stamp.mode
    {
        let sha = cached.sha256.clone();
        let seq = store.next_seq;
        store.next_seq = store.next_seq.saturating_add(1);
        if let Some(entry) = store.file_digests.get_mut(&key) {
            entry.last_used = seq;
        }
        return Some((sha, true));
    }
    let bytes = fs::read(path).ok()?;
    let sha = common::sha256(&bytes);
    if settled {
        let seq = store.next_seq;
        store.next_seq = store.next_seq.saturating_add(1);
        store.file_digests.insert(
            key,
            CachedFileDigest {
                len: stamp.len,
                mtime_ns: stamp.mtime_ns,
                ctime_ns: stamp.ctime_ns,
                mode: stamp.mode,
                sha256: sha.clone(),
                last_used: seq,
            },
        );
        store.evict_excess();
        *dirty = true;
    }
    Some((sha, settled))
}

fn compute_dir_tree_sha(
    root: &Path,
    store: &mut TreeReceiptStore,
    now_ns: i128,
    dirty: &mut bool,
) -> Option<(String, bool)> {
    let mut parts = Vec::new();
    let mut all_settled = true;

    for marker in ["AGENTS.md", "CLAUDE.md", ".claude-plugin/plugin.json"] {
        let (sha, settled) = hash_optional_regular_file(&root.join(marker), store, now_ns, dirty)?;
        all_settled &= settled;
        parts.push(format!("{marker}:{sha}"));
    }

    let skill_dirs = validation::discover_skill_dirs(root).ok()?;
    let mut skill_entries = Vec::new();
    for skill_dir in skill_dirs {
        let md_path = skill_dir.join("SKILL.md");
        let rel = skill_dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| skill_dir.to_string_lossy().into_owned());
        let (sha, settled) = hash_optional_regular_file(&md_path, store, now_ns, dirty)?;
        all_settled &= settled;
        skill_entries.push(format!("skill:{rel}/SKILL.md:{sha}"));
    }
    skill_entries.sort();
    parts.extend(skill_entries);

    for (label, dir, pattern) in [
        ("commands", root.join("commands"), "*.md"),
        (".claude/commands", root.join(".claude/commands"), "*.md"),
        ("agents", root.join("agents"), "*.md"),
        (".claude/agents", root.join(".claude/agents"), "*.md"),
    ] {
        if !dir.is_dir() {
            parts.push(format!("{label}:absent"));
            continue;
        }
        let mut dir_parts = Vec::new();
        for relative in common::walk(&dir).ok()? {
            if relative.contains('/') || !common::glob_match(pattern, &relative) {
                continue;
            }
            let file = common::under(&dir, &relative).ok()?;
            let (sha, settled) = hash_optional_regular_file(&file, store, now_ns, dirty)?;
            all_settled &= settled;
            dir_parts.push(format!("{label}:{relative}:{sha}"));
        }
        dir_parts.sort();
        parts.extend(dir_parts);
    }

    Some((common::sha256(parts.join("\n").as_bytes()), all_settled))
}

fn ceilings_sha(ceilings: Option<&Value>) -> String {
    match ceilings {
        Some(v) => {
            let serialized = serde_json::to_string(v).unwrap_or_default();
            common::sha256(serialized.as_bytes())
        }
        None => "none".into(),
    }
}

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
    measure_dir_with_options(
        root,
        ceilings,
        true,
        None,
        common::now_unix_ns(),
    )
    .map(|(report, _)| report)
}

pub fn measure_dir_with_options(
    root: &Path,
    ceilings: Option<&Value>,
    use_cache: bool,
    cache_path: Option<&Path>,
    now_ns: i128,
) -> Result<(Value, bool)> {
    let root = common::absolute(root)?;
    if !root.exists() {
        return Err(RuntimeError::not_found(format!(
            "Target directory not found: {}",
            root.display()
        )));
    }
    let effective_cache = use_cache && !common::is_cache_bypassed();
    let receipt_path = effective_cache.then(|| {
        cache_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_receipt_path(&root))
    });
    let mut store = receipt_path
        .as_deref()
        .map(load_store)
        .unwrap_or_default();
    let mut dirty = false;

    let canonical_root = common::safe_canonicalize(&root).unwrap_or_else(|_| root.clone());
    let scope = format!("budget-measure:{}", canonical_root.display());
    let rules = ceilings_sha(ceilings);
    let tree_info = if effective_cache {
        compute_dir_tree_sha(&root, &mut store, now_ns, &mut dirty)
    } else {
        None
    };

    if effective_cache
        && let Some((ref tree_sha, all_settled)) = tree_info
        && all_settled
        && let Some(mut cached) = store.matches_report(&scope, tree_sha, &rules, BUDGET_RULES_TAG)
    {
        if let Some(obj) = cached.as_object_mut() {
            obj.insert("root".into(), json!(root));
        }
        if dirty && let Some(path) = receipt_path.as_deref() {
            save_store(path, &store);
        }
        return Ok((cached, true));
    }

    let report = measure_dir_uncached(&root, ceilings)?;

    if effective_cache
        && let Some((tree_sha, all_settled)) = tree_info
        && all_settled
    {
        store.insert_report(
            &scope,
            tree_sha,
            rules,
            BUDGET_RULES_TAG.into(),
            Some(report.clone()),
        );
        dirty = true;
    }
    if effective_cache
        && dirty
        && let Some(path) = receipt_path.as_deref()
    {
        save_store(path, &store);
    }
    Ok((report, false))
}

fn measure_dir_uncached(root: &Path, ceilings: Option<&Value>) -> Result<Value> {
    #[cfg(test)]
    budget_live_runs().fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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

    for skill_dir in validation::discover_skill_dirs(root)? {
        let md_path = skill_dir.join("SKILL.md");
        let text = common::read_regular(&md_path)?;
        let rel = skill_dir
            .strip_prefix(root)
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
    let use_cache = !(common::flag(v, "strict") || common::flag(v, "no_cache"));
    let (report, _) = measure_dir_with_options(
        target,
        ceiling_value.as_ref(),
        use_cache,
        None,
        common::now_unix_ns(),
    )?;
    common::json_cli(report, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_tree_receipt_hits_settled_and_invalidates_on_edit_or_strict() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("pack");
        std::fs::create_dir_all(pack.join("skills/alpha")).unwrap();
        let skill_md = pack.join("skills/alpha/SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: alpha\ndescription: First description\n---\nBody one.\n",
        )
        .unwrap();
        let receipt = tmp.path().join("budget-receipts.json");
        let meta = std::fs::symlink_metadata(&skill_md).unwrap();
        let stamp = FileStatStamp::from_metadata(&meta);
        let racy_now = stamp.mtime_ns + TREE_RACY_NS - 1;
        let settled_now = stamp.mtime_ns + TREE_RACY_NS + 10_000;

        let before = budget_live_runs().load(std::sync::atomic::Ordering::Relaxed);
        // Unsettled within TREE_RACY_NS does not reuse receipt.
        let (_, hit_racy) =
            measure_dir_with_options(&pack, None, true, Some(&receipt), racy_now).unwrap();
        assert!(!hit_racy);
        assert_eq!(
            budget_live_runs().load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );

        // Settled warms then hits.
        let (r1, hit1) =
            measure_dir_with_options(&pack, None, true, Some(&receipt), settled_now).unwrap();
        assert!(!hit1);
        let (r2, hit2) =
            measure_dir_with_options(&pack, None, true, Some(&receipt), settled_now + 5).unwrap();
        assert!(hit2);
        assert_eq!(r1["measured"], r2["measured"]);
        assert_eq!(
            budget_live_runs().load(std::sync::atomic::Ordering::Relaxed),
            before + 2
        );

        // Strict/no-cache forces live recompute.
        let (_, hit_strict) =
            measure_dir_with_options(&pack, None, false, Some(&receipt), settled_now + 10)
                .unwrap();
        assert!(!hit_strict);
        assert_eq!(
            budget_live_runs().load(std::sync::atomic::Ordering::Relaxed),
            before + 3
        );

        // Ceiling change misses and recomputes cleanly.
        let (_, hit_ceil) = measure_dir_with_options(
            &pack,
            Some(&json!({"skills": 10})),
            true,
            Some(&receipt),
            settled_now + 20,
        )
        .unwrap();
        assert!(!hit_ceil);

        // Same-length SKILL.md edit misses even if same size.
        std::fs::write(
            &skill_md,
            "---\nname: alpha\ndescription: Secnd description\n---\nBody two.\n",
        )
        .unwrap();
        let stamp2 = FileStatStamp::from_metadata(&std::fs::symlink_metadata(&skill_md).unwrap());
        let (_, hit_edit) = measure_dir_with_options(
            &pack,
            None,
            true,
            Some(&receipt),
            stamp2.mtime_ns + TREE_RACY_NS + 10_000,
        )
        .unwrap();
        assert!(!hit_edit);
    }
}
