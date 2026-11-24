//! Standalone interactive eval-output and eval-set HTML review generator/server.
use crate::{
    ErrorKind, Result, RuntimeError, cancel,
    common::{self, FileStatStamp, RACY_FS_NS},
    review_feedback::{self, FeedbackStore, LIMIT as MAX_FEEDBACK_BYTES},
    workbook_preview,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub const REVIEW_RACY_NS: i128 = RACY_FS_NS;
const EMBED_CACHE_CAP: usize = 1024;
const RUN_WALK_CACHE_CAP: usize = 64;

const REVIEW_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'self'";

thread_local! {
    static REVIEW_CACHE_BYPASS_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct ReviewBypassGuard;
impl Drop for ReviewBypassGuard {
    fn drop(&mut self) {
        REVIEW_CACHE_BYPASS_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn with_cache_bypass<T>(bypass: bool, f: impl FnOnce() -> T) -> T {
    if !bypass {
        return f();
    }
    REVIEW_CACHE_BYPASS_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
    let _guard = ReviewBypassGuard;
    f()
}

fn is_cache_bypassed() -> bool {
    REVIEW_CACHE_BYPASS_DEPTH.with(|depth| depth.get() > 0) || common::is_cache_bypassed()
}

#[derive(Default)]
struct EmbedFileCache {
    seq: u64,
    entries: HashMap<PathBuf, (FileStatStamp, Value, u64)>,
}

fn embed_file_cache() -> &'static Mutex<EmbedFileCache> {
    static CACHE: OnceLock<Mutex<EmbedFileCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(EmbedFileCache::default()))
}

type RunWalkStamp = (String, bool, FileStatStamp);

#[derive(Clone)]
struct CachedRunWalk {
    stamps: Vec<RunWalkStamp>,
    runs: Vec<Value>,
    last_used: u64,
}

#[derive(Default)]
struct RunWalkCache {
    seq: u64,
    entries: HashMap<PathBuf, CachedRunWalk>,
}

fn run_walk_cache() -> &'static Mutex<RunWalkCache> {
    static CACHE: OnceLock<Mutex<RunWalkCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(RunWalkCache::default()))
}

#[cfg(test)]
thread_local! {
    static TEST_EMBED_READS: Cell<usize> = const { Cell::new(0) };
    static TEST_FIND_RUNS_WALKS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn bump_embed_reads() {
    TEST_EMBED_READS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn embed_read_count() -> usize {
    TEST_EMBED_READS.with(Cell::get)
}

#[cfg(test)]
fn bump_find_runs_walks() {
    TEST_FIND_RUNS_WALKS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn find_runs_walk_count() -> usize {
    TEST_FIND_RUNS_WALKS.with(Cell::get)
}

fn eval_prompt_re() -> &'static regex::Regex {
    static EVAL_PROMPT_RE: OnceLock<regex::Regex> = OnceLock::new();
    EVAL_PROMPT_RE
        .get_or_init(|| regex::Regex::new(r"(?s)## Eval Prompt\n\n(.*?)(?:\n##|$)").unwrap())
}

const TEXT: [&str; 26] = [
    "txt", "md", "json", "csv", "py", "js", "ts", "tsx", "jsx", "yaml", "yml", "xml", "html",
    "css", "sh", "rb", "go", "rs", "java", "c", "cpp", "h", "hpp", "sql", "r", "toml",
];

pub fn get_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "html" => "text/html",
        "txt" | "md" => "text/plain",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn embed_file_uncached(path: &Path) -> Value {
    #[cfg(test)]
    bump_embed_reads();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mime = get_mime_type(path);
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.is_file())
    {
        return json!({
            "name": name,
            "type": "error",
            "content": "(Refusing symlinked or non-regular output file)"
        });
    }
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(_) => {
            return json!({
                "name": name,
                "type": if TEXT.contains(&ext.as_str()) { "text" } else { "error" },
                "content": "(Error reading file)"
            });
        }
    };
    if TEXT.contains(&ext.as_str()) {
        return json!({
            "name": name,
            "type": "text",
            "content": common::normalize(&String::from_utf8_lossy(&raw))
        });
    }
    let b64 = STANDARD.encode(&raw);
    let data_uri = format!("data:{mime};base64,{b64}");
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => {
            json!({"name": name, "type": "image", "mime": mime, "data_uri": data_uri})
        }
        "pdf" => json!({"name": name, "type": "pdf", "data_uri": data_uri}),
        "xlsx" => match workbook_preview::read_bytes(&raw) {
            Ok(sheets) => {
                json!({"name": name, "type": "xlsx", "data_b64": b64, "worksheets": sheets})
            }
            Err(e) => {
                json!({"name": name, "type": "xlsx", "data_b64": b64, "render_error": e.to_string()})
            }
        },
        _ => json!({"name": name, "type": "binary", "mime": mime, "data_uri": data_uri}),
    }
}

