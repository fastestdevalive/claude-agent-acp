//! Port of upstream `src/usage-markdown.ts` (pure): validation of the
//! experimental `get_usage` response and its Markdown rendering for the
//! `/usage` command.
//!
//! Everything here is data in, string out: the caller passes the validated
//! response and the local [`Zone`] used for reset times. Nothing reads the
//! clock, the environment or the file system, so the renderer is deterministic
//! and the wiring (Turn 4) owns the `/usage` command itself.
//!
//! Formatting mirrors the JavaScript primitives exactly, including the quirks:
//! `Intl.NumberFormat(.., { notation: "compact" })` rounds the *shortest
//! decimal representation* (so `999.95` renders `1K`), while `toFixed(2)` for
//! the cost rounds the exact binary value (so `1.005` renders `1.00`).

use std::cmp::Ordering;

use serde_json::Value;

/// Compact-notation units, largest last, as `Intl.NumberFormat` uses them for
/// `en`.
const COMPACT_UNITS: [(f64, &str); 4] = [(1e3, "K"), (1e6, "M"), (1e9, "B"), (1e12, "T")];

/// The number of cells in a usage bar.
const BAR_CELLS: usize = 20;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A local zone used to render a reset instant. The pure module never resolves
/// the host's zone: Turn 4 builds one `Zone` from the machine's offset and
/// short name (`PDT`, `GMT+2`, `UTC`, …) and passes it in, which is what
/// `Intl.DateTimeFormat(.., { timeZoneName: "short" })` renders under the
/// process's `TZ`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Offset east of UTC, in minutes (negative west).
    pub offset_minutes: i32,
    /// The short name `Intl` prints after the time.
    pub name: String,
}

impl Zone {
    /// The zone whose short name is `UTC` and offset zero.
    #[must_use]
    pub fn utc() -> Self {
        Self {
            offset_minutes: 0,
            name: "UTC".to_string(),
        }
    }

    /// A fixed-offset zone with an explicit short name.
    #[must_use]
    pub fn fixed(offset_minutes: i32, name: impl Into<String>) -> Self {
        Self {
            offset_minutes,
            name: name.into(),
        }
    }
}

/// Validate the experimental SDK response at the runtime boundary
/// (`parseUsageResponse`). Only the fields the renderer reads are checked, and
/// the complete response is preserved so future subtrees survive untouched.
#[must_use]
pub fn parse_usage_response(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let session = object.get("session")?.as_object()?;
    count_field(session.get("total_cost_usd"))?;
    count_field(session.get("total_api_duration_ms"))?;
    count_field(session.get("total_duration_ms"))?;
    let model_usage = session.get("model_usage")?.as_object()?;
    for model in model_usage.values() {
        let model = model.as_object()?;
        count_field(model.get("inputTokens"))?;
        count_field(model.get("outputTokens"))?;
        count_field(model.get("cacheReadInputTokens"))?;
        count_field(model.get("cacheCreationInputTokens"))?;
    }
    nullable_string(object.get("subscription_type"))?;
    object.get("rate_limits_available")?.as_bool()?;
    let rate_limits = object.get("rate_limits")?;
    if !rate_limits.is_null() {
        let rate_limits = rate_limits.as_object()?;
        optional_window(rate_limits.get("five_hour"))?;
        optional_window(rate_limits.get("seven_day"))?;
        optional_window(rate_limits.get("seven_day_oauth_apps"))?;
        optional_window(rate_limits.get("seven_day_opus"))?;
        optional_window(rate_limits.get("seven_day_sonnet"))?;
        if let Some(model_scoped) = rate_limits.get("model_scoped") {
            for model in model_scoped.as_array()? {
                let model = model.as_object()?;
                model.get("display_name")?.as_str()?;
                nullable_percent(model.get("utilization"))?;
                nullable_string(model.get("resets_at"))?;
            }
        }
        if let Some(extra_usage) = rate_limits.get("extra_usage") {
            if !extra_usage.is_null() {
                let extra_usage = extra_usage.as_object()?;
                extra_usage.get("is_enabled")?.as_bool()?;
                nullable_count(extra_usage.get("monthly_limit"))?;
                nullable_count(extra_usage.get("used_credits"))?;
                nullable_percent(extra_usage.get("utilization"))?;
                if let Some(currency) = extra_usage.get("currency") {
                    nullable_string(Some(currency))?;
                }
            }
        }
    }
    let behaviors = object.get("behaviors")?;
    if !behaviors.is_null() {
        let behaviors = behaviors.as_object()?;
        behavior_period(behaviors.get("day"))?;
        behavior_period(behaviors.get("week"))?;
    }
    Some(value.clone())
}

/// `/usage` is the only local command this renderer replaces
/// (`isUsageCommandText`): surrounding whitespace is ignored, nothing else is.
#[must_use]
pub fn is_usage_command_text(text: &str) -> bool {
    text.trim() == "/usage"
}

/// Escape the Markdown syntax characters of `value` and fold its line breaks
/// (`escapeMarkdown`).
#[must_use]
pub fn escape_markdown(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_break = false;
    for ch in value.chars() {
        if ch == '\r' || ch == '\n' {
            if !in_break {
                out.push(' ');
                in_break = true;
            }
            continue;
        }
        in_break = false;
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// A 20-cell usage bar (`usageBar`). `NaN` has no cells, as `"█".repeat(NaN)`
/// produces an empty string; infinities clamp to the ends.
#[must_use]
pub fn usage_bar(percent: f64) -> String {
    if percent.is_nan() {
        return String::new();
    }
    let clamped = percent.clamp(0.0, 100.0);
    let filled = if clamped == 0.0 {
        0
    } else {
        let cells = (clamped / 100.0 * BAR_CELLS as f64).round() as usize;
        cells.clamp(1, BAR_CELLS)
    };
    let mut bar = String::with_capacity(BAR_CELLS * 3);
    for cell in 0..BAR_CELLS {
        bar.push(if cell < filled { '█' } else { '░' });
    }
    bar
}

/// `formatDuration`: whole seconds, then `Nm Ms` past a minute.
#[must_use]
pub fn format_duration(milliseconds: f64) -> String {
    let seconds = (milliseconds / 1000.0).round();
    let seconds = if seconds < 0.0 { 0.0 } else { seconds };
    if seconds < 60.0 {
        return format!("{seconds}s");
    }
    let minutes = (seconds / 60.0).floor();
    let remainder = seconds % 60.0;
    if remainder == 0.0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m {remainder}s")
    }
}

/// `formatCount`: `Intl.NumberFormat("en", { notation: "compact",
/// maximumFractionDigits: 1 })`, including its unit re-selection when rounding
/// reaches the next magnitude (`999950` → `1M`).
#[must_use]
pub fn format_count(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "∞" } else { "-∞" }.to_string();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let magnitude = value.abs();
    let mut unit: Option<usize> = if magnitude >= 1e12 {
        Some(3)
    } else if magnitude >= 1e9 {
        Some(2)
    } else if magnitude >= 1e6 {
        Some(1)
    } else if magnitude >= 1e3 {
        Some(0)
    } else {
        None
    };
    loop {
        let divisor = unit.map_or(1.0, |index| COMPACT_UNITS[index].0);
        let rounded = round_shortest_decimal(magnitude / divisor, 1);
        let rounded_value: f64 = rounded.parse().unwrap_or(0.0);
        let next = match unit {
            None => Some(0),
            Some(index) if index < COMPACT_UNITS.len() - 1 => Some(index + 1),
            Some(_) => None,
        };
        if rounded_value >= 1000.0 {
            if let Some(next) = next {
                unit = Some(next);
                continue;
            }
        }
        let suffix = unit.map_or("", |index| COMPACT_UNITS[index].1);
        return format!("{sign}{}{suffix}", group_thousands(&rounded));
    }
}

