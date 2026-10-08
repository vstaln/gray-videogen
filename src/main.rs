//! gray-videogen — async text-to-video across three env-keyed providers.
//!
//! Port of hermes' video_gen plugins (fal / deepinfra / xai). One `video_gen`
//! tool: {prompt, provider?:"auto", model?, seconds?:5, out?, action?,
//! job?}. fal and deepinfra are queue APIs; xAI is async too. Because the
//! host-side tool TTL is ~30s, generation polls for ~20s and then PERSISTS the
//! job to `~/.gray/videogen/jobs.json`, replying with a job id — `video_gen
//! {action:"status", job:"<id>"}` completes the download later.
//!
//! HTTP is shelled out to `curl`; keys come from FAL_KEY, DEEPINFRA_API_KEY,
//! XAI_API_KEY — never hardcoded.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

const POLL_WINDOW_S: u64 = 20;
const POLL_STEP_S: u64 = 2;

fn manifest() -> Value {
    json!({
        "name": "videogen",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [{
            "name": "video_gen",
            "description": "Generate a short video from a text prompt. Providers (auto-probed by env key): fal (FAL_KEY), deepinfra (DEEPINFRA_API_KEY), xai (XAI_API_KEY). Generation is async: if the job isn't done in ~20s you get a job id — call again with action=status + job to finish the download.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action":   {"type": "string", "enum": ["generate", "status", "fetch"], "description": "generate (default) submits a new job; status/fetch check a persisted job and download when done"},
                    "job":      {"type": "string", "description": "Job id for action=status/fetch"},
                    "prompt":   {"type": "string", "description": "What should happen in the clip"},
                    "provider": {"type": "string", "description": "auto (default) | fal | deepinfra | xai"},
                    "model":    {"type": "string", "description": "Provider-specific model id override"},
                    "seconds":  {"type": "integer", "description": "Clip length in seconds (default 5)"},
                    "out":      {"type": "string", "description": "Output mp4 path, default ./video-<ts>.mp4"}
                }
            }
        }],
        "commands": ["/videogen"],
    })
}

// ---------- helpers ----------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("videogen")
}

fn jobs_path() -> PathBuf {
    state_dir().join("jobs.json")
}

fn load_jobs() -> Value {
    std::fs::read_to_string(jobs_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(json!({}))
}

fn save_jobs(jobs: &Value) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(jobs_path(), serde_json::to_string_pretty(jobs).unwrap());
}

fn env_key(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

fn split_status(out: std::process::Output) -> Result<(u16, String), String> {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    let status: u16 = status.trim().parse().unwrap_or(0);
    if status == 0 && !out.status.success() {
        return Err(format!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok((status, body.to_string()))
}

fn auth_header(kind: &str, token: &str) -> String {
    if kind == "fal" {
        format!("Authorization: Key {token}")
    } else {
        format!("Authorization: Bearer {token}")
    }
}

fn post_json(url: &str, kind: &str, token: &str, body: &Value) -> Result<(u16, String), String> {
    let out = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "60", "-w", "\n%{http_code}", "-X", "POST", url])
        .args(["-H", &auth_header(kind, token), "-H", "Content-Type: application/json", "-d"])
        .arg(body.to_string())
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    split_status(out)
}

fn get(url: &str, kind: &str, token: &str) -> Result<(u16, String), String> {
    let mut args: Vec<String> = vec!["-sS".into(), "--max-time".into(), "30".into(), "-w".into(), "\n%{http_code}".into()];
    if !token.is_empty() {
        args.push("-H".into());
        args.push(auth_header(kind, token));
    }
    args.push(url.into());
    let out = std::process::Command::new("curl")
        .args(&args)
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    split_status(out)
}

fn api_err(provider: &str, status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/error"))
                .or_else(|| v.pointer("/detail"))
                .map(|d| d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string()))
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    format!("{provider} → HTTP {status}: {detail}")
}

fn out_path(arg: Option<&str>, session: &Value) -> PathBuf {
    let mut name = arg.unwrap_or("").trim().to_string();
    if name.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        name = format!("video-{ts}.mp4");
    }
    let path = if let Some(rest) = name.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(&name))
    } else {
        PathBuf::from(&name)
    };
    if path.is_relative() {
        if let Some(cwd) = session.pointer("/cwd").and_then(Value::as_str) {
            return PathBuf::from(cwd).join(path);
        }
    }
    path
}