pub fn embed_file_with_now(path: &Path, now_ns: i128, bypass: bool) -> Value {
    if bypass {
        return embed_file_uncached(path);
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return embed_file_uncached(path);
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return embed_file_uncached(path);
    }
    let Ok(canon) = common::safe_canonicalize(path) else {
        return embed_file_uncached(path);
    };
    let stamp = FileStatStamp::from_metadata(&meta);
    if !stamp.is_settled(now_ns) {
        return embed_file_uncached(path);
    }
    if let Ok(mut cache) = embed_file_cache().lock() {
        cache.seq = cache.seq.saturating_add(1);
        let seq = cache.seq;
        if let Some((cached_stamp, value, last_used)) = cache.entries.get_mut(&canon)
            && *cached_stamp == stamp
        {
            *last_used = seq;
            return value.clone();
        }
    }
    let embedded = embed_file_uncached(path);
    if let Ok(mut cache) = embed_file_cache().lock() {
        cache.seq = cache.seq.saturating_add(1);
        let seq = cache.seq;
        if cache.entries.len() >= EMBED_CACHE_CAP
            && !cache.entries.contains_key(&canon)
            && let Some(evict) = cache
                .entries
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(k, _)| k.clone())
        {
            cache.entries.remove(&evict);
        }
        cache.entries.insert(canon, (stamp, embedded.clone(), seq));
    }
    embedded
}

pub fn embed_file(path: &Path) -> Value {
    embed_file_with_now(path, common::now_unix_ns(), is_cache_bypassed())
}

pub fn build_run(root: &Path, directory: &Path) -> Result<Value> {
    build_run_with_now(root, directory, common::now_unix_ns(), is_cache_bypassed())
}

fn build_run_with_now(
    root: &Path,
    directory: &Path,
    now_ns: i128,
    bypass: bool,
) -> Result<Value> {
    let mut prompt = String::new();
    let mut eval_id = Value::Null;
    for file in [
        directory.join("eval_metadata.json"),
        directory.parent().unwrap_or(directory).join("eval_metadata.json"),
    ] {
        if let Ok(meta) = common::json_file(&file) {
            prompt = meta["prompt"].as_str().unwrap_or("").into();
            eval_id = meta.get("eval_id").cloned().unwrap_or(Value::Null);
        }
        if !prompt.is_empty() {
            break;
        }
    }
    if prompt.is_empty() {
        let re = eval_prompt_re();
        for file in [
            directory.join("transcript.md"),
            directory.join("outputs/transcript.md"),
        ] {
            if let Ok(text) = common::read(&file)
                && let Some(c) = re.captures(&text)
            {
                prompt = c[1].trim().into();
                break;
            }
        }
    }
    if prompt.is_empty() {
        prompt = "(No prompt found)".into();
    }
    let id = directory
        .strip_prefix(root)
        .unwrap_or(directory)
        .to_string_lossy()
        .replace(['/', '\\'], "-");
    let outs = directory.join("outputs");
    let mut files = vec![];
    let mut seen_names = BTreeSet::new();
    if outs.is_dir() {
        for path in common::sorted(&outs)? {
            if path.is_file() {
                if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
                    seen_names.insert(name);
                }
                files.push(embed_file_with_now(&path, now_ns, bypass));
            }
        }
    }
    for extra in ["transcript.md", "metrics.json", "user_notes.md"] {
        if !seen_names.contains(extra) {
            let candidate = directory.join(extra);
            if candidate.is_file() {
                files.push(embed_file_with_now(&candidate, now_ns, bypass));
            }
        }
    }
    let mut grading = Value::Null;
    for candidate in [
        directory.join("grading.json"),
        directory.parent().unwrap_or(directory).join("grading.json"),
    ] {
        if let Ok(g) = common::json_file(&candidate) {
            grading = g;
            break;
        }
    }
    Ok(json!({
        "id": id,
        "eval_id": eval_id,
        "prompt": prompt,
        "outputs": files,
        "grading": grading,
    }))
}