/// `toFixed(2)` for the session cost, exact in the binary value like the
/// JavaScript primitive. `NaN`/`±Infinity` and magnitudes at or past `1e21`
/// keep JavaScript's `String` spellings.
#[must_use]
pub fn format_cost(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value.abs() >= 1e21 {
        return js_exponential(value);
    }
    let negative = value < 0.0;
    let (integer, fraction) = exact_decimal_parts(value.abs());
    let mut digits: Vec<u8> = integer.bytes().map(|byte| byte - b'0').collect();
    if digits.is_empty() {
        digits.push(0);
    }
    let mut kept: Vec<u8> = fraction.bytes().take(2).map(|byte| byte - b'0').collect();
    while kept.len() < 2 {
        kept.push(0);
    }
    if fraction
        .as_bytes()
        .get(2)
        .is_some_and(|digit| *digit >= b'5')
    {
        increment(&mut kept, &mut digits);
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(
        &digits
            .iter()
            .map(|digit| char::from(b'0' + digit))
            .collect::<String>(),
    );
    out.push('.');
    out.push_str(
        &kept
            .iter()
            .map(|digit| char::from(b'0' + digit))
            .collect::<String>(),
    );
    out
}

/// `formatReset`: empty for no reset, the raw value (escaped) when it is not a
/// date, and `Mon D, H:MM AM/PM ZONE` otherwise.
#[must_use]
pub fn format_reset(value: Option<&str>, zone: &Zone) -> String {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let Some(instant) = parse_instant(value, zone) else {
        return format!(" · Resets {}", escape_markdown(value));
    };
    let local = instant + i64::from(zone.offset_minutes) * 60_000;
    let days = local.div_euclid(86_400_000);
    let millis_of_day = local.rem_euclid(86_400_000);
    let seconds_of_day = millis_of_day / 1000;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let (_year, month, day) = civil_from_days(days);
    let hour12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    let meridiem = if hour < 12 { "AM" } else { "PM" };
    format!(
        " · Resets {} {day}, {hour12}:{minute:02} {meridiem} {}",
        MONTHS[(month - 1) as usize],
        zone.name
    )
}

/// Render the validated structured `/usage` response as Markdown
/// (`formatUsageResponse`).
#[must_use]
pub fn format_usage_response(usage: &Value, zone: &Zone) -> String {
    let mut lines: Vec<String> = vec!["## Usage".to_string()];
    if let Some(subscription) = usage.get("subscription_type").and_then(Value::as_str) {
        if !subscription.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "> Claude {} subscription usage",
                escape_markdown(subscription)
            ));
        }
    }

    let rate_limits_available = usage
        .get("rate_limits_available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if rate_limits_available {
        if let Some(rate_limits) = usage.get("rate_limits").filter(|limits| !limits.is_null()) {
            let mut limit_lines: Vec<String> = Vec::new();
            append_limit(
                &mut limit_lines,
                "5-hour limit",
                rate_limits.get("five_hour"),
                zone,
            );
            append_limit(
                &mut limit_lines,
                "Weekly · all models",
                rate_limits.get("seven_day"),
                zone,
            );
            let model_scoped = rate_limits
                .get("model_scoped")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for model in &model_scoped {
                let name = model
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                append_limit(
                    &mut limit_lines,
                    &format!("Weekly · {name}"),
                    Some(model),
                    zone,
                );
            }
            if model_scoped.is_empty() {
                append_limit(
                    &mut limit_lines,
                    "Weekly · Opus",
                    rate_limits.get("seven_day_opus"),
                    zone,
                );
                append_limit(
                    &mut limit_lines,
                    "Weekly · Sonnet",
                    rate_limits.get("seven_day_sonnet"),
                    zone,
                );
            }
            if !limit_lines.is_empty() {
                if limit_lines.last().is_some_and(String::is_empty) {
                    limit_lines.pop();
                }
                lines.push(String::new());
                lines.push("### Limits".to_string());
                lines.push(String::new());
                lines.extend(limit_lines);
            }
        }
    }

    let session = usage.get("session");
    let mut input = 0.0;
    let mut output = 0.0;
    let mut cache_read = 0.0;
    let mut cache_write = 0.0;
    if let Some(models) = session
        .and_then(|session| session.get("model_usage"))
        .and_then(Value::as_object)
    {
        for model in models.values() {
            input += number_field(model, "inputTokens");
            output += number_field(model, "outputTokens");
            cache_read += number_field(model, "cacheReadInputTokens");
            cache_write += number_field(model, "cacheCreationInputTokens");
        }
    }
    let cost = number_field_opt(session, "total_cost_usd");
    let api_time = number_field_opt(session, "total_api_duration_ms");
    let active_time = number_field_opt(session, "total_duration_ms");

    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("### This session".to_string());
    lines.push(String::new());
    lines.push("| Cost | API time | Active |".to_string());
    lines.push("|:--|:--|:--|".to_string());
    lines.push(format!(
        "| ${} | {} | {} |",
        format_cost(cost),
        format_duration(api_time),
        format_duration(active_time)
    ));
    lines.push(String::new());
    lines.push("| Breakdown | Tokens |".to_string());
    lines.push("|:--|--:|".to_string());
    lines.push(format!("| Input | {} |", format_count(input)));
    lines.push(format!("| Output | {} |", format_count(output)));
    lines.push(format!("| Cache read | {} |", format_count(cache_read)));
    lines.push(format!("| Cache write | {} |", format_count(cache_write)));

    if let Some(behaviors) = usage.get("behaviors").filter(|value| !value.is_null()) {
        lines.push(String::new());
        lines.push("---".to_string());
        lines.push(String::new());
        lines.push("### What’s using your limits?".to_string());
        lines.push(String::new());
        lines.push(
            "> Approximate, overlapping measures · this machine only · excludes claude.ai"
                .to_string(),
        );
        append_contributions(&mut lines, "Last 24h", behaviors.get("day"));
        append_contributions(&mut lines, "Last 7d", behaviors.get("week"));
    }

    lines.join("\n")
}