fn download(url: &str, path: &std::path::Path) -> Result<u64, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let status = std::process::Command::new("curl")
        .args(["-sSL", "--max-time", "180", "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if !status.success() {
        return Err(format!("video download failed for {url}"));
    }
    Ok(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
}

// ---------- providers --------------------------------------------------------
//
// Each provider submits a job and returns a partially-filled job record:
//   {provider, poll_url, response_url?, status_field, out, prompt, model}
// poll_url is re-GET'd on every check; the per-provider `check` functions
// interpret the body into (done?, video_url?).

fn providers() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("fal", "FAL_KEY", "fal-ai/pixverse/v6/text-to-video"),
        ("deepinfra", "DEEPINFRA_API_KEY", "PrunaAI/p-video"),
        ("xai", "XAI_API_KEY", "grok-imagine-video"),
    ]
}

fn submit(provider: &str, token: &str, model: &str, prompt: &str, seconds: i64) -> Result<Value, String> {
    match provider {
        "fal" => {
            let body = json!({
                "prompt": prompt,
                "duration": seconds.to_string(),
                "resolution": "720p",
            });
            let url = format!("https://queue.fal.run/{model}");
            let (status, text) = post_json(&url, "fal", token, &body)?;
            let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            if !(200..300).contains(&status) {
                return Err(api_err("fal", status, &text));
            }
            let request_id = v.get("request_id").and_then(Value::as_str).unwrap_or("");
            let status_url = v.get("status_url").and_then(Value::as_str).unwrap_or("");
            let response_url = v.get("response_url").and_then(Value::as_str).unwrap_or("");
            if status_url.is_empty() {
                // Synchronous answer (rare on queue): treat as done.
                if let Some(u) = v.pointer("/video/url").and_then(Value::as_str) {
                    return Ok(json!({"provider": "fal", "job_id": format!("fal-{request_id}"),
                                     "status": "done", "video_url": u}));
                }
                return Err(format!("fal submit gave no status_url: {}", text.chars().take(200).collect::<String>()));
            }
            Ok(json!({
                "provider": "fal", "job_id": format!("fal-{request_id}"),
                "status": "running", "poll_url": status_url, "response_url": response_url,
            }))
        }
        "deepinfra" => {
            let body = json!({
                "model": model,
                "prompt": prompt,
                "seconds": seconds.to_string(),
                "size": "1280x720",
            });
            let url = "https://api.deepinfra.com/v1/openai/videos";
            let (status, text) = post_json(url, "bearer", token, &body)?;
            let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            if !(200..300).contains(&status) {
                return Err(api_err("deepinfra", status, &text));
            }
            let id = v.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                return Err(format!("deepinfra submit gave no job id: {}", text.chars().take(200).collect::<String>()));
            }
            Ok(json!({
                "provider": "deepinfra", "job_id": format!("deepinfra-{id}"),
                "status": "running",
                "poll_url": format!("https://api.deepinfra.com/v1/openai/videos/{id}"),
                "content_url": format!("https://api.deepinfra.com/v1/openai/videos/{id}/content"),
            }))
        }
        "xai" => {
            let body = json!({
                "model": model,
                "prompt": prompt,
                "duration": seconds,
                "aspect_ratio": "16:9",
                "resolution": "720p",
            });
            let url = "https://api.x.ai/v1/videos/generations";
            let (status, text) = post_json(url, "bearer", token, &body)?;
            let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            if !(200..300).contains(&status) {
                return Err(api_err("xai", status, &text));
            }
            let id = v.get("request_id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                return Err(format!("xai submit gave no request_id: {}", text.chars().take(200).collect::<String>()));
            }
            Ok(json!({
                "provider": "xai", "job_id": format!("xai-{id}"),
                "status": "running",
                "poll_url": format!("https://api.x.ai/v1/videos/{id}"),
            }))
        }
        other => Err(format!("unknown provider '{other}' — fal|deepinfra|xai")),
    }
}