fn record_run_file_stat(
    p: &Path,
    label: String,
    stamps: &mut Vec<RunWalkStamp>,
    saw_symlink: &mut bool,
) {
    if let Ok(m) = std::fs::symlink_metadata(p) {
        if m.file_type().is_symlink() {
            *saw_symlink = true;
        } else {
            stamps.push((label, false, FileStatStamp::from_metadata(&m)));
        }
    }
}

fn stat_run_snapshot(
    root: &Path,
    current: &Path,
    stamps: &mut Vec<RunWalkStamp>,
    seen: &mut BTreeSet<PathBuf>,
    saw_symlink: &mut bool,
) -> Result<()> {
    let Ok(root_canon) = common::safe_canonicalize(root) else {
        *saw_symlink = true;
        return Ok(());
    };
    let Ok(dir_meta) = std::fs::symlink_metadata(current) else {
        return Ok(());
    };
    if dir_meta.file_type().is_symlink() {
        *saw_symlink = true;
        return Ok(());
    }
    if !dir_meta.is_dir() {
        return Ok(());
    }
    let Ok(canon) = common::safe_canonicalize(current) else {
        *saw_symlink = true;
        return Ok(());
    };
    if !canon.starts_with(&root_canon) || !seen.insert(canon.clone()) {
        return Ok(());
    }
    let rel = canon
        .strip_prefix(&root_canon)
        .unwrap_or(&canon)
        .to_string_lossy()
        .replace('\\', "/");
    stamps.push((rel.clone(), true, FileStatStamp::from_metadata(&dir_meta)));

    let outputs_dir = current.join("outputs");
    if let Ok(out_meta) = std::fs::symlink_metadata(&outputs_dir) {
        if out_meta.file_type().is_symlink() {
            *saw_symlink = true;
            return Ok(());
        }
        if out_meta.is_dir() {
            stamps.push((
                format!("{rel}/outputs"),
                true,
                FileStatStamp::from_metadata(&out_meta),
            ));
            for f in common::sorted(&outputs_dir)? {
                let fname = f.file_name().unwrap_or_default().to_string_lossy();
                record_run_file_stat(&f, format!("{rel}/outputs/{fname}"), stamps, saw_symlink);
            }
            for extra in [
                "eval_metadata.json",
                "transcript.md",
                "metrics.json",
                "user_notes.md",
                "grading.json",
            ] {
                record_run_file_stat(
                    &current.join(extra),
                    format!("{rel}/{extra}"),
                    stamps,
                    saw_symlink,
                );
            }
            if let Some(parent) = current.parent() {
                for parent_extra in ["eval_metadata.json", "grading.json"] {
                    record_run_file_stat(
                        &parent.join(parent_extra),
                        format!("{rel}/../{parent_extra}"),
                        stamps,
                        saw_symlink,
                    );
                }
            }
            return Ok(());
        }
    }

    for child in common::sorted(current)? {
        let name = child.file_name().unwrap_or_default().to_string_lossy();
        if ["node_modules", ".git", "__pycache__", "skill", "inputs"].contains(&name.as_ref()) {
            continue;
        }
        if let Ok(cm) = std::fs::symlink_metadata(&child) {
            if cm.file_type().is_symlink() {
                *saw_symlink = true;
            } else if cm.is_dir() {
                stat_run_snapshot(root, &child, stamps, seen, saw_symlink)?;
            }
        }
    }
    Ok(())
}