/// One `appendLimit` block: label, utilization, optional reset, bar. Windows
/// with a `null` utilization are skipped.
fn append_limit(lines: &mut Vec<String>, label: &str, window: Option<&Value>, zone: &Zone) {
    let Some(window) = window.filter(|window| !window.is_null()) else {
        return;
    };
    let Some(utilization) = window.get("utilization").and_then(Value::as_f64) else {
        return;
    };
    lines.push(format!(
        "**{}** — **{}%**{}",
        escape_markdown(label),
        utilization,
        format_reset(window.get("resets_at").and_then(Value::as_str), zone)
    ));
    lines.push(String::new());
    lines.push(format!("`{}`", usage_bar(utilization)));
    lines.push(String::new());
}

/// One `appendContributions` block: period totals and the top three MCP
/// servers by percentage (stable on ties).
fn append_contributions(lines: &mut Vec<String>, label: &str, period: Option<&Value>) {
    let Some(period) = period else {
        return;
    };
    lines.push(String::new());
    lines.push(format!(
        "**{label}** · {} requests · {} sessions",
        number_field(period, "request_count"),
        number_field(period, "session_count")
    ));
    let servers = period
        .get("mcp_servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if servers.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push("| MCP server | Usage |".to_string());
    lines.push("|:--|--:|".to_string());
    let mut sorted = servers;
    sorted.sort_by(|left, right| {
        number_field(right, "pct")
            .partial_cmp(&number_field(left, "pct"))
            .unwrap_or(Ordering::Equal)
    });
    for server in sorted.iter().take(3) {
        let name = server.get("name").and_then(Value::as_str).unwrap_or("");
        let pct = number_field(server, "pct");
        lines.push(format!(
            "| {} | `{}` {}% |",
            escape_markdown(name),
            usage_bar(pct),
            pct
        ));
    }
}

fn number_field(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn number_field_opt(value: Option<&Value>, key: &str) -> f64 {
    value.map_or(0.0, |value| number_field(value, key))
}

/// `z.number().finite().nonnegative()`, `None` when absent.
fn count_field(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    if number.is_finite() && number >= 0.0 {
        Some(number)
    } else {
        None
    }
}

/// `countSchema.nullable()`.
fn nullable_count(value: Option<&Value>) -> Option<()> {
    let value = value?;
    if value.is_null() {
        return Some(());
    }
    count_field(Some(value)).map(|_| ())
}

/// `percentSchema`: a count no greater than 100.
fn percent_field(value: Option<&Value>) -> Option<f64> {
    let number = count_field(value)?;
    if number <= 100.0 {
        Some(number)
    } else {
        None
    }
}

/// `percentSchema.nullable()`.
fn nullable_percent(value: Option<&Value>) -> Option<()> {
    let value = value?;
    if value.is_null() {
        return Some(());
    }
    percent_field(Some(value)).map(|_| ())
}

/// `z.string().nullable()`, `None` when absent.
fn nullable_string(value: Option<&Value>) -> Option<()> {
    let value = value?;
    if value.is_null() || value.is_string() {
        Some(())
    } else {
        None
    }
}

/// `usageWindowSchema` — absent, `null`, or an object with both keys.
fn optional_window(value: Option<&Value>) -> Option<()> {
    let Some(value) = value else {
        return Some(());
    };
    if value.is_null() {
        return Some(());
    }
    let window = value.as_object()?;
    nullable_percent(window.get("utilization"))?;
    nullable_string(window.get("resets_at"))?;
    Some(())
}

/// `behaviorPeriodSchema`.
fn behavior_period(value: Option<&Value>) -> Option<()> {
    let period = value?.as_object()?;
    count_field(period.get("request_count"))?;
    count_field(period.get("session_count"))?;
    for server in period.get("mcp_servers")?.as_array()? {
        let server = server.as_object()?;
        server.get("name")?.as_str()?;
        percent_field(server.get("pct"))?;
    }
    Some(())
}

/// Round the shortest round-trip decimal of `value` to `digits` fractional
/// digits, half away from zero, the way `Intl.NumberFormat` does.
fn round_shortest_decimal(value: f64, digits: usize) -> String {
    let repr = format!("{value}");
    let (integer, fraction) = repr.split_once('.').unwrap_or((repr.as_str(), ""));
    let mut integer_digits: Vec<u8> = integer.bytes().map(|byte| byte - b'0').collect();
    let fraction_digits: Vec<u8> = fraction.bytes().map(|byte| byte - b'0').collect();
    let keep = digits.min(fraction_digits.len());
    let mut kept: Vec<u8> = fraction_digits[..keep].to_vec();
    if fraction_digits.get(keep).is_some_and(|digit| *digit >= 5) {
        increment(&mut kept, &mut integer_digits);
    }
    while kept.last() == Some(&0) {
        kept.pop();
    }
    let mut out: String = integer_digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    if !kept.is_empty() {
        out.push('.');
        out.extend(kept.iter().map(|digit| char::from(b'0' + digit)));
    }
    out
}

/// Add one to `kept`, carrying into `integer` (and growing it).
fn increment(kept: &mut [u8], integer: &mut Vec<u8>) {
    let mut carry = true;
    for digit in kept.iter_mut().rev() {
        if !carry {
            break;
        }
        *digit += 1;
        carry = *digit == 10;
        if carry {
            *digit = 0;
        }
    }
    if !carry {
        return;
    }
    for digit in integer.iter_mut().rev() {
        *digit += 1;
        carry = *digit == 10;
        if carry {
            *digit = 0;
        } else {
            return;
        }
    }
    if carry {
        integer.insert(0, 1);
    }
}

/// Group the integer part of `value` with `,` like `Intl` in compact notation:
/// four-digit integers stay ungrouped (`1000T`), five and more are grouped
/// (`10,000T`, `1,000,000,000T`). A fraction part is never touched.
fn group_thousands(value: &str) -> String {
    let (sign, unsigned) = value
        .strip_prefix('-')
        .map_or(("", value), |digits| ("-", digits));
    let (digits, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(digits, fraction)| {
            (digits, Some(fraction))
        });
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if digits.len() >= 5 && index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(fraction) = fraction {
        out.push('.');
        out.push_str(fraction);
    }
    format!("{sign}{out}")
}

/// The exact decimal expansion of a finite non-negative `f64` as
/// `(integer digits, fraction digits)`; `fraction` carries no trailing zeros
/// beyond the value's binary precision.
fn exact_decimal_parts(value: f64) -> (String, String) {
    let bits = value.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction_bits = bits & ((1u64 << 52) - 1);
    let (mantissa, exponent) = if exponent_bits == 0 {
        (fraction_bits, -1074)
    } else {
        (fraction_bits | (1u64 << 52), exponent_bits - 1075)
    };
    if mantissa == 0 {
        return ("0".to_string(), String::new());
    }
    let mut digits = big_decimal(mantissa);
    if exponent >= 0 {
        for _ in 0..exponent {
            big_mul(&mut digits, 2);
        }
        return (big_string(&digits), String::new());
    }
    let places = (-exponent) as usize;
    for _ in 0..places {
        big_mul(&mut digits, 5);
    }
    let text = big_string(&digits);
    if text.len() > places {
        let split = text.len() - places;
        (text[..split].to_string(), text[split..].to_string())
    } else {
        let mut fraction = "0".repeat(places - text.len());
        fraction.push_str(&text);
        ("0".to_string(), fraction)
    }
}

/// A non-negative integer as little-endian decimal digits.
fn big_decimal(mut value: u64) -> Vec<u8> {
    let mut digits = Vec::new();
    if value == 0 {
        return vec![0];
    }
    while value > 0 {
        digits.push((value % 10) as u8);
        value /= 10;
    }
    digits
}

fn big_mul(digits: &mut Vec<u8>, factor: u8) {
    let mut carry = 0u32;
    for digit in digits.iter_mut() {
        let product = u32::from(*digit) * u32::from(factor) + carry;
        *digit = (product % 10) as u8;
        carry = product / 10;
    }
    while carry > 0 {
        digits.push((carry % 10) as u8);
        carry /= 10;
    }
}

fn big_string(digits: &[u8]) -> String {
    digits
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + digit))
        .collect()
}

