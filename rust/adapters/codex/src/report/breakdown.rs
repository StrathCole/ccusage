use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use jiff::tz::TimeZone as JiffTimeZone;
use serde_json::Value;

use crate::{
    Align, CodexModelUsage, Color, PricingMap, Result, SimpleTable,
    cli::{AgentReportKind, SharedArgs, WeekDay},
    color, format_currency, format_date_tz, format_number, parse_ts_timestamp, parse_tz,
    print_box_title, week_start,
};

use super::{calculate_codex_model_cost, non_cached_input_tokens};
use crate::speed::CodexSpeedPolicy;
use crate::{loader, paths};

#[derive(Debug, Clone, Default)]
struct BreakdownUsage {
    usage: CodexModelUsage,
    requests: u64,
}

#[derive(Debug, Clone, Default)]
struct SessionAttribution {
    source: String,
    is_subagent: bool,
}

pub(super) fn print_codex_breakdowns(
    kind: AgentReportKind,
    pricing: &PricingMap,
    speed: CodexSpeedPolicy,
    shared: &SharedArgs,
) -> Result<()> {
    let mut events = loader::load_codex_events(shared)?;
    crate::filter_events_by_date(&mut events, shared)?;
    if events.is_empty() {
        return Ok(());
    }

    let attribution = load_session_attribution()?;
    let timezone = parse_tz(shared.timezone.as_deref()).or_else(|| Some(JiffTimeZone::system()));
    let mut by_period = BTreeMap::<(String, String, String), BreakdownUsage>::new();
    let mut by_subagent = BTreeMap::<(String, String), BreakdownUsage>::new();

    for event in &events {
        let Some(model) = event.model.as_deref().filter(|model| !model.is_empty()) else {
            continue;
        };
        let model = crate::model_aliases::resolve_model_name(model).into_owned();
        let timestamp = parse_ts_timestamp(&event.timestamp)
            .ok_or_else(|| crate::cli_error(format!("Invalid Codex timestamp: {}", event.timestamp)))?;
        let period = period_for(timestamp, event.session_id.as_str(), kind, timezone.as_ref());
        let session = attribution
            .get(&event.session_id)
            .cloned()
            .unwrap_or_else(|| SessionAttribution {
                source: "main".to_string(),
                is_subagent: false,
            });

        add_event(
            by_period
                .entry((period, session.source.clone(), model.clone()))
                .or_default(),
            event,
            &model,
        );
        if session.is_subagent {
            add_event(
                by_subagent
                    .entry((session.source, model.clone()))
                    .or_default(),
                event,
                &model,
            );
        }
    }

    print_period_model_breakdown(&by_period, kind, pricing, speed, shared)?;
    if !by_subagent.is_empty() {
        print_subagent_model_breakdown(&by_subagent, pricing, speed, shared)?;
    }
    Ok(())
}

fn add_event(
    target: &mut BreakdownUsage,
    event: &crate::CodexTokenUsageEvent,
    model: &str,
) {
    target.requests += 1;
    target.usage.input_tokens += event.input_tokens;
    target.usage.cached_input_tokens += event.cached_input_tokens;
    target.usage.output_tokens += event.output_tokens;
    target.usage.reasoning_output_tokens += event.reasoning_output_tokens;
    target.usage.total_tokens += event.total_tokens;
    let is_long_context = event.input_tokens > crate::pricing::long_context_split_threshold(model);
    if is_long_context {
        target.usage.long_context_input_tokens += event.input_tokens;
        target.usage.long_context_cached_input_tokens += event.cached_input_tokens;
        target.usage.long_context_output_tokens += event.output_tokens;
    }
    match event.service_tier {
        Some(crate::CodexServiceTier::Standard) => {
            target.usage.recorded_standard_usage.input_tokens += event.input_tokens;
            target.usage.recorded_standard_usage.cached_input_tokens += event.cached_input_tokens;
            target.usage.recorded_standard_usage.output_tokens += event.output_tokens;
            if is_long_context {
                target.usage.recorded_standard_usage.long_context_input_tokens += event.input_tokens;
                target
                    .usage
                    .recorded_standard_usage
                    .long_context_cached_input_tokens += event.cached_input_tokens;
                target.usage.recorded_standard_usage.long_context_output_tokens += event.output_tokens;
            }
        }
        Some(crate::CodexServiceTier::Fast) => {
            target.usage.recorded_fast_usage.input_tokens += event.input_tokens;
            target.usage.recorded_fast_usage.cached_input_tokens += event.cached_input_tokens;
            target.usage.recorded_fast_usage.output_tokens += event.output_tokens;
            if is_long_context {
                target.usage.recorded_fast_usage.long_context_input_tokens += event.input_tokens;
                target.usage.recorded_fast_usage.long_context_cached_input_tokens += event.cached_input_tokens;
                target.usage.recorded_fast_usage.long_context_output_tokens += event.output_tokens;
            }
        }
        None => {}
    }
    target.usage.is_fallback |= event.is_fallback_model;
}

