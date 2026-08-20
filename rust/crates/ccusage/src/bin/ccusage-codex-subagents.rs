use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use ccusage_adapter_codex::{filter_events_by_date, load_codex_events};
use ccusage_cli::SharedArgs;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Default)]
struct Args {
    since: Option<String>,
    until: Option<String>,
    timezone: Option<String>,
    json: bool,
    single_thread: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    requests: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl Usage {
    fn add_event(&mut self, event: &ccusage_adapter_codex::CodexTokenUsageEvent) {
        self.requests += 1;
        self.input_tokens += event.input_tokens;
        self.cached_input_tokens += event.cached_input_tokens;
        self.output_tokens += event.output_tokens;
        self.reasoning_output_tokens += event.reasoning_output_tokens;
        self.total_tokens += event.total_tokens;
    }

    fn non_cached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    subagent: String,
    model: String,
    requests: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let mut shared = SharedArgs::with_defaults();
    shared.since = args.since.clone();
    shared.until = args.until.clone();
    shared.timezone = args.timezone.clone();
    shared.single_thread = args.single_thread;
    shared.json = args.json;

    let mut events = load_codex_events(&shared).map_err(|error| error.to_string())?;
    filter_events_by_date(&mut events, &shared).map_err(|error| error.to_string())?;

    let metadata = collect_session_subagents()?;
    let mut usage = BTreeMap::<(String, String), Usage>::new();
    for event in &events {
        let Some(subagent) = metadata.get(&event.session_id) else {
            continue;
        };
        let Some(model) = event.model.as_deref().filter(|model| !model.is_empty()) else {
            continue;
        };
        usage
            .entry((subagent.clone(), model.to_string()))
            .or_default()
            .add_event(event);
    }

    if args.json {
        let rows = usage
            .into_iter()
            .map(|((subagent, model), usage)| Row {
                subagent,
                model,
                requests: usage.requests,
                input_tokens: usage.non_cached_input_tokens(),
                cache_read_tokens: usage.cached_input_tokens,
                output_tokens: usage.output_tokens,
                reasoning_output_tokens: usage.reasoning_output_tokens,
                total_tokens: usage.total_tokens,
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "subagentModelUsage": rows }))
                .map_err(|error| error.to_string())?
        );
        return Ok(());
    }

    if usage.is_empty() {
        println!("No Codex subagent usage found.");
        return Ok(());
    }

    println!("Codex Subagent -> Model Usage");
    println!(
        "{:<28}  {:<24}  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}",
        "Subagent", "Model", "Requests", "Input", "Cache Read", "Output", "Reasoning", "Total"
    );
    println!("{}", "-".repeat(132));

    let mut totals = Usage::default();
    for ((subagent, model), row) in usage {
        println!(
            "{:<28}  {:<24}  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}",
            truncate(&subagent, 28),
            truncate(&model, 24),
            row.requests,
            row.non_cached_input_tokens(),
            row.cached_input_tokens,
            row.output_tokens,
            row.reasoning_output_tokens,
            row.total_tokens,
        );
        totals.requests += row.requests;
        totals.input_tokens += row.input_tokens;
        totals.cached_input_tokens += row.cached_input_tokens;
        totals.output_tokens += row.output_tokens;
        totals.reasoning_output_tokens += row.reasoning_output_tokens;
        totals.total_tokens += row.total_tokens;
    }
    println!("{}", "-".repeat(132));
    println!(
        "{:<28}  {:<24}  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}",
        "Total",
        "",
        totals.requests,
        totals.non_cached_input_tokens(),
        totals.cached_input_tokens,
        totals.output_tokens,
        totals.reasoning_output_tokens,
        totals.total_tokens,
    );

    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" | "-j" => parsed.json = true,
            "--single-thread" => parsed.single_thread = true,
            "--since" | "-s" => parsed.since = Some(next_value(&mut args, &arg)?),
            "--until" | "-u" => parsed.until = Some(next_value(&mut args, &arg)?),
            "--timezone" | "-z" => parsed.timezone = Some(next_value(&mut args, &arg)?),
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => return Err(format!("Unknown option '{arg}'. Run with --help for usage.")),
        }
    }
    Ok(parsed)
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("Missing value for {flag}"))
}