/// `Number.prototype.toString` for a magnitude at or past `1e21`: JavaScript
/// switches to exponent form there.
fn js_exponential(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let digits = format!("{}", value.abs());
    let trimmed = digits.trim_end_matches('0');
    let exponent = digits.len() - 1;
    let mut out = String::from(sign);
    let mut chars = trimmed.chars();
    if let Some(first) = chars.next() {
        out.push(first);
        let rest: String = chars.collect();
        if !rest.is_empty() {
            out.push('.');
            out.push_str(&rest);
        }
    }
    out.push_str(&format!("e+{exponent}"));
    out
}

/// Parse the SDK's ISO-8601 `resets_at` into epoch milliseconds. A value
/// without a zone is a date-only (UTC) or a local date-time, matching
/// `Date.parse`.
fn parse_instant(value: &str, zone: &Zone) -> Option<i64> {
    // `Date.parse` only accepts ASCII for this format, so a non-ASCII byte
    // makes the value `Invalid Date` upstream. Rejecting it here also keeps
    // every slice below on a character boundary.
    if !value.is_ascii() {
        return None;
    }
    let (date, rest) = value.split_at_checked(10)?;
    if date.as_bytes().get(4) != Some(&b'-') || date.as_bytes().get(7) != Some(&b'-') {
        return None;
    }
    let year = date[..4].parse::<i64>().ok()?;
    let month = date[5..7].parse::<u32>().ok()?;
    let day = date[8..10].parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut offset_minutes = 0i32;
    let mut millis = 0i64;
    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    if !rest.is_empty() {
        let time = rest.strip_prefix('T')?;
        if time.len() < 5 {
            return None;
        }
        hour = time[..2].parse::<u32>().ok()?;
        if time.as_bytes().get(2) != Some(&b':') {
            return None;
        }
        minute = time[3..5].parse::<u32>().ok()?;
        if hour > 23 || minute > 59 {
            return None;
        }
        let mut tail = &time[5..];
        if let Some(stripped) = tail.strip_prefix(':') {
            if stripped.len() < 2 {
                return None;
            }
            second = stripped[..2].parse::<u32>().ok()?;
            if second > 59 {
                return None;
            }
            tail = &stripped[2..];
        }
        if let Some(stripped) = tail.strip_prefix('.') {
            let fraction_len = stripped.bytes().take_while(u8::is_ascii_digit).count();
            if fraction_len == 0 {
                return None;
            }
            let fraction = &stripped[..fraction_len];
            let millis_digits = &fraction[..fraction_len.min(3)];
            let padded = format!("{millis_digits:0<3}");
            millis = padded.parse::<i64>().ok()?;
            tail = &stripped[fraction_len..];
        }
        if tail.is_empty() {
            // A local date-time, like `new Date("2026-09-04T16:59:00")`.
            offset_minutes = zone.offset_minutes;
        } else if tail == "Z" || tail == "z" {
            offset_minutes = 0;
        } else {
            let sign = match tail.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return None,
            };
            let digits = &tail[1..];
            let (hours, minutes) = if let Some((hours, minutes)) = digits.split_once(':') {
                if hours.len() != 2 || minutes.len() != 2 {
                    return None;
                }
                (hours, minutes)
            } else if digits.len() == 4 {
                (&digits[..2], &digits[2..])
            } else if digits.len() == 2 {
                (digits, "00")
            } else {
                return None;
            };
            let hours = hours.parse::<i32>().ok()?;
            let minutes = minutes.parse::<i32>().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            offset_minutes = sign * (hours * 60 + minutes);
        }
    }
    let days = days_from_civil(year, month, day);
    let seconds =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    Some((seconds - i64::from(offset_minutes) * 60) * 1000 + millis)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        _ => 28,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

