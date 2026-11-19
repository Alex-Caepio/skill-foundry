//! Standalone interactive eval-output and eval-set HTML review generator/server.
use crate::{Result, RuntimeError, cancel, common, workbook_preview};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

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

pub fn embed_file(path: &Path) -> Value {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mime = get_mime_type(path);
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
            Ok(sheets) => json!({"name": name, "type": "xlsx", "data_b64": b64, "worksheets": sheets}),
            Err(e) => json!({"name": name, "type": "xlsx", "data_b64": b64, "render_error": e.to_string()}),
        },
        _ => json!({"name": name, "type": "binary", "mime": mime, "data_uri": data_uri}),
    }
}

pub fn build_run(root: &Path, directory: &Path) -> Result<Value> {
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
        let re = regex::Regex::new(r"(?s)## Eval Prompt\n\n(.*?)(?:\n##|$)").unwrap();
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
    if outs.is_dir() {
        for path in common::sorted(&outs)? {
            if path.is_file() {
                files.push(embed_file(&path));
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

fn recurse(
    root: &Path,
    current: &Path,
    runs: &mut Vec<Value>,
    seen: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    if !current.is_dir() {
        return Ok(());
    }
    let canonical = common::safe_canonicalize(current)?;
    if !canonical.starts_with(common::safe_canonicalize(root)?) || !seen.insert(canonical) {
        return Ok(());
    }
    if current.join("outputs").is_dir() {
        runs.push(build_run(root, current)?);
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
            recurse(root, &child, runs, seen)?;
        }
    }
    Ok(())
}

pub fn find_runs(workspace: &Path) -> Result<Vec<Value>> {
    let mut runs = vec![];
    recurse(workspace, workspace, &mut runs, &mut Default::default())?;
    runs.sort_by(|a, b| {
        let a_id = a["eval_id"].as_f64().unwrap_or(f64::INFINITY);
        let b_id = b["eval_id"].as_f64().unwrap_or(f64::INFINITY);
        a_id.total_cmp(&b_id)
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    Ok(runs)
}

pub fn load_previous_iteration(workspace: &Path) -> Result<Value> {
    if !workspace.is_dir() {
        return Ok(json!({}));
    }
    let feedback_path = workspace.join("feedback.json");
    let mut feedback = BTreeMap::new();
    if feedback_path.is_file()
        && let Ok(value) = common::json_file(&feedback_path)
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
    for run in find_runs(workspace)? {
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
        bench["configuration_order"] = json!(
            b["run_summary"]
                .as_object()
                .map(|m| m.keys().filter(|k| *k != "delta").cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );
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
        .replace("__SKILL_DESCRIPTION_PLACEHOLDER__", &common::html(&skill.description))
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

fn response(
    status: u16,
    mime: &str,
    body: Vec<u8>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(tiny_http::Header::from_bytes("Content-Type", mime).unwrap())
        .with_header(tiny_http::Header::from_bytes("X-Content-Type-Options", "nosniff").unwrap())
}

pub fn execute(v: &Value) -> Result<Value> {
    let workspace = common::absolute(common::positional(v, "workspace", 0)?)?;
    if !workspace.is_dir() {
        return Err(RuntimeError::not_found(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    let runs = find_runs(&workspace)?;
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
        .map(|p| load_previous_iteration(Path::new(p)))
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
    let server = tiny_http::Server::http(("127.0.0.1", n as u16))
        .or_else(|_| tiny_http::Server::http(("127.0.0.1", 0)))
        .map_err(|e| RuntimeError::unavailable(e.to_string()))?;
    let url = format!("http://{}", server.server_addr());
    let feedback_path = workspace.join("feedback.json");
    println!("Eval Viewer URL: {url}\nWorkspace: {}\nFeedback: {}\nPress Ctrl+C to stop.", workspace.display(), feedback_path.display());
    if !common::flag(v, "no_browser") {
        let _ = common::browser(&url);
    }

    while !cancel::interrupted() {
        let Some(mut request) = server
            .recv_timeout(Duration::from_millis(200))
            .map_err(|e| RuntimeError::unavailable(e.to_string()))?
        else {
            continue;
        };
        let path = request.url().to_owned();
        let method = request.method().as_str().to_owned();
        let reply = match (method.as_str(), path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                let runs = find_runs(&workspace)?;
                let b = benchmark.as_deref().and_then(|p| common::json_file(p).ok());
                response(
                    200,
                    "text/html; charset=utf-8",
                    generate_html(&runs, &name, &previous, b.as_ref(), false)?.into_bytes(),
                )
            }
            ("GET", "/api/feedback") => {
                let bytes = std::fs::read(&feedback_path).unwrap_or_else(|_| b"{}".to_vec());
                response(200, "application/json", bytes)
            }
            ("POST", "/api/feedback") => {
                let mut body = vec![];
                let _ = request.as_reader().take(8 * 1024 * 1024).read_to_end(&mut body);
                match serde_json::from_slice::<Value>(&body)
                    .ok()
                    .filter(|val| val.is_object() && val.get("reviews").is_some())
                {
                    Some(val) => {
                        let _ = common::write_json(&feedback_path, &val, true);
                        response(200, "application/json", b"{\"ok\":true}".to_vec())
                    }
                    None => response(400, "application/json", b"{\"error\":\"expected object with reviews\"}".to_vec()),
                }
            }
            _ => response(404, "text/plain", b"Not found".to_vec()),
        };
        let _ = request.respond(reply);
    }
    common::json_cli(json!({"url": url, "feedback": feedback_path}), String::new())
}