fn period_for(
    timestamp: crate::TimestampMs,
    session_id: &str,
    kind: AgentReportKind,
    timezone: Option<&JiffTimeZone>,
) -> String {
    let date = format_date_tz(timestamp, timezone);
    match kind {
        AgentReportKind::Daily => date,
        AgentReportKind::Weekly => week_start(&date, WeekDay::Monday).unwrap_or(date),
        AgentReportKind::Monthly => date[..7].to_string(),
        AgentReportKind::Session => session_id.to_string(),
    }
}

fn print_period_model_breakdown(
    rows: &BTreeMap<(String, String, String), BreakdownUsage>,
    kind: AgentReportKind,
    pricing: &PricingMap,
    speed: CodexSpeedPolicy,
    shared: &SharedArgs,
) -> Result<()> {
    print_box_title(
        &format!(
            "Codex Usage Breakdown - {} / Source / Model",
            match kind {
                AgentReportKind::Daily => "Daily",
                AgentReportKind::Weekly => "Weekly",
                AgentReportKind::Monthly => "Monthly",
                AgentReportKind::Session => "Session",
            }
        ),
        shared,
    );
    print_rows(
        rows.iter().map(|((period, source, model), usage)| {
            (period.as_str(), source.as_str(), model.as_str(), usage)
        }),
        pricing,
        speed,
        shared,
    )
}

fn print_subagent_model_breakdown(
    rows: &BTreeMap<(String, String), BreakdownUsage>,
    pricing: &PricingMap,
    speed: CodexSpeedPolicy,
    shared: &SharedArgs,
) -> Result<()> {
    print_box_title("Codex Subagent -> Model Usage", shared);
    print_rows(
        rows.iter().map(|((source, model), usage)| {
            ("", source.as_str(), model.as_str(), usage)
        }),
        pricing,
        speed,
        shared,
    )
}

fn print_rows<'a>(
    rows: impl Iterator<Item = (&'a str, &'a str, &'a str, &'a BreakdownUsage)>,
    pricing: &PricingMap,
    speed: CodexSpeedPolicy,
    shared: &SharedArgs,
) -> Result<()> {
    let mut headers = vec![
        "Period",
        "Source",
        "Model",
        "Requests",
        "Input",
        "Output",
        "Reasoning",
        "Cache Read",
        "Total Tokens",
        "Cost (USD)",
    ];
    let mut aligns = vec![
        Align::Left,
        Align::Left,
        Align::Left,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
    ];
    if shared.no_cost {
        headers.pop();
        aligns.pop();
    }
    let mut table = SimpleTable::new(headers, aligns, crate::terminal_style(shared))
        .with_terminal_width(crate::terminal_width())
        .with_date_compaction(true);

    let mut total = BreakdownUsage::default();
    let mut total_cost = 0.0;
    for (period, source, model, row) in rows {
        let input = non_cached_input_tokens(row.usage.input_tokens, row.usage.cached_input_tokens);
        let cost = calculate_codex_model_cost(model, &row.usage, pricing, speed);
        let mut cells = vec![
            period.to_string(),
            source.to_string(),
            model.to_string(),
            format_number(row.requests),
            format_number(input),
            format_number(row.usage.output_tokens),
            format_number(row.usage.reasoning_output_tokens),
            format_number(row.usage.cached_input_tokens),
            format_number(row.usage.total_tokens),
            format_currency(cost),
        ];
        if shared.no_cost {
            cells.pop();
        }
        table.push(cells);
        total.requests += row.requests;
        total.usage.input_tokens += row.usage.input_tokens;
        total.usage.cached_input_tokens += row.usage.cached_input_tokens;
        total.usage.output_tokens += row.usage.output_tokens;
        total.usage.reasoning_output_tokens += row.usage.reasoning_output_tokens;
        total.usage.total_tokens += row.usage.total_tokens;
        total_cost += cost;
    }
    table.separator();
    let mut total_row = vec![
        color(shared, "Total", Color::Yellow),
        String::new(),
        String::new(),
        color(shared, format_number(total.requests), Color::Yellow),
        color(
            shared,
            format_number(non_cached_input_tokens(
                total.usage.input_tokens,
                total.usage.cached_input_tokens,
            )),
            Color::Yellow,
        ),
        color(shared, format_number(total.usage.output_tokens), Color::Yellow),
        color(
            shared,
            format_number(total.usage.reasoning_output_tokens),
            Color::Yellow,
        ),
        color(
            shared,
            format_number(total.usage.cached_input_tokens),
            Color::Yellow,
        ),
        color(shared, format_number(total.usage.total_tokens), Color::Yellow),
        color(shared, format_currency(total_cost), Color::Yellow),
    ];
    if shared.no_cost {
        total_row.pop();
    }
    table.push(total_row);
    Ok(table.print()?)
}