fn recurse(
    root: &Path,
    current: &Path,
    runs: &mut Vec<Value>,
    seen: &mut BTreeSet<PathBuf>,
    now_ns: i128,
    bypass: bool,
) -> Result<()> {
    if !current.is_dir() {
        return Ok(());
    }
    let canonical = common::safe_canonicalize(current)?;
    if !canonical.starts_with(common::safe_canonicalize(root)?) || !seen.insert(canonical) {
        return Ok(());
    }
    if current.join("outputs").is_dir() {
        runs.push(build_run_with_now(root, current, now_ns, bypass)?);
        return Ok(());
    }
    for child in common::sorted(current)? {
        if child.is_dir()
            && !["node_modules", ".git", "__pycache__", "skill", "inputs"].contains(
                &child
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
        {
            recurse(root, &child, runs, seen, now_ns, bypass)?;
        }
    }
    Ok(())
}

pub fn find_runs_with_options(workspace: &Path, now_ns: i128, bypass: bool) -> Result<Vec<Value>> {
    let mut snapshot = Vec::new();
    let mut saw_symlink = false;
    let canon = if !bypass {
        common::safe_canonicalize(workspace).ok()
    } else {
        None
    };
    if let Some(canon_ws) = canon.as_ref() {
        stat_run_snapshot(
            canon_ws,
            canon_ws,
            &mut snapshot,
            &mut BTreeSet::new(),
            &mut saw_symlink,
        )?;
        snapshot.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let settled = !saw_symlink && snapshot.iter().all(|(_, _, s)| s.is_settled(now_ns));
        if settled && let Ok(mut cache) = run_walk_cache().lock() {
            cache.seq = cache.seq.saturating_add(1);
            let seq = cache.seq;
            if let Some(hit) = cache.entries.get_mut(canon_ws)
                && hit.stamps == snapshot
            {
                hit.last_used = seq;
                return Ok(hit.runs.clone());
            }
        }
    }

    #[cfg(test)]
    bump_find_runs_walks();
    let mut runs = vec![];
    recurse(
        workspace,
        workspace,
        &mut runs,
        &mut BTreeSet::new(),
        now_ns,
        bypass,
    )?;
    runs.sort_by(|a, b| {
        let a_id = a["eval_id"].as_f64().unwrap_or(f64::INFINITY);
        let b_id = b["eval_id"].as_f64().unwrap_or(f64::INFINITY);
        a_id.total_cmp(&b_id)
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    if let Some(canon_ws) = canon
        && !bypass
        && !saw_symlink
        && snapshot.iter().all(|(_, _, s)| s.is_settled(now_ns))
        && let Ok(mut cache) = run_walk_cache().lock()
    {
        cache.seq = cache.seq.saturating_add(1);
        let seq = cache.seq;
        if cache.entries.len() >= RUN_WALK_CACHE_CAP
            && !cache.entries.contains_key(&canon_ws)
            && let Some(evict) = cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone())
        {
            cache.entries.remove(&evict);
        }
        cache.entries.insert(
            canon_ws,
            CachedRunWalk {
                stamps: snapshot,
                runs: runs.clone(),
                last_used: seq,
            },
        );
    }
    Ok(runs)
}

pub fn find_runs(workspace: &Path) -> Result<Vec<Value>> {
    find_runs_with_options(workspace, common::now_unix_ns(), is_cache_bypassed())
}

pub fn load_previous_iteration(workspace: &Path) -> Result<Value> {
    load_previous_iteration_with_options(workspace, common::now_unix_ns(), is_cache_bypassed())
}

fn load_previous_iteration_with_options(
    workspace: &Path,
    now_ns: i128,
    bypass: bool,
) -> Result<Value> {
    if !workspace.is_dir() {
        return Ok(json!({}));
    }
    let mut feedback = BTreeMap::new();
    let mut store = FeedbackStore::open(workspace)?;
    let raw = store.read()?;
    if let Ok(value) = serde_json::from_slice::<Value>(&raw)
        && let Some(reviews) = value["reviews"].as_array()
    {
        for r in reviews {
            if let (Some(id), Some(text)) = (r["run_id"].as_str(), r["feedback"].as_str())
                && !text.trim().is_empty()
            {
                feedback.insert(id.to_owned(), text.to_owned());
            }
        }
    }
    let mut previous = serde_json::Map::new();
    for run in find_runs_with_options(workspace, now_ns, bypass)? {
        let id = run["id"].as_str().unwrap_or("").to_owned();
        previous.insert(
            id.clone(),
            json!({
                "feedback": feedback.remove(&id).unwrap_or_default(),
                "outputs": run["outputs"]
            }),
        );
    }
    for (id, text) in feedback {
        previous.insert(id, json!({"feedback": text, "outputs": []}));
    }
    Ok(Value::Object(previous))
}

fn escape_script_json(raw: String) -> String {
    raw.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

pub fn generate_html(
    runs: &[Value],
    name: &str,
    previous: &Value,
    benchmark: Option<&Value>,
    standalone: bool,
) -> Result<String> {
    let mut payload = json!({
        "skill_name": name,
        "runs": runs,
        "previous_feedback": {},
        "previous_outputs": {},
        "standalone": standalone,
    });
    if let Some(map) = previous.as_object() {
        for (id, p) in map {
            if p["feedback"].as_str().is_some_and(|s| !s.is_empty()) {
                payload["previous_feedback"][id] = p["feedback"].clone();
            }
            if p["outputs"].as_array().is_some_and(|a| !a.is_empty()) {
                payload["previous_outputs"][id] = p["outputs"].clone();
            }
        }
    }
    if let Some(b) = benchmark {
        let mut bench = b.clone();
        if !bench["configuration_order"].is_array() {
            bench["configuration_order"] = json!(
                b["run_summary"]
                    .as_object()
                    .map(|m| {
                        m.keys()
                            .filter(|k| {
                                !matches!(k.as_str(), "delta" | "deltas" | "delta_vs_baseline")
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            );
        }
        payload["benchmark"] = bench;
    }
    let safe = escape_script_json(serde_json::to_string(&payload)?);
    Ok(include_str!("../resources/viewer.html").replace("__EVAL_DATA__", &safe))
}

pub fn eval_set_execute(v: &Value) -> Result<Value> {
    let skill = crate::utils::parse_skill_md(Path::new(common::required(v, "skill_path")?))?;
    let items = common::json_file(Path::new(common::required(v, "eval_set")?))?;
    if !items.as_array().is_some_and(|items| {
        items
            .iter()
            .all(|i| i["query"].is_string() && i["should_trigger"].is_boolean())
    }) {
        return Err(RuntimeError::invalid(
            "Eval set must contain query strings and boolean should_trigger values",
        ));
    }
    let data = json!({
        "mode": "eval-set",
        "items": items,
        "skill_name": skill.name,
        "description": skill.description,
        "standalone": true
    });
    let safe = escape_script_json(serde_json::to_string(&data)?);
    let html = include_str!("../resources/eval-set.html")
        .replace("__SKILL_NAME_PLACEHOLDER__", &common::html(&skill.name))
        .replace(
            "__SKILL_DESCRIPTION_PLACEHOLDER__",
            &common::html(&skill.description),
        )
        .replace("__EVAL_DATA__", &safe);
    let output = common::required(v, "output")?;
    common::atomic_write(Path::new(output), html.as_bytes())?;
    Ok(common::cli(
        0,
        format!("Eval set review written to {output}\n"),
        "",
        json!({"output": output}),
    ))
}

pub fn request_path(raw_url: &str) -> &str {
    raw_url.split(['?', '#']).next().unwrap_or(raw_url)
}

fn response(
    status: u16,
    mime: &str,
    body: Vec<u8>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(tiny_http::Header::from_bytes("Content-Type", mime).unwrap())
        .with_header(tiny_http::Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap())
        .with_header(tiny_http::Header::from_bytes("Referrer-Policy", "no-referrer").unwrap())
        .with_header(
            tiny_http::Header::from_bytes("Cross-Origin-Resource-Policy", "same-origin").unwrap(),
        )
        .with_header(tiny_http::Header::from_bytes("X-Frame-Options", "DENY").unwrap())
        .with_header(tiny_http::Header::from_bytes("Content-Security-Policy", REVIEW_CSP).unwrap())
}

fn code_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request",
        403 => "policy_violation",
        404 => "not_found",
        409 => "conflict",
        413 => "payload_too_large",
        415 => "unsupported_media_type",
        503 => "unavailable",
        _ => "internal_error",
    }
}

fn json_error_response(
    status: u16,
    code: Option<&str>,
    message: impl Into<String>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let err_code = code.unwrap_or_else(|| code_for_status(status));
    response(
        status,
        "application/json",
        json!({
            "error": message.into(),
            "code": err_code,
            "status": status
        })
        .to_string()
        .into_bytes(),
    )
}

fn status_for_runtime_error(err: &RuntimeError) -> u16 {
    if err.code.as_deref() == Some("payload_too_large") {
        return 413;
    }
    match err.kind {
        ErrorKind::InvalidInput => 400,
        ErrorKind::NotFound => 404,
        ErrorKind::Conflict => 409,
        ErrorKind::Policy => 403,
        ErrorKind::Unavailable => 503,
        ErrorKind::Io => 500,
    }
}

pub struct RequestContext<'a> {
    pub workspace: &'a Path,
    pub name: &'a str,
    pub previous: &'a Value,
    pub benchmark: Option<&'a Path>,
    pub feedback_store: &'a mut FeedbackStore,
}

pub fn handle_request(
    method: &str,
    raw_url: &str,
    content_type: Option<&str>,
    body_reader: &mut impl Read,
    ctx: &mut RequestContext<'_>,
) -> Result<tiny_http::Response<std::io::Cursor<Vec<u8>>>> {
    let path = request_path(raw_url);
    Ok(match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let rendered = (|| -> Result<String> {
                let runs = find_runs(ctx.workspace)?;
                let b = ctx.benchmark.and_then(|p| common::json_file(p).ok());
                generate_html(&runs, ctx.name, ctx.previous, b.as_ref(), false)
            })();
            match rendered {
                Ok(html) => response(200, "text/html; charset=utf-8", html.into_bytes()),
                Err(error) => json_error_response(
                    status_for_runtime_error(&error),
                    error.code.as_deref(),
                    error.to_string(),
                ),
            }
        }
        ("GET", "/api/feedback") => match ctx.feedback_store.read() {
            Ok(bytes) => response(200, "application/json", bytes),
            Err(error) => json_error_response(
                status_for_runtime_error(&error),
                error.code.as_deref(),
                error.to_string(),
            ),
        },
        ("POST", "/api/feedback") => {
            if !content_type.is_some_and(review_feedback::json_content_type) {
                return Ok(json_error_response(
                    415,
                    Some("unsupported_media_type"),
                    "Feedback requires application/json",
                ));
            }
            let mut body = vec![];
            if let Err(error) = body_reader
                .take((MAX_FEEDBACK_BYTES + 1) as u64)
                .read_to_end(&mut body)
            {
                let status = if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::UnexpectedEof
                ) {
                    400
                } else {
                    500
                };
                return Ok(json_error_response(status, None, error.to_string()));
            }
            if body.len() > MAX_FEEDBACK_BYTES {
                return Ok(json_error_response(
                    413,
                    Some("payload_too_large"),
                    "Feedback exceeds limit",
                ));
            }
            match ctx.feedback_store.save(&body) {
                Ok(()) => response(200, "application/json", b"{\"ok\":true}".to_vec()),
                Err(error) => json_error_response(
                    status_for_runtime_error(&error),
                    error.code.as_deref(),
                    error.to_string(),
                ),
            }
        }
        _ => response(404, "text/plain", b"Not found".to_vec()),
    })
}

pub fn execute(v: &Value) -> Result<Value> {
    let bypass = common::flag(v, "strict") || common::flag(v, "no_cache") || is_cache_bypassed();
    let workspace = common::absolute(common::positional(v, "workspace", 0)?)?;
    if !workspace.is_dir() {
        return Err(RuntimeError::not_found(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    let runs = find_runs_with_options(&workspace, common::now_unix_ns(), bypass)?;
    if runs.is_empty() {
        return Err(RuntimeError::not_found(format!(
            "No runs found in {}",
            workspace.display()
        )));
    }
    let name = common::text(v, "skill_name")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            workspace
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .replace("-workspace", "")
        });
    let previous = common::text(v, "previous_workspace")
        .map(|p| load_previous_iteration_with_options(Path::new(p), common::now_unix_ns(), bypass))
        .transpose()?
        .unwrap_or(json!({}));
    let benchmark = common::text(v, "benchmark").map(PathBuf::from);
    let benchmark_data = benchmark.as_deref().and_then(|p| common::json_file(p).ok());

    if let Some(path) = common::text(v, "static") {
        let html = generate_html(&runs, &name, &previous, benchmark_data.as_ref(), true)?;
        common::atomic_write(Path::new(path), html.as_bytes())?;
        return common::json_cli(
            json!({"static": path, "runs": runs.len()}),
            String::new(),
        );
    }

    let n = common::number(v, "port", 3117.0)?;
    if n < 0.0 || n > 65535.0 || n.fract() != 0.0 {
        return Err(RuntimeError::invalid("port must be in 0..65535"));
    }
    let mut feedback_store = FeedbackStore::open(&workspace)?;
    let feedback_path = workspace.join("feedback.json");
    let server = tiny_http::Server::http(("127.0.0.1", n as u16))
        .or_else(|_| tiny_http::Server::http(("127.0.0.1", 0)))
        .map_err(|e| RuntimeError::unavailable(e.to_string()))?;
    let url = format!("http://{}", server.server_addr());
    println!(
        "Eval Viewer URL: {url}\nWorkspace: {}\nFeedback: {}\nPress Ctrl+C to stop.",
        workspace.display(),
        feedback_path.display()
    );
    let suppress_browser = common::flag(v, "no_browser")
        || std::env::var_os("INFRA_NO_BROWSER").is_some_and(|val| !val.is_empty() && val != "0");
    if !suppress_browser {
        let _ = common::browser(&url);
    }

    let mut ctx = RequestContext {
        workspace: &workspace,
        name: &name,
        previous: &previous,
        benchmark: benchmark.as_deref(),
        feedback_store: &mut feedback_store,
    };
    with_cache_bypass(bypass, || -> Result<()> {
        while !cancel::interrupted() {
            let Some(mut request) = server
                .recv_timeout(Duration::from_millis(200))
                .map_err(|e| RuntimeError::unavailable(e.to_string()))?
            else {
                continue;
            };
            let raw_url = request.url().to_owned();
            let method = request.method().as_str().to_owned();
            let content_type = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str().to_owned());
            let reply = handle_request(
                &method,
                &raw_url,
                content_type.as_deref(),
                &mut request.as_reader(),
                &mut ctx,
            )?;
            let _ = request.respond(reply);
        }
        Ok(())
    })?;
    common::json_cli(json!({"url": url, "feedback": feedback_path}), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdMap;

    fn header_map(headers: &[tiny_http::Header]) -> StdMap<String, String> {
        headers
            .iter()
            .map(|h| (h.field.as_str().to_string().to_ascii_lowercase(), h.value.as_str().to_string()))
            .collect()
    }

    fn assert_security_headers(headers: &[tiny_http::Header]) {
        let map = header_map(headers);
        assert_eq!(map.get("x-content-type-options").map(String::as_str), Some("nosniff"));
        assert_eq!(map.get("referrer-policy").map(String::as_str), Some("no-referrer"));
        assert_eq!(map.get("cross-origin-resource-policy").map(String::as_str), Some("same-origin"));
        assert_eq!(map.get("x-frame-options").map(String::as_str), Some("DENY"));
        let csp = map.get("content-security-policy").expect("CSP header");
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("object-src 'none'"));
    }

    #[test]
    fn http_handlers_enforce_csp_query_stripping_validation_and_non_fatal_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let run1 = ws.join("eval-1/with_skill");
        std::fs::create_dir_all(run1.join("outputs")).unwrap();
        std::fs::write(run1.join("outputs/result.txt"), "hello").unwrap();
        std::fs::write(run1.join("transcript.md"), "## Eval Prompt\n\nTest prompt\n").unwrap();
        std::fs::write(run1.join("metrics.json"), "{}").unwrap();
        std::fs::write(run1.join("user_notes.md"), "note").unwrap();

        let runs = find_runs(&ws).unwrap();
        let names: Vec<_> = runs[0]["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["name"].as_str())
            .collect();
        assert!(names.contains(&"transcript.md"));
        assert!(names.contains(&"metrics.json"));
        assert!(names.contains(&"user_notes.md"));

        let mut store = FeedbackStore::open(&ws).unwrap();
        let prev = Value::Null;
        let mut ctx = RequestContext {
            workspace: &ws,
            name: "demo",
            previous: &prev,
            benchmark: None,
            feedback_store: &mut store,
        };

        let get_home = handle_request("GET", "/?t=123", None, &mut std::io::Cursor::new(&[]), &mut ctx).unwrap();
        assert_eq!(get_home.status_code().0, 200);
        assert_security_headers(get_home.headers());

        let get_fb = handle_request("GET", "/api/feedback?t=456", None, &mut std::io::Cursor::new(&[]), &mut ctx).unwrap();
        assert_eq!(get_fb.status_code().0, 200);
        assert_security_headers(get_fb.headers());

        let wrong_ct = handle_request("POST", "/api/feedback", Some("text/plain"), &mut std::io::Cursor::new(br#"{"reviews":[]}"#), &mut ctx).unwrap();
        assert_eq!(wrong_ct.status_code().0, 415);
        assert_security_headers(wrong_ct.headers());

        let too_big = vec![b' '; MAX_FEEDBACK_BYTES + 1];
        let over = handle_request("POST", "/api/feedback", Some("application/json"), &mut std::io::Cursor::new(&too_big), &mut ctx).unwrap();
        assert_eq!(over.status_code().0, 413);

        let ok_post = handle_request("POST", "/api/feedback", Some("application/json; charset=utf-8"), &mut std::io::Cursor::new(br#"{"reviews":[{"run_id":"eval-1-with_skill","feedback":"ok"}]}"#), &mut ctx).unwrap();
        assert_eq!(ok_post.status_code().0, 200);
        assert_security_headers(ok_post.headers());

        let not_found = handle_request("GET", "/unknown", None, &mut std::io::Cursor::new(&[]), &mut ctx).unwrap();
        assert_eq!(not_found.status_code().0, 404);
        assert_security_headers(not_found.headers());

        // Non-fatal error when feedback.json becomes a directory
        std::fs::remove_file(ws.join("feedback.json")).unwrap();
        std::fs::create_dir_all(ws.join("feedback.json")).unwrap();
        let err_reply = handle_request("GET", "/api/feedback", None, &mut std::io::Cursor::new(&[]), &mut ctx).unwrap();
        assert!(err_reply.status_code().0 >= 400);
        assert_security_headers(err_reply.headers());
    }

    #[test]
    fn settled_racy_ns_caches_skip_redundant_walks_and_respect_bypass() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let run1 = ws.join("eval-1/with_skill");
        std::fs::create_dir_all(run1.join("outputs")).unwrap();
        let img = run1.join("outputs/chart.png");
        std::fs::write(&img, b"\x89PNG\r\n\x1a\nAAAA").unwrap();

        let stamp = FileStatStamp::from_metadata(&std::fs::symlink_metadata(&img).unwrap());
        let racy_now = stamp.mtime_ns + REVIEW_RACY_NS - 1;
        let settled_now = stamp.mtime_ns + REVIEW_RACY_NS + 10_000_000;

        let r_before = embed_read_count();
        let _ = embed_file_with_now(&img, racy_now, false);
        let _ = embed_file_with_now(&img, racy_now, false);
        assert_eq!(embed_read_count(), r_before + 2);

        let e1 = embed_file_with_now(&img, settled_now, false);
        let e2 = embed_file_with_now(&img, settled_now + 5, false);
        assert_eq!(e1, e2);
        assert_eq!(embed_read_count(), r_before + 3);

        let _ = embed_file_with_now(&img, settled_now + 10, true);
        assert_eq!(embed_read_count(), r_before + 4);

        let w_before = find_runs_walk_count();
        let _ = find_runs_with_options(&ws, racy_now, false).unwrap();
        assert_eq!(find_runs_walk_count(), w_before + 1);

        let runs1 = find_runs_with_options(&ws, settled_now, false).unwrap();
        let reads_after_warm = embed_read_count();
        let runs2 = find_runs_with_options(&ws, settled_now + 20, false).unwrap();
        assert_eq!(runs1, runs2);
        assert_eq!(find_runs_walk_count(), w_before + 2);
        assert_eq!(embed_read_count(), reads_after_warm);

        let _ = find_runs_with_options(&ws, settled_now + 30, true).unwrap();
        assert_eq!(find_runs_walk_count(), w_before + 3);

        // Same-length output edit invalidates settled run walk cache cleanly.
        std::fs::write(&img, b"\x89PNG\r\n\x1a\nBBBB").unwrap();
        let stamp2 = FileStatStamp::from_metadata(&std::fs::symlink_metadata(&img).unwrap());
        let runs_updated = find_runs_with_options(&ws, stamp2.mtime_ns + REVIEW_RACY_NS + 10_000, false).unwrap();
        assert_ne!(runs_updated[0]["outputs"], runs1[0]["outputs"]);
    }
}