/// The upstream fixture from `src/tests/acp-agent.test.ts`, with the unused
/// subtrees kept so the parse test proves they survive.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage_fixture() -> Value {
        json!({
            "session": {
                "total_cost_usd": 0.33,
                "total_api_duration_ms": 20_000,
                "total_duration_ms": 69_000,
                "total_lines_added": 0,
                "total_lines_removed": 0,
                "model_usage": {
                    "claude-opus-4-1": {
                        "inputTokens": 4,
                        "outputTokens": 872,
                        "cacheReadInputTokens": 98_700,
                        "cacheCreationInputTokens": 25_300,
                        "webSearchRequests": 0,
                        "costUSD": 0.33,
                        "contextWindow": 200_000,
                        "maxOutputTokens": 32_000
                    }
                }
            },
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {
                "five_hour": { "utilization": 3, "resets_at": "2026-09-04T16:59:00.000Z" },
                "seven_day": { "utilization": 0, "resets_at": "2026-09-04T17:59:00.000Z" },
                "model_scoped": [{ "display_name": "Fable", "utilization": 0, "resets_at": null }]
            },
            "behaviors": {
                "day": {
                    "request_count": 34,
                    "session_count": 3,
                    "behaviors": [],
                    "agents": [],
                    "skills": [],
                    "plugins": [],
                    "mcp_servers": [{ "name": "ccd_session_mgmt", "pct": 13 }]
                },
                "week": {
                    "request_count": 43,
                    "session_count": 5,
                    "behaviors": [],
                    "agents": [],
                    "skills": [],
                    "plugins": [],
                    "mcp_servers": [{ "name": "claude_agent_acp", "pct": 3 }]
                }
            }
        })
    }

    // Golden strings below are the exact output of the compiled upstream
    // `dist/usage-markdown.js` `formatUsageResponse` under Node 22 (TZ as
    // named), not hand-written.
    const BASE_UTC: &str = r"## Usage

> Claude max subscription usage

### Limits

**5-hour limit** — **3%** · Resets Sep 4, 4:59 PM UTC

`█░░░░░░░░░░░░░░░░░░░`

**Weekly · all models** — **0%** · Resets Sep 4, 5:59 PM UTC

`░░░░░░░░░░░░░░░░░░░░`

**Weekly · Fable** — **0%**

`░░░░░░░░░░░░░░░░░░░░`

---

### This session

| Cost | API time | Active |
|:--|:--|:--|
| $0.33 | 20s | 1m 9s |

| Breakdown | Tokens |
|:--|--:|
| Input | 4 |
| Output | 872 |
| Cache read | 98.7K |
| Cache write | 25.3K |

---

### What’s using your limits?

> Approximate, overlapping measures · this machine only · excludes claude.ai

**Last 24h** · 34 requests · 3 sessions

| MCP server | Usage |
|:--|--:|
| ccd\_session\_mgmt | `███░░░░░░░░░░░░░░░░░` 13% |

**Last 7d** · 43 requests · 5 sessions