/// Poll a job once. Returns updated job record: status running|done|failed and,
/// when done, `video_url`.
fn check_job(job: &Value) -> Result<Value, String> {
    let provider = job.get("provider").and_then(Value::as_str).unwrap_or("");
    let env = providers()
        .iter()
        .find(|(n, _, _)| *n == provider)
        .map(|(_, e, _)| *e)
        .ok_or_else(|| format!("unknown provider in job record: {provider}"))?;
    let token = env_key(env)?;
    let poll_url = job.get("poll_url").and_then(Value::as_str).unwrap_or("");
    if poll_url.is_empty() {
        return Err("job record has no poll_url".into());
    }
    let kind = if provider == "fal" { "fal" } else { "bearer" };
    let (status, text) = get(poll_url, kind, &token)?;
    let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    if !(200..300).contains(&status) {
        return Err(api_err(provider, status, &text));
    }
    let mut job = job.clone();
    match provider {
        "fal" => {
            match v.get("status").and_then(Value::as_str).unwrap_or("") {
                "COMPLETED" => {
                    let response_url = job.get("response_url").and_then(Value::as_str).unwrap_or("");
                    let (rs, rtext) = get(response_url, "fal", &token)?;
                    let rv: Value = serde_json::from_str(&rtext).unwrap_or(json!({}));
                    if !(200..300).contains(&rs) {
                        return Err(api_err("fal", rs, &rtext));
                    }
                    let u = rv.pointer("/video/url").and_then(Value::as_str)
                        .ok_or_else(|| format!("fal result had no video.url: {}", rtext.chars().take(200).collect::<String>()))?;
                    job["status"] = json!("done");
                    job["video_url"] = json!(u);
                }
                "FAILED" | "CANCELLED" => {
                    job["status"] = json!("failed");
                    job["error"] = json!(text.chars().take(200).collect::<String>());
                }
                s => job["status"] = json!(if s.is_empty() { "running" } else { s }),
            }
        }
        "deepinfra" => {
            match v.get("status").and_then(Value::as_str).unwrap_or("") {
                "succeeded" | "completed" => {
                    let url = v.pointer("/data/0/url")
                        .or_else(|| v.pointer("/video/url"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    match url {
                        Some(u) => {
                            job["status"] = json!("done");
                            job["video_url"] = json!(u);
                        }
                        None => {
                            // Fall back to the content endpoint.
                            let content_url = job.get("content_url").and_then(Value::as_str).unwrap_or("").to_string();
                            job["status"] = json!("done");
                            job["content_fetch"] = json!(content_url);
                        }
                    }
                }
                "failed" | "error" | "canceled" | "cancelled" => {
                    job["status"] = json!("failed");
                    job["error"] = json!(text.chars().take(200).collect::<String>());
                }
                s => job["status"] = json!(if s.is_empty() { "running" } else { s }),
            }
        }
        "xai" => {
            match v.get("status").and_then(Value::as_str).unwrap_or("").to_lowercase().as_str() {
                "done" => {
                    let u = v.pointer("/video/file_output/public_url")
                        .or_else(|| v.pointer("/video/url"))
                        .and_then(Value::as_str)
                        .ok_or("xai job done but no video url")?;
                    job["status"] = json!("done");
                    job["video_url"] = json!(u);
                }
                "failed" | "error" | "expired" | "cancelled" => {
                    job["status"] = json!("failed");
                    job["error"] = json!(text.chars().take(200).collect::<String>());
                }
                s => job["status"] = json!(if s.is_empty() { "running" } else { s }),
            }
        }
        _ => return Err(format!("unknown provider {provider}")),
    }
    Ok(job)
}

// ---------- actions ----------------------------------------------------------

fn still_running(job_id: &str) -> String {
    format!(
        "job {job_id} still running — call video_gen {{action:\"status\",job:\"{job_id}\"}} to check"
    )
}

fn finish_download(job: &Value, session: &Value) -> Result<String, String> {
    let job_id = job.get("job_id").and_then(Value::as_str).unwrap_or("?");
    let out = job.get("out").and_then(Value::as_str).unwrap_or("");
    let path = if out.is_empty() {
        out_path(None, session)
    } else {
        PathBuf::from(out)
    };
    let provider = job.get("provider").and_then(Value::as_str).unwrap_or("");
    if let Some(u) = job.get("video_url").and_then(Value::as_str) {
        let bytes = download(u, &path)?;
        return Ok(format!("saved {} ({} bytes, {provider})", path.display(), bytes));
    }
    if let Some(content) = job.get("content_fetch").and_then(Value::as_str) {
        let env = providers().iter().find(|(n, _, _)| *n == provider).map(|(_, e, _)| *e).unwrap_or("");
        let token = env_key(env)?;
        // Content endpoints return the bytes directly.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }
        let status = std::process::Command::new("curl")
            .args(["-sSL", "--max-time", "180", "-o"])
            .arg(&path)
            .args(["-H", &auth_header("bearer", &token)])
            .arg(content)
            .status()
            .map_err(|e| format!("couldn't run curl: {e}"))?;
        if !status.success() {
            return Err("content download failed".into());
        }
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        return Ok(format!("saved {} ({} bytes, {provider})", path.display(), bytes));
    }
    Err(format!("job {job_id} done but no video location recorded"))
}

fn act_generate(args: &Value, session: &Value) -> Result<String, String> {
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("").trim();
    if prompt.is_empty() {
        return Err("video_gen needs a prompt".into());
    }
    let seconds = args.get("seconds").and_then(Value::as_i64).unwrap_or(5).clamp(1, 15);
    let model_arg = args.get("model").and_then(Value::as_str).unwrap_or("");
    let requested = args.get("provider").and_then(Value::as_str).unwrap_or("auto");
    let out = out_path(args.get("out").and_then(Value::as_str), session);

    let targets: Vec<(&str, &str, &str)> = if requested == "auto" || requested.is_empty() {
        providers()
            .into_iter()
            .filter(|(_, env, _)| std::env::var(env).map(|v| !v.trim().is_empty()).unwrap_or(false))
            .collect()
    } else {
        match providers().into_iter().find(|(n, _, _)| *n == requested) {
            Some(p) => vec![p],
            None => return Err(format!("unknown provider '{requested}' — fal|deepinfra|xai")),
        }
    };
    if targets.is_empty() {
        return Err(if requested == "auto" || requested.is_empty() {
            "no video provider configured — set one of FAL_KEY, DEEPINFRA_API_KEY, XAI_API_KEY".into()
        } else {
            format!("provider '{requested}' selected but its key is not set")
        });
    }

    let mut tried: Vec<String> = Vec::new();
    for (name, env, default_model) in targets {
        let token = match env_key(env) {
            Ok(t) => t,
            Err(e) => return Err(e),
        };
        let model = if model_arg.is_empty() {
            std::env::var(format!("{}_VIDEO_MODEL", name.to_uppercase()))
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| default_model.to_string())
        } else {
            model_arg.to_string()
        };
        match submit(name, &token, &model, prompt, seconds) {
            Ok(mut job) => {
                let job_id = job.get("job_id").and_then(Value::as_str).unwrap_or("?").to_string();
                job["out"] = json!(out.to_string_lossy());
                job["prompt"] = json!(prompt);
                job["model"] = json!(model);
                job["submitted_at"] = json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
                if job["status"] == "done" {
                    return finish_download(&job, session);
                }
                // Poll within the host tool TTL, then persist + hand back the id.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_WINDOW_S);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(POLL_STEP_S));
                    match check_job(&job) {
                        Ok(j) => {
                            job = j;
                            match job["status"].as_str().unwrap_or("running") {
                                "done" => {
                                    persist(&job);
                                    return finish_download(&job, session);
                                }
                                "failed" => {
                                    persist(&job);
                                    let e = job["error"].as_str().unwrap_or("");
                                    tried.push(format!("{name}: job failed: {e}"));
                                    break;
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            tried.push(format!("{name}: poll failed: {e}"));
                            break;
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        persist(&job);
                        return Ok(still_running(&job_id));
                    }
                }
                if requested != "auto" && !requested.is_empty() {
                    return Err(tried.join("\n"));
                }
            }
            Err(e) => {
                tried.push(format!("{name}: {e}"));
                if requested != "auto" && !requested.is_empty() {
                    return Err(tried.join("\n"));
                }
            }
        }
    }
    Err(format!("all providers failed:\n{}", tried.join("\n")))
}

fn persist(job: &Value) {
    let mut jobs = load_jobs();
    let id = job.get("job_id").and_then(Value::as_str).unwrap_or("unknown");
    jobs[id] = job.clone();
    save_jobs(&jobs);
}

fn act_status(args: &Value, session: &Value) -> Result<String, String> {
    let job_id = args.get("job").and_then(Value::as_str).unwrap_or("").trim();
    if job_id.is_empty() {
        let jobs = load_jobs();
        let mut lines: Vec<String> = jobs
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(id, j)| format!("{id}  {}  {}", j["status"].as_str().unwrap_or("?"), j["prompt"].as_str().unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        lines.sort();
        return Ok(if lines.is_empty() {
            "no jobs — video_gen {prompt:…} submits one".into()
        } else {
            format!("jobs:\n{}", lines.join("\n"))
        });
    }
    let mut jobs = load_jobs();
    let Some(job) = jobs.get(job_id).cloned() else {
        return Err(format!("unknown job '{job_id}' — run video_gen {{action:\"status\"}} to list"));
    };
    let job = if job["status"] == "done" || job["status"] == "failed" {
        job
    } else {
        let j = check_job(&job)?;
        jobs[job_id] = j.clone();
        save_jobs(&jobs);
        j
    };
    match job["status"].as_str().unwrap_or("running") {
        "done" => finish_download(&job, session),
        "failed" => Err(format!("job {job_id} failed: {}", job["error"].as_str().unwrap_or(""))),
        s => Ok(format!("job {job_id} still running ({s})")),
    }
}

fn call_tool(name: &str, args: &Value, session: &Value) -> Result<String, String> {
    if name != "video_gen" {
        return Err(format!("unknown tool: {name}"));
    }
    match args.get("action").and_then(Value::as_str).unwrap_or("generate") {
        "status" | "fetch" => act_status(args, session),
        "generate" => act_generate(args, session),
        other => Err(format!("unknown action '{other}' — generate|status|fetch")),
    }
}

fn run_command(argv: &[&str]) -> String {
    let configured: Vec<&str> = providers()
        .iter()
        .filter(|(_, env, _)| std::env::var(env).map(|v| !v.trim().is_empty()).unwrap_or(false))
        .map(|(n, _, _)| *n)
        .collect();
    let mut jobs_note = String::new();
    let jobs = load_jobs();
    if let Some(m) = jobs.as_object() {
        if !m.is_empty() {
            jobs_note = format!(" Jobs on file: {}.", m.len());
        }
    }
    let _ = argv;
    format!(
        "gray-videogen {} — video_gen tool. Providers probed in order: fal → deepinfra → xai. Configured: {}.{}",
        env!("CARGO_PKG_VERSION"),
        if configured.is_empty() { "none".into() } else { configured.join(", ") },
        jobs_note,
    )
}

fn handle(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            let session = params.get("session").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args, &session) {
                Ok(text) => json!({ "content": text }),
                Err(e) => json!({ "content": e, "is_error": true }),
            }
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    struct TempHome;
    impl TempHome {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("gray-videogen-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            unsafe { std::env::set_var("GRAY_HOME", &dir) };
            for k in ["FAL_KEY", "DEEPINFRA_API_KEY", "XAI_API_KEY"] {
                unsafe { std::env::remove_var(k) };
            }
            Self
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let dir = std::env::temp_dir().join(format!("gray-videogen-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn call(method: &str, params: Value) -> Value {
        handle(&json!({ "id": 1, "method": method, "params": params })).0.unwrap()
    }

    fn tool(args: Value) -> Value {
        call(
            "tool/call",
            json!({ "name": "video_gen", "args": args, "session": {"cwd": "/tmp"} }),
        )["result"]
            .clone()
    }

    #[test]
    fn manifest_shape() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "videogen");
        assert_eq!(m["tools"][0]["name"], "video_gen");
        assert_eq!(m["commands"], json!(["/videogen"]));
    }

    #[test]
    fn no_provider_is_clear_error() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        let r = tool(json!({"prompt": "a boat"}));
        assert_eq!(r["is_error"], true);
        assert!(r["content"].as_str().unwrap().contains("FAL_KEY"));
    }

    #[test]
    fn explicit_provider_needs_its_key() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        let r = tool(json!({"prompt": "x", "provider": "xai"}));
        assert!(r["content"].as_str().unwrap().contains("XAI_API_KEY"));
    }

    #[test]
    fn status_lists_and_rejects_unknown_jobs() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        let r = tool(json!({"action": "status"}));
        assert!(r["content"].as_str().unwrap().contains("no jobs"));
        let r = tool(json!({"action": "status", "job": "fal-nope"}));
        assert_eq!(r["is_error"], true);
    }

    #[test]
    fn persisted_done_job_downloads() {
        let _g = LOCK.lock().unwrap();
        let _h = TempHome::new();
        let out = std::env::temp_dir().join(format!("vg-test-{}.mp4", std::process::id()));
        let job = json!({
            "provider": "xai", "job_id": "xai-test1", "status": "done",
            "video_url": "file:///etc/hostname", "out": out.to_string_lossy(),
            "prompt": "t", "model": "m",
        });
        persist(&job);
        let r = tool(json!({"action": "status", "job": "xai-test1"}));
        assert!(r["content"].as_str().unwrap().contains("saved"), "{r}");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