fn print_help() {
    println!(
        "ccusage-codex-subagents\n\nReport Codex token usage grouped by persisted subagent source and model.\n\nOptions:\n  -s, --since <date>       Inclusive start date (YYYY-MM-DD or YYYYMMDD)\n  -u, --until <date>       Inclusive end date (YYYY-MM-DD or YYYYMMDD)\n  -z, --timezone <tz>      Timezone used for date filtering\n  -j, --json               Emit JSON\n      --single-thread      Disable parallel Codex log parsing\n  -h, --help               Show this help\n\nCODEX_HOME may contain one path or a comma-separated list, matching ccusage codex."
    );
}

fn collect_session_subagents() -> Result<HashMap<String, String>, String> {
    let mut result = HashMap::new();
    for home in codex_homes()? {
        let sessions = home.join("sessions");
        let archived = home.join("archived_sessions");
        let roots = if sessions.is_dir() || archived.is_dir() {
            [sessions, archived]
                .into_iter()
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>()
        } else if home.is_dir() {
            vec![home]
        } else {
            Vec::new()
        };

        for root in roots {
            let mut files = Vec::new();
            collect_jsonl_files(&root, &mut files)?;
            files.sort();
            for file in files {
                if let Some(subagent) = read_subagent_label(&file)? {
                    let session_id = session_id(&root, &file);
                    result.entry(session_id).or_insert(subagent);
                }
            }
        }
    }
    Ok(result)
}

fn codex_homes() -> Result<Vec<PathBuf>, String> {
    if let Ok(value) = env::var("CODEX_HOME") {
        let homes = value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if !homes.is_empty() {
            return Ok(homes);
        }
    }

    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "Unable to determine the home directory; set CODEX_HOME explicitly.".to_string())?;
    Ok(vec![home.join(".codex")])
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Unable to read {}: {error}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn read_subagent_label(path: &Path) -> Result<Option<String>, String> {
    let file = File::open(path).map_err(|error| format!("Unable to open {}: {error}", path.display()))?;
    for line in BufReader::new(file).lines().take(32) {
        let line = line.map_err(|error| error.to_string())?;
        if !line.contains("session_meta") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let source = payload.get("source").unwrap_or(&Value::Null);
        let Some(subagent) = source
            .get("subAgent")
            .or_else(|| source.get("sub_agent"))
        else {
            return Ok(None);
        };
        return Ok(subagent_label(subagent, payload));
    }
    Ok(None)
}

fn subagent_label(source: &Value, payload: &Value) -> Option<String> {
    if let Some(name) = source.as_str() {
        return Some(normalize_subagent_name(name));
    }
    let object = source.as_object()?;
    if object.contains_key("review") {
        return Some("review".to_string());
    }
    if object.contains_key("compact") {
        return Some("compact".to_string());
    }
    if object.contains_key("memory_consolidation") || object.contains_key("memoryConsolidation") {
        return Some("memory-consolidation".to_string());
    }
    if let Some(other) = object.get("other").and_then(Value::as_str) {
        return Some(normalize_subagent_name(other));
    }
    if object.contains_key("thread_spawn") || object.contains_key("threadSpawn") {
        let role = payload
            .get("agent_role")
            .or_else(|| payload.get("agentRole"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let nickname = payload
            .get("agent_nickname")
            .or_else(|| payload.get("agentNickname"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        return Some(match (role, nickname) {
            (Some(role), Some(nickname)) => format!("thread-spawn:{role} ({nickname})"),
            (Some(role), None) => format!("thread-spawn:{role}"),
            (None, Some(nickname)) => format!("thread-spawn:{nickname}"),
            (None, None) => "thread-spawn".to_string(),
        });
    }
    Some("subagent".to_string())
}

fn normalize_subagent_name(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "guardian" | "auto_review" | "auto-review" => "auto-review".to_string(),
        "memory_consolidation" => "memory-consolidation".to_string(),
        other => other.to_string(),
    }
}

fn session_id(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut session_id = relative
        .with_extension("")
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    if session_id.is_empty() {
        session_id = "unknown".to_string();
    }
    session_id
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut result = value.chars().take(keep).collect::<String>();
    result.push('…');
    result
}