| MCP server | Usage |
|:--|--:|
| claude\_agent\_acp | `█░░░░░░░░░░░░░░░░░░░` 3% |";

    /// `src/tests/acp-agent.test.ts | usage Markdown > keeps the expected experimental SDK response shape compile- and runtime-checked`
    #[test]
    fn keeps_the_expected_experimental_sdk_response_shape() {
        let fixture = usage_fixture();
        assert_eq!(parse_usage_response(&fixture), Some(fixture.clone()));

        let broken = json!({
            "session": { "total_cost_usd": "broken" },
            "subscription_type": null,
            "rate_limits_available": false,
            "rate_limits": null,
            "behaviors": null,
        });
        assert_eq!(parse_usage_response(&broken), None);

        let mut future = fixture.clone();
        future["session"]["model_usage"]["claude-opus-4-1"]["costBasis"] =
            Value::String("a-future-value".to_string());
        future["behaviors"]["day"]["behaviors"] =
            json!([{ "key": "a-future-behavior", "pct": 1, "count": 1 }]);
        assert_eq!(parse_usage_response(&future), Some(future.clone()));
    }

    /// `src/tests/acp-agent.test.ts | usage Markdown > renders limits, session totals, and MCP contributions from structured data`
    #[test]
    fn renders_limits_session_totals_and_mcp_contributions() {
        let formatted = format_usage_response(&usage_fixture(), &Zone::utc());
        assert_eq!(formatted, BASE_UTC);
        assert!(formatted.contains("## Usage"));
        assert!(formatted.contains("**5-hour limit** — **3%**"));
        assert!(formatted.contains("**Weekly · Fable** — **0%**"));
        assert!(formatted.contains("| $0.33 | 20s | 1m 9s |"));
        assert!(formatted.contains("| Cache read | 98.7K |"));
        assert!(formatted.contains("**Last 24h** · 34 requests · 3 sessions"));
        assert!(formatted.contains("| ccd\\_session\\_mgmt | `███░░░░░░░░░░░░░░░░░` 13% |"));
    }

    /// `src/tests/acp-agent.test.ts | usage Markdown > escapes and limits MCP contribution rows`
    #[test]
    fn escapes_and_limits_mcp_contribution_rows() {
        let mut fixture = usage_fixture();
        fixture["behaviors"]["day"]["mcp_servers"] = json!([
            { "name": "lowest", "pct": 1 },
            { "name": "pipe|name", "pct": 40 },
            { "name": "second", "pct": 30 },
            { "name": "third", "pct": 20 }
        ]);
        let formatted = format_usage_response(&fixture, &Zone::utc());
        assert!(formatted.contains("pipe\\|name"));
        assert!(formatted.contains("second"));
        assert!(formatted.contains("third"));
        assert!(!formatted.contains("lowest"));
        // Exact Node golden for the same input.
        assert_eq!(
            formatted,
            BASE_UTC.replace(
                "| ccd\\_session\\_mgmt | `███░░░░░░░░░░░░░░░░░` 13% |",
                "| pipe\\|name | `████████░░░░░░░░░░░░` 40% |\n| second | `██████░░░░░░░░░░░░░░` 30% |\n| third | `████░░░░░░░░░░░░░░░░` 20% |"
            )
        );
    }

    /// `src/tests/acp-agent.test.ts | usage Markdown > recognizes only exact local usage commands`
    #[test]
    fn recognizes_only_exact_local_usage_commands() {
        assert!(is_usage_command_text(" /usage "));
        assert!(!is_usage_command_text("/cost"));
        assert!(!is_usage_command_text("/stats"));
        assert!(!is_usage_command_text("please review /usage"));
        assert!(!is_usage_command_text("/usage now"));
    }

    /// `src/tests/acp-agent.test.ts | usage Markdown > omits the limits section when every utilization is unavailable`
    #[test]
    fn omits_the_limits_section_when_every_utilization_is_unavailable() {
        let mut fixture = usage_fixture();
        fixture["rate_limits"] = json!({
            "five_hour": { "utilization": null, "resets_at": null },
            "seven_day": { "utilization": null, "resets_at": null },
            "seven_day_opus": { "utilization": null, "resets_at": null },
            "seven_day_sonnet": { "utilization": null, "resets_at": null }
        });
        let formatted = format_usage_response(&fixture, &Zone::utc());
        assert!(!formatted.contains("### Limits"));
        assert!(formatted.contains("### This session"));
        assert_eq!(
            formatted,
            r"## Usage

> Claude max subscription usage

---

### This session

| Cost | API time | Active |
|:--|:--|:--|
| $0.33 | 20s | 1m 9s |

| Breakdown | Tokens |
|:--|--:|
| Input | 4 |
| Output | 872 |
| Cache read | 98.7K |
| Cache write | 25.3K |

---

### What’s using your limits?

> Approximate, overlapping measures · this machine only · excludes claude.ai

**Last 24h** · 34 requests · 3 sessions

| MCP server | Usage |
|:--|--:|
| ccd\_session\_mgmt | `███░░░░░░░░░░░░░░░░░` 13% |

**Last 7d** · 43 requests · 5 sessions

| MCP server | Usage |
|:--|--:|
| claude\_agent\_acp | `█░░░░░░░░░░░░░░░░░░░` 3% |"
        );
    }

    /// Without `model_scoped` the renderer falls back to the Opus/Sonnet
    /// windows; a null window and an unparseable reset are skipped/escaped.
    #[test]
    fn falls_back_to_opus_and_sonnet_windows() {
        let mut fixture = usage_fixture();
        fixture["rate_limits"] = json!({
            "five_hour": { "utilization": 42.5, "resets_at": null },
            "seven_day": null,
            "seven_day_opus": { "utilization": 100, "resets_at": "not-a-date" },
            "seven_day_sonnet": { "utilization": 0.4, "resets_at": "2026-09-04T17:59:00.000Z" }
        });
        let formatted = format_usage_response(&fixture, &Zone::utc());
        assert!(formatted.contains("**5-hour limit** — **42.5%**\n\n`█████████░░░░░░░░░░░`"));
        assert!(!formatted.contains("Weekly · all models"));
        assert!(formatted.contains("**Weekly · Opus** — **100%** · Resets not-a-date"));
        assert!(formatted.contains("**Weekly · Sonnet** — **0.4%** · Resets Sep 4, 5:59 PM UTC"));
        assert!(formatted.contains("`████████████████████`"));
        assert!(formatted.contains("`█░░░░░░░░░░░░░░░░░░░`"));
    }

    /// Reset times render in the zone passed in, like `Intl` under the
    /// process's `TZ`.
    #[test]
    fn renders_reset_times_in_the_passed_zone() {
        let mut fixture = usage_fixture();
        fixture["behaviors"] = Value::Null;
        let los_angeles = Zone::fixed(-420, "PDT");
        let berlin = Zone::fixed(120, "GMT+2");
        let pacific = format_usage_response(&fixture, &los_angeles);
        assert!(pacific.contains("**5-hour limit** — **3%** · Resets Sep 4, 9:59 AM PDT"));
        assert!(pacific.contains("**Weekly · all models** — **0%** · Resets Sep 4, 10:59 AM PDT"));
        assert!(!pacific.contains("What’s using your limits?"));
        let central_europe = format_usage_response(&fixture, &berlin);
        assert!(central_europe.contains("**5-hour limit** — **3%** · Resets Sep 4, 6:59 PM GMT+2"));
        assert!(central_europe
            .contains("**Weekly · all models** — **0%** · Resets Sep 4, 7:59 PM GMT+2"));
    }

    /// Zero and large values, matching Node byte for byte.
    #[test]
    fn renders_zero_and_huge_numbers() {
        let zero = json!({
            "session": {
                "total_cost_usd": 0,
                "total_api_duration_ms": 0,
                "total_duration_ms": 59_999,
                "model_usage": {}
            },
            "subscription_type": null,
            "rate_limits_available": false,
            "rate_limits": null,
            "behaviors": null
        });
        assert_eq!(
            format_usage_response(&zero, &Zone::utc()),
            r"## Usage

---

### This session

| Cost | API time | Active |
|:--|:--|:--|
| $0.00 | 0s | 1m |

| Breakdown | Tokens |
|:--|--:|
| Input | 0 |
| Output | 0 |
| Cache read | 0 |
| Cache write | 0 |"
        );

        let huge = json!({
            "session": {
                "total_cost_usd": 1_234_567.891,
                "total_api_duration_ms": 3_661_000,
                "total_duration_ms": 999_999_999,
                "model_usage": {
                    "a": {
                        "inputTokens": 1_234_567,
                        "outputTokens": 999_999_999,
                        "cacheReadInputTokens": 1_000,
                        "cacheCreationInputTokens": 1_499
                    },
                    "b": {
                        "inputTokens": 0,
                        "outputTokens": 1,
                        "cacheReadInputTokens": 0,
                        "cacheCreationInputTokens": 0
                    }
                }
            },
            "subscription_type": "max",
            "rate_limits_available": false,
            "rate_limits": null,
            "behaviors": null
        });
        let formatted = format_usage_response(&huge, &Zone::utc());
        assert!(formatted.contains("| $1234567.89 | 61m 1s | 16666m 40s |"));
        assert!(formatted.contains("| Input | 1.2M |"));
        assert!(formatted.contains("| Output | 1B |"));
        assert!(formatted.contains("| Cache read | 1K |"));
        assert!(formatted.contains("| Cache write | 1.5K |"));
        assert!(formatted.contains("> Claude max subscription usage"));
        assert!(!formatted.contains("### Limits"));
    }

    /// Bars clamp, `0` is empty, tiny non-zero values fill one cell, `NaN`
    /// fills none (Node: `"█".repeat(NaN)` is `""`).
    #[test]
    fn usage_bars_match_node() {
        let cases = [
            (0.0, "░░░░░░░░░░░░░░░░░░░░"),
            (0.4, "█░░░░░░░░░░░░░░░░░░░"),
            (2.5, "█░░░░░░░░░░░░░░░░░░░"),
            (2.6, "█░░░░░░░░░░░░░░░░░░░"),
            (7.5, "██░░░░░░░░░░░░░░░░░░"),
            (12.5, "███░░░░░░░░░░░░░░░░░"),
            (13.0, "███░░░░░░░░░░░░░░░░░"),
            (97.5, "████████████████████"),
            (99.9, "████████████████████"),
            (100.0, "████████████████████"),
            (-5.0, "░░░░░░░░░░░░░░░░░░░░"),
            (f64::INFINITY, "████████████████████"),
            (f64::NEG_INFINITY, "░░░░░░░░░░░░░░░░░░░░"),
        ];
        for (percent, expected) in cases {
            assert_eq!(usage_bar(percent), expected, "bar for {percent}");
        }
        assert_eq!(usage_bar(f64::NAN), "");
    }

    /// `formatDuration` goldens from the Node primitive.
    #[test]
    fn durations_match_node() {
        let cases = [
            (0.0, "0s"),
            (999.0, "1s"),
            (1_500.0, "2s"),
            (20_000.0, "20s"),
            (59_499.0, "59s"),
            (59_999.0, "1m"),
            (60_000.0, "1m"),
            (69_000.0, "1m 9s"),
            (3_661_000.0, "61m 1s"),
            (999_999_999.0, "16666m 40s"),
            (-1_500.0, "0s"),
        ];
        for (millis, expected) in cases {
            assert_eq!(format_duration(millis), expected, "duration for {millis}");
        }
    }

    /// `formatCount` goldens from `Intl.NumberFormat("en", { notation:
    /// "compact", maximumFractionDigits: 1 })`.
    #[test]
    fn counts_match_node() {
        let cases = [
            (0.0, "0"),
            (1.0, "1"),
            (4.0, "4"),
            (872.0, "872"),
            (999.0, "999"),
            (999.4, "999.4"),
            (999.5, "999.5"),
            (999.95, "1K"),
            (1_000.0, "1K"),
            (1_000.4, "1K"),
            (1_049.0, "1K"),
            (1_049.9, "1K"),
            (1_050.0, "1.1K"),
            (1_499.0, "1.5K"),
            (9_949.0, "9.9K"),
            (9_950.0, "10K"),
            (9_999.0, "10K"),
            (12_345.0, "12.3K"),
            (99_949.0, "99.9K"),
            (99_950.0, "100K"),
            (98_700.0, "98.7K"),
            (999_949.0, "999.9K"),
            (999_950.0, "1M"),
            (1_000_000.0, "1M"),
            (1_234_567.0, "1.2M"),
            (12_345_678.0, "12.3M"),
            (999_999_999.0, "1B"),
            (1_234_567_890.0, "1.2B"),
            (1_000_000_000_000.0, "1T"),
            (999_999_999_999.0, "1T"),
            (1_000_000_000_000_000.0, "1000T"),
            (1.234_567_890_123e15, "1234.6T"),
            (1e21, "1,000,000,000T"),
            (1.234e21, "1,234,000,000T"),
            (-5.0, "-5"),
            (-0.4, "-0.4"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_count(value), expected, "count for {value}");
        }
        assert_eq!(format_count(-0.0), "-0");
        assert_eq!(format_count(f64::NAN), "NaN");
        assert_eq!(format_count(f64::INFINITY), "∞");
        assert_eq!(format_count(f64::NEG_INFINITY), "-∞");
    }

    /// `toFixed(2)` goldens, including the binary-value quirks (`1.005` →
    /// `1.00`, `0.125` → `0.13`).
    #[test]
    fn costs_match_to_fixed() {
        let cases = [
            (0.0, "0.00"),
            (0.33, "0.33"),
            (0.005, "0.01"),
            (1.005, "1.00"),
            (0.125, "0.13"),
            (2.625, "2.63"),
            (1.2345, "1.23"),
            (12.999, "13.00"),
            (1_234_567.891, "1234567.89"),
            (0.1 + 0.2, "0.30"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_cost(value), expected, "cost for {value}");
        }
        assert_eq!(format_cost(-0.0), "0.00");
        assert_eq!(format_cost(f64::NAN), "NaN");
        assert_eq!(format_cost(f64::INFINITY), "Infinity");
        assert_eq!(format_cost(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn escapes_markdown_syntax_and_folds_line_breaks() {
        assert_eq!(escape_markdown("plain"), "plain");
        assert_eq!(
            escape_markdown("a|b*c_d`e[f]g<h>i\\j"),
            "a\\|b\\*c\\_d\\`e\\[f\\]g\\<h\\>i\\\\j"
        );
        assert_eq!(escape_markdown("line\nbreak"), "line break");
        assert_eq!(escape_markdown("two\r\n\nbreaks"), "two breaks");
        assert_eq!(
            escape_markdown("- # ! ( ) not escaped"),
            "- # ! ( ) not escaped"
        );
    }

    #[test]
    fn reset_times_are_empty_raw_or_formatted() {
        let utc = Zone::utc();
        assert_eq!(format_reset(None, &utc), "");
        assert_eq!(format_reset(Some(""), &utc), "");
        assert_eq!(
            format_reset(Some("not-a-date"), &utc),
            " · Resets not-a-date"
        );
        assert_eq!(format_reset(Some("a|b"), &utc), " · Resets a\\|b");
        assert_eq!(
            format_reset(Some("2026-09-04T16:59:00.000Z"), &utc),
            " · Resets Sep 4, 4:59 PM UTC"
        );
        assert_eq!(
            format_reset(Some("2026-01-01T00:00:00Z"), &Zone::fixed(-480, "PST")),
            " · Resets Dec 31, 4:00 PM PST"
        );
        assert_eq!(
            format_reset(
                Some("2026-06-15T12:00:00+02:00"),
                &Zone::fixed(330, "GMT+5:30")
            ),
            " · Resets Jun 15, 3:30 PM GMT+5:30"
        );
        assert_eq!(
            format_reset(Some("2026-10-26"), &Zone::fixed(0, "UTC")),
            " · Resets Oct 26, 12:00 AM UTC"
        );
        assert_eq!(
            format_reset(Some("2026-10-26T15:04:00"), &Zone::fixed(-420, "PDT")),
            " · Resets Oct 26, 3:04 PM PDT"
        );
    }

    /// `new Date(...)` on these inputs is `Invalid Date`, so upstream prints
    /// the raw string. The Rust path must agree instead of panicking on a
    /// slice that splits a multi-byte character.
    #[test]
    fn reset_times_reject_non_ascii_without_panicking() {
        let utc = Zone::utc();
        let cases = [
            "2026-09-04T1é:00",
            "2026-09-04T16:59:00é",
            "2026-09-04T16:59:00Zé",
            "2026-09-04T16:59:00+01é00",
            "2026-09-04T16:59:00.5é",
            "2026-09-04Té:59:00",
            "2026-09-04T16:59:0é",
            "2026-09-04T16:59:00.５",
            "2026-09-04T16:59:00+０１:00",
            "2026-09-04T16:59:00Z\u{a0}",
            "\u{a0}2026-09-04T16:59:00Z",
            "2026-09-04T16:59:00Z\u{2003}",
            "2026-09-04T16:59:00Z\u{200b}",
            "2026-09-04é",
            "2026-0é-04",
            "202é-09-04",
            "é",
        ];
        for value in cases {
            assert_eq!(parse_instant(value, &utc), None, "instant for {value:?}");
            assert_eq!(
                format_reset(Some(value), &utc),
                format!(" · Resets {}", escape_markdown(value)),
                "reset for {value:?}"
            );
        }

        // A multi-byte character at any byte position is `Invalid Date`
        // upstream, and must never split a slice here.
        let base = "2026-09-04T16:59:00.123+05:30";
        for index in 0..=base.len() {
            let mut value = String::with_capacity(base.len() + 2);
            value.push_str(&base[..index]);
            value.push('é');
            value.push_str(&base[index..]);
            assert_eq!(parse_instant(&value, &utc), None, "instant for {value:?}");
            assert_eq!(
                format_reset(Some(&value), &utc),
                format!(" · Resets {}", escape_markdown(&value)),
                "reset for {value:?}"
            );
        }
    }

    /// Every prefix of a valid instant is either a date or a malformed
    /// time; none may panic, whatever the truncation.
    #[test]
    fn parse_instant_stays_total_on_truncated_inputs() {
        let utc = Zone::utc();
        let base = "2026-09-04T16:59:00.123+05:30";
        for index in 0..=base.len() {
            let value = &base[..index];
            let parsed = parse_instant(value, &utc);
            if index < 10 {
                assert_eq!(parsed, None, "instant for {value:?}");
            }
            let _ = format_reset(Some(value), &utc);
        }
    }

    /// The numeric formatters are `pub` and take any `f64`; `NaN`-like,
    /// huge and subnormal magnitudes must not panic or hang.
    #[test]
    fn formatters_stay_total_on_adversarial_numbers() {
        let values = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            1e21,
            1.5e21,
            1e308,
            -1e308,
            999_950.0,
            -999_950.0,
        ];
        for value in values {
            let _ = format_count(value);
            let _ = format_cost(value);
            let _ = format_duration(value);
            let _ = usage_bar(value);
        }
        assert_eq!(format_count(f64::NAN), "NaN");
        assert_eq!(format_cost(f64::NAN), "NaN");
        assert_eq!(usage_bar(f64::NAN), "");
        assert_eq!(format_count(f64::INFINITY), "∞");
        assert_eq!(format_cost(f64::INFINITY), "Infinity");
        assert_eq!(usage_bar(f64::INFINITY).len(), 20 * 3);
    }

    /// Node golden: upstream `formatUsageResponse` prints the raw
    /// `resets_at` when `new Date` rejects it, even when the string is not
    /// ASCII. Captured from `dist/usage-markdown.js` under `TZ=UTC`.
    #[test]
    fn renders_a_non_ascii_reset_as_the_raw_value() {
        let mut fixture = usage_fixture();
        fixture["rate_limits"]["five_hour"]["resets_at"] = json!("2026-09-04T1é:00");
        let expected = BASE_UTC.replace(
            "**5-hour limit** — **3%** · Resets Sep 4, 4:59 PM UTC",
            "**5-hour limit** — **3%** · Resets 2026-09-04T1é:00",
        );
        assert_eq!(format_usage_response(&fixture, &Zone::utc()), expected);
    }

    #[test]
    fn parse_rejects_malformed_responses() {
        let fixture = usage_fixture();
        assert_eq!(parse_usage_response(&Value::Null), None);
        assert_eq!(parse_usage_response(&json!({})), None);

        let mut negative = fixture.clone();
        negative["session"]["total_cost_usd"] = json!(-1);
        assert_eq!(parse_usage_response(&negative), None);

        let mut over = fixture.clone();
        over["rate_limits"]["five_hour"]["utilization"] = json!(101);
        assert_eq!(parse_usage_response(&over), None);

        let mut missing = fixture.clone();
        missing["rate_limits"] = Value::Null;
        assert_eq!(parse_usage_response(&missing), Some(missing.clone()));
        let mut missing_key = fixture.clone();
        missing_key.as_object_mut().unwrap().remove("rate_limits");
        assert_eq!(parse_usage_response(&missing_key), None);

        let mut missing_behaviors = fixture.clone();
        missing_behaviors
            .as_object_mut()
            .unwrap()
            .remove("behaviors");
        assert_eq!(parse_usage_response(&missing_behaviors), None);

        let mut null_window = fixture.clone();
        null_window["rate_limits"]["five_hour"] = Value::Null;
        assert_eq!(
            parse_usage_response(&null_window),
            Some(null_window.clone())
        );

        let mut bad_model = fixture.clone();
        bad_model["session"]["model_usage"] = json!({ "a": { "inputTokens": 1 } });
        assert_eq!(parse_usage_response(&bad_model), None);

        let mut bad_scoped = fixture.clone();
        bad_scoped["rate_limits"]["model_scoped"] = json!([{ "utilization": 0 }]);
        assert_eq!(parse_usage_response(&bad_scoped), None);
    }

    #[test]
    fn zone_helpers() {
        assert_eq!(Zone::utc().offset_minutes, 0);
        assert_eq!(Zone::utc().name, "UTC");
        assert_eq!(Zone::fixed(120, "GMT+2").name, "GMT+2");
    }
}