fn load_session_attribution() -> Result<HashMap<String, SessionAttribution>> {
    let mut result = HashMap::new();
    for source in paths::codex_usage_sources()? {
        for file in paths::collect_codex_usage_files(&source.dir) {
            let Some(attribution) = read_session_attribution(&file) else {
                continue;
            };
            result
                .entry(session_id(&source.dir, &file))
                .or_insert(attribution);
        }
    }
    Ok(result)
}

fn read_session_attribution(path: &Path) -> Option<SessionAttribution> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(32) {
        let line = line.ok()?;
        if !line.contains("session_meta") {
            continue;
        }
        let value = serde_json::from_str::<Value>(&line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        let source = payload.get("source")?;
        let subagent = source
            .get("subagent")
            .or_else(|| source.get("subAgent"))
            .or_else(|| source.get("sub_agent"));
        return match subagent {
            Some(subagent) => Some(SessionAttribution {
                source: subagent_label(subagent, payload),
                is_subagent: true,
            }),
            None if payload.get("thread_source").and_then(Value::as_str) == Some("subagent") => {
                Some(SessionAttribution {
                    source: payload
                        .get("agent_role")
                        .or_else(|| payload.get("agentRole"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map_or_else(|| "subagent".to_string(), |role| format!("subagent:{role}")),
                    is_subagent: true,
                })
            }
            None => Some(SessionAttribution {
                source: "main".to_string(),
                is_subagent: false,
            }),
        };
    }
    None
}

fn subagent_label(source: &Value, payload: &Value) -> String {
    if let Some(name) = source.as_str() {
        return normalize_subagent_name(name);
    }
    let Some(object) = source.as_object() else {
        return "subagent".to_string();
    };
    if object.contains_key("review") {
        return "review".to_string();
    }
    if object.contains_key("compact") {
        return "compact".to_string();
    }
    if object.contains_key("memory_consolidation") || object.contains_key("memoryConsolidation") {
        return "memory-consolidation".to_string();
    }
    if let Some(other) = object.get("other").and_then(Value::as_str) {
        return normalize_subagent_name(other);
    }
    if let Some(thread_spawn) = object
        .get("thread_spawn")
        .or_else(|| object.get("threadSpawn"))
    {
        let role = thread_spawn
            .get("agent_role")
            .or_else(|| thread_spawn.get("agentRole"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                payload
                    .get("agent_role")
                    .or_else(|| payload.get("agentRole"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
            });
        return role.map_or_else(
            || "thread-spawn".to_string(),
            |role| format!("thread-spawn:{role}"),
        );
    }
    "subagent".to_string()
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
    let mut id = relative
        .with_extension("")
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    if id.is_empty() {
        id = "unknown".to_string();
    }
    id
}
