//! Scheduled jobs: typed specs, cron → systemd `OnCalendar=` conversion,
//! and the validation shared by panel-api and panel-agent.
//!
//! A job runs one exec-form command inside a service of a Compose project
//! (`docker compose exec -T <service> <argv...>`) from a generated systemd
//! timer. Nothing here ever reaches a shell: the command is an argv vector,
//! and every value written into a unit file is checked against a narrow
//! character set first, so it cannot add a line or a systemd specifier.

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Most jobs one Compose project may have.
pub const MAX_JOBS_PER_PROJECT: usize = 32;
/// Longest a single run may take before its process group is stopped.
pub const MAX_JOB_TIMEOUT_SECS: u32 = 24 * 60 * 60;
pub const DEFAULT_JOB_TIMEOUT_SECS: u32 = 60 * 60;
/// Output kept per run on the agent; further output is discarded.
pub const JOB_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Runs kept per job, on the agent (logs) and in PostgreSQL (records).
pub const JOB_RUNS_RETAINED: usize = 50;

const MAX_COMMAND_ARGS: usize = 64;
const MAX_ARG_LEN: usize = 4096;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_EXPRESSION_LEN: usize = 128;

/// When a job runs: a five-field cron expression (or `@daily`-style
/// macro), converted to `OnCalendar=`, or a systemd calendar expression
/// used as is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "expression", rename_all = "snake_case")]
pub enum JobSchedule {
    Cron(String),
    Calendar(String),
}

impl JobSchedule {
    pub fn kind(&self) -> &'static str {
        match self {
            JobSchedule::Cron(_) => "cron",
            JobSchedule::Calendar(_) => "calendar",
        }
    }

    pub fn expression(&self) -> &str {
        match self {
            JobSchedule::Cron(e) | JobSchedule::Calendar(e) => e,
        }
    }

    pub fn from_parts(kind: &str, expression: String) -> Option<Self> {
        match kind {
            "cron" => Some(JobSchedule::Cron(expression)),
            "calendar" => Some(JobSchedule::Calendar(expression)),
            _ => None,
        }
    }

    /// The `OnCalendar=` values for this schedule in `timezone`. A cron
    /// expression restricting both day of month and day of week needs two
    /// (cron runs when either matches; systemd requires both).
    pub fn on_calendar(&self, timezone: &str) -> Result<Vec<String>, String> {
        if !is_valid_timezone(timezone) {
            return Err("invalid time zone (use an IANA name such as Europe/Istanbul)".into());
        }
        let expressions = match self {
            JobSchedule::Cron(expression) => cron_to_on_calendar(expression)?,
            JobSchedule::Calendar(expression) => {
                let expression = expression.trim();
                if !is_valid_calendar_expression(expression) {
                    return Err("invalid calendar expression: use letters, digits, spaces and * , . : / ~ + - only (at most 128 characters)".into());
                }
                vec![expression.to_string()]
            }
        };
        Ok(expressions
            .into_iter()
            .map(|e| format!("{e} {timezone}"))
            .collect())
    }
}

/// What the agent needs to install a job: a typed command and a schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduledJobSpec {
    /// Compose service whose running container executes the command.
    pub service_name: String,
    /// Exec-form argv, e.g. `["php", "artisan", "schedule:run"]`.
    pub command: Vec<String>,
    pub schedule: JobSchedule,
    /// IANA time zone the schedule is read in.
    pub timezone: String,
    pub timeout_secs: u32,
}

impl ScheduledJobSpec {
    /// Validates the spec and returns its `OnCalendar=` values.
    pub fn validate(&self) -> Result<Vec<String>, String> {
        if !crate::types::validate::is_valid_service_name(&self.service_name) {
            return Err("invalid service name".into());
        }
        validate_job_command(&self.command)?;
        if self.timeout_secs == 0 || self.timeout_secs > MAX_JOB_TIMEOUT_SECS {
            return Err(format!(
                "timeout must be between 1 and {MAX_JOB_TIMEOUT_SECS} seconds"
            ));
        }
        self.schedule.on_calendar(&self.timezone)
    }
}

/// Programs that interpret a command string given with `-c`.
const SHELLS: &[&str] = &[
    "sh", "bash", "dash", "ash", "zsh", "ksh", "mksh", "csh", "tcsh", "fish", "busybox",
];

static OPTION_CLUSTER_WITH_C: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:-[A-Za-z]*c[A-Za-z]*|--command(?:=.*)?)$").unwrap());

/// An exec-form job command: 1..=64 arguments without NUL or newlines, at
/// most 16 KiB in total, not starting with an option, and never a shell
/// handed a command string (`sh -c '...'`, also behind `env`, `busybox`, or
/// another wrapper).
pub fn validate_job_command(command: &[String]) -> Result<(), String> {
    if command.is_empty() || command.len() > MAX_COMMAND_ARGS {
        return Err(format!("a command needs 1..={MAX_COMMAND_ARGS} arguments"));
    }
    if command
        .iter()
        .any(|arg| arg.len() > MAX_ARG_LEN || arg.contains(['\0', '\n', '\r']))
    {
        return Err(format!(
            "arguments must be single-line and at most {MAX_ARG_LEN} bytes"
        ));
    }
    if command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES {
        return Err("the command is longer than 16 KiB".into());
    }
    let program = &command[0];
    if program.trim().is_empty() || program.starts_with('-') {
        return Err("the command must start with a program name".into());
    }
    let shell_at = command.iter().position(|arg| {
        let name = arg.rsplit('/').next().unwrap_or(arg);
        SHELLS.contains(&name)
    });
    if let Some(at) = shell_at {
        if command[at + 1..]
            .iter()
            .any(|arg| OPTION_CLUSTER_WITH_C.is_match(arg))
        {
            return Err("shell command strings (sh -c) are not allowed; give the program and its arguments directly, or run a script file".into());
        }
    }
    Ok(())
}

static TIMEZONE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z][A-Za-z0-9_+-]{0,31}(?:/[A-Za-z0-9][A-Za-z0-9_+-]{0,31}){0,2}$").unwrap()
});

/// An IANA time zone name such as `UTC`, `Europe/Istanbul`, or
/// `America/Argentina/Buenos_Aires`. The agent also checks that the zone
/// exists in its tz database.
pub fn is_valid_timezone(timezone: &str) -> bool {
    TIMEZONE_RE.is_match(timezone)
}

static CALENDAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9*][A-Za-z0-9 *,.:/~+-]*$").unwrap());

/// A systemd calendar expression limited to the characters its grammar
/// uses. No `%` (specifiers), `\`, quotes, `=`, or newlines, so it cannot
/// change a unit file beyond its own `OnCalendar=` value; `systemd-analyze
/// calendar` on the agent checks the grammar itself.
pub fn is_valid_calendar_expression(expression: &str) -> bool {
    expression.len() <= MAX_EXPRESSION_LEN
        && CALENDAR_RE.is_match(expression)
        && !expression.contains("  ")
}

/// One cron field: its bounds and accepted names.
struct Field {
    label: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
}

const MINUTE: Field = Field {
    label: "minute",
    min: 0,
    max: 59,
    names: &[],
};
const HOUR: Field = Field {
    label: "hour",
    min: 0,
    max: 23,
    names: &[],
};
const DAY_OF_MONTH: Field = Field {
    label: "day of month",
    min: 1,
    max: 31,
    names: &[],
};
const MONTH: Field = Field {
    label: "month",
    min: 1,
    max: 12,
    names: &[
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ],
};
/// 0 and 7 are both Sunday.
const DAY_OF_WEEK: Field = Field {
    label: "day of week",
    min: 0,
    max: 7,
    names: &["sun", "mon", "tue", "wed", "thu", "fri", "sat"],
};

/// Values a field matches, as a bit set (bit n = value n).
type Values = u64;

fn parse_value(field: &Field, raw: &str) -> Result<u32, String> {
    let invalid = || format!("invalid {} value {raw:?}", field.label);
    let value = match raw.parse::<u32>() {
        Ok(n) => n,
        Err(_) => {
            let lower = raw.to_ascii_lowercase();
            let index = field
                .names
                .iter()
                .position(|n| *n == lower)
                .ok_or_else(invalid)?;
            // Month names start at 1, weekday names at 0 (Sunday).
            index as u32 + if field.min == 1 { 1 } else { 0 }
        }
    };
    if value < field.min || value > field.max {
        return Err(format!(
            "{} must be between {} and {}",
            field.label, field.min, field.max
        ));
    }
    Ok(value)
}

/// Parses one field (`*`, `5`, `1-5`, `*/15`, `10-50/10`, `5/15`, lists).
fn parse_field(field: &Field, raw: &str) -> Result<Values, String> {
    let mut values: Values = 0;
    for part in raw.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|s| *s >= 1 && *s <= field.max.max(1))
                    .ok_or_else(|| format!("invalid {} step in {part:?}", field.label))?;
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (field.min, field.max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (parse_value(field, a)?, parse_value(field, b)?);
            if a > b {
                return Err(format!("{} range {range:?} runs backwards", field.label));
            }
            (a, b)
        } else {
            let a = parse_value(field, range)?;
            // `a/n` means a, a+n, ... up to the field maximum.
            (
                a,
                if step > 1 || part.contains('/') {
                    field.max
                } else {
                    a
                },
            )
        };
        let mut value = start;
        while value <= end {
            values |= 1 << value;
            value += step;
        }
    }
    Ok(values)
}

fn all_values(min: u32, max: u32) -> Values {
    (min..=max).fold(0, |acc, v| acc | (1 << v))
}

/// Renders a numeric field compactly: `*`, `a/step`, or a list of values
/// and `a..b` ranges. Two digits, as systemd prints them.
fn render_numeric(values: Values, min: u32, max: u32) -> String {
    if values == all_values(min, max) {
        return "*".into();
    }
    let set: Vec<u32> = (min..=max).filter(|v| values & (1 << v) != 0).collect();
    if set.len() >= 3 {
        let step = set[1] - set[0];
        let progression = step > 1
            && set.windows(2).all(|w| w[1] - w[0] == step)
            && set[set.len() - 1] + step > max;
        if progression {
            return format!("{:02}/{step}", set[0]);
        }
    }
    render_runs(&set, |v| format!("{v:02}"))
}

/// `1,2,3,5` → `01..03,05`: runs of three or more become ranges.
fn render_runs(set: &[u32], label: impl Fn(u32) -> String) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < set.len() {
        let mut j = i;
        while j + 1 < set.len() && set[j + 1] == set[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}..{}", label(set[i]), label(set[j])));
        } else {
            parts.extend(set[i..=j].iter().map(|v| label(*v)));
        }
        i = j + 1;
    }
    parts.join(",")
}

/// Weekdays in systemd's order (Monday first), `None` when every day.
fn render_weekdays(values: Values) -> Option<String> {
    const NAMES: [&str; 8] = ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    // Fold cron's Sunday (0 or 7) into 7, after Saturday.
    let days = (values & 0x7e) | if values & 0x81 != 0 { 1 << 7 } else { 0 };
    if days == 0xfe {
        return None;
    }
    let set: Vec<u32> = (1..=7).filter(|d| days & (1 << d) != 0).collect();
    Some(render_runs(&set, |d| NAMES[d as usize].to_string()))
}

/// Converts a five-field cron expression (minute, hour, day of month,
/// month, day of week; names, ranges, steps, and lists allowed) or a macro
/// (`@hourly`, `@daily`, `@midnight`, `@weekly`, `@monthly`, `@yearly`,
/// `@annually`) to `OnCalendar=` expressions without a time zone.
///
/// Like Vixie cron, a schedule that restricts both day fields (neither
/// starts with `*`) runs when either matches, so it becomes two
/// expressions; otherwise both restrictions apply together.
pub fn cron_to_on_calendar(expression: &str) -> Result<Vec<String>, String> {
    let expression = expression.trim();
    let expanded = match expression.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 1 1 *",
        "@monthly" => "0 0 1 * *",
        "@weekly" => "0 0 * * 0",
        "@daily" | "@midnight" => "0 0 * * *",
        "@hourly" => "0 * * * *",
        other if other.starts_with('@') => {
            return Err(format!("unsupported cron macro {expression:?}"));
        }
        _ => expression,
    };
    if expanded.len() > MAX_EXPRESSION_LEN
        || !expanded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " */,-".contains(c))
    {
        return Err("invalid cron expression".into());
    }
    let fields: Vec<&str> = expanded.split_whitespace().collect();
    let [minute, hour, dom, month, dow] = fields[..] else {
        return Err(
            "a cron expression has five fields: minute hour day-of-month month day-of-week".into(),
        );
    };
    let minutes = render_numeric(parse_field(&MINUTE, minute)?, 0, 59);
    let hours = render_numeric(parse_field(&HOUR, hour)?, 0, 23);
    let days = parse_field(&DAY_OF_MONTH, dom)?;
    let months = render_numeric(parse_field(&MONTH, month)?, 1, 12);
    let weekdays = parse_field(&DAY_OF_WEEK, dow)?;

    let line = |weekdays: Option<String>, days: Values| {
        let date = format!("*-{months}-{}", render_numeric(days, 1, 31));
        let time = format!("{hours}:{minutes}:00");
        match weekdays {
            Some(w) => format!("{w} {date} {time}"),
            None => format!("{date} {time}"),
        }
    };
    let (dom_star, dow_star) = (dom.starts_with('*'), dow.starts_with('*'));
    let weekdays = render_weekdays(weekdays);
    if !dom_star && !dow_star && weekdays.is_some() {
        let every_day = all_values(1, 31);
        let mut both = vec![line(None, days), line(weekdays, every_day)];
        both.dedup();
        return Ok(both);
    }
    Ok(vec![line(weekdays, days)])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobRunStatus {
    Running,
    Succeeded,
    Failed,
}

impl JobRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobRunStatus::Running => "running",
            JobRunStatus::Succeeded => "succeeded",
            JobRunStatus::Failed => "failed",
        }
    }
}

impl fmt::Display for JobRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for JobRunStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown job run status: {other}")),
        }
    }
}

/// How a run started: its timer, or "run now" from the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobRunTrigger {
    Schedule,
    Manual,
}

impl JobRunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            JobRunTrigger::Schedule => "schedule",
            JobRunTrigger::Manual => "manual",
        }
    }
}

impl std::str::FromStr for JobRunTrigger {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "schedule" => Ok(Self::Schedule),
            "manual" => Ok(Self::Manual),
            other => Err(format!("unknown job run trigger: {other}")),
        }
    }
}

/// One run of a job, as the agent records it next to the run's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobRunInfo {
    pub id: Uuid,
    pub job_id: Uuid,
    pub trigger: JobRunTrigger,
    pub status: JobRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// The command's exit code; `None` while running or when it never ran
    /// to completion (timeout, missing container).
    pub exit_code: Option<i32>,
    /// Why a run failed, without command output (that is in the log).
    pub error_message: Option<String>,
    /// Bytes of (masked) output in the log.
    pub log_bytes: u64,
    /// Output went past [`JOB_LOG_MAX_BYTES`] and the rest was dropped.
    pub log_truncated: bool,
    /// Last time the agent wrote this record; clients poll with it.
    pub updated_at: DateTime<Utc>,
}

/// A job definition as panel-api stores and returns it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduledJob {
    pub id: Uuid,
    pub compose_project_id: Uuid,
    pub name: String,
    pub service_name: String,
    pub command: Vec<String>,
    pub schedule: JobSchedule,
    pub timezone: String,
    /// The generated `OnCalendar=` values (time zone included).
    pub on_calendar: Vec<String>,
    pub timeout_secs: u32,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_status: Option<JobRunStatus>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cron(expression: &str) -> Vec<String> {
        cron_to_on_calendar(expression).unwrap_or_else(|e| panic!("{expression}: {e}"))
    }

    #[test]
    fn converts_common_cron_expressions() {
        assert_eq!(cron("*/15 * * * *"), ["*-*-* *:00/15:00"]);
        assert_eq!(cron("0 3 * * *"), ["*-*-* 03:00:00"]);
        assert_eq!(cron("30 2 * * 1-5"), ["Mon..Fri *-*-* 02:30:00"]);
        assert_eq!(cron("0 0 1 * *"), ["*-*-01 00:00:00"]);
        assert_eq!(cron("0 */6 * * *"), ["*-*-* 00/6:00:00"]);
        assert_eq!(cron("5,35 9-17 * * *"), ["*-*-* 09..17:05,35:00"]);
        assert_eq!(cron("0 12 * jan,jul sun"), ["Sun *-01,07-* 12:00:00"]);
        assert_eq!(cron("* * * * *"), ["*-*-* *:*:00"]);
        assert_eq!(cron("10-50/20 * * * *"), ["*-*-* *:10/20:00"]);
        assert_eq!(cron("10-40/10 * * * *"), ["*-*-* *:10,20,30,40:00"]);
        assert_eq!(cron("5/15 * * * *"), ["*-*-* *:05/15:00"]);
        assert_eq!(cron("0 0 */10 * *"), ["*-*-01/10 00:00:00"]);
        assert_eq!(cron("0 0 * */3 *"), ["*-01/3-* 00:00:00"]);
    }

    #[test]
    fn macros_expand_to_their_cron_meaning() {
        assert_eq!(cron("@hourly"), ["*-*-* *:00:00"]);
        assert_eq!(cron("@daily"), cron("@midnight"));
        assert_eq!(cron("@weekly"), ["Sun *-*-* 00:00:00"]);
        assert_eq!(cron("@monthly"), ["*-*-01 00:00:00"]);
        assert_eq!(cron("@yearly"), ["*-01-01 00:00:00"]);
        assert!(cron_to_on_calendar("@reboot").is_err());
    }

    #[test]
    fn sunday_is_zero_or_seven_and_weekdays_wrap() {
        assert_eq!(cron("0 0 * * 0"), cron("0 0 * * 7"));
        assert_eq!(cron("0 0 * * 5-7"), ["Fri..Sun *-*-* 00:00:00"]);
        assert_eq!(cron("0 0 * * 0,6"), ["Sat,Sun *-*-* 00:00:00"]);
        assert_eq!(cron("0 0 * * 0-6"), ["*-*-* 00:00:00"]);
        assert_eq!(cron("0 0 * * */2"), ["Tue,Thu,Sat,Sun *-*-* 00:00:00"]);
    }

    #[test]
    fn both_day_fields_restricted_means_either_like_cron() {
        assert_eq!(
            cron("0 9 1,15 * mon"),
            ["*-*-01,15 09:00:00", "Mon *-*-* 09:00:00"]
        );
        // A field starting with `*` keeps the AND rule, as in Vixie cron.
        assert_eq!(cron("0 9 */2 * mon"), ["Mon *-*-01/2 09:00:00"]);
    }

    #[test]
    fn rejects_malformed_cron() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "*/0 * * * *",
            "*/61 * * * *",
            "a * * * *",
            "* * * * monday",
            "$(id) * * * *",
            "0 3 * * *\nExecStart=/bin/sh",
            "1,,2 * * * *",
        ] {
            assert!(cron_to_on_calendar(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn calendar_expressions_cannot_escape_their_unit_line() {
        let ok = |e: &str| JobSchedule::Calendar(e.into()).on_calendar("UTC");
        assert_eq!(ok("daily").unwrap(), ["daily UTC"]);
        assert_eq!(
            ok("Mon..Fri *-*-* 03:00").unwrap(),
            ["Mon..Fri *-*-* 03:00 UTC"]
        );
        for bad in [
            "daily\nExecStart=/bin/sh",
            "daily %h",
            "daily \\",
            "-daily",
            "daily; rm",
            "\"daily\"",
            "a=b",
            "",
        ] {
            assert!(ok(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(ok(&"1".repeat(129)).is_err());
    }

    #[test]
    fn time_zones_are_iana_names() {
        for good in [
            "UTC",
            "Europe/Istanbul",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+3",
        ] {
            assert!(is_valid_timezone(good), "{good}");
        }
        for bad in [
            "",
            "../etc/passwd",
            "Europe/../../etc",
            "/UTC",
            "UTC ",
            "Europe/Istanbul\n",
            "a/b/c/d",
            "%Z",
        ] {
            assert!(!is_valid_timezone(bad), "{bad:?}");
        }
        let schedule = JobSchedule::Cron("0 3 * * *".into());
        assert_eq!(
            schedule.on_calendar("Europe/Istanbul").unwrap(),
            ["*-*-* 03:00:00 Europe/Istanbul"]
        );
        assert!(schedule.on_calendar("Mars/Base").is_ok());
        assert!(schedule.on_calendar("Europe/../x").is_err());
    }

    #[test]
    fn commands_are_argv_never_shell_strings() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for good in [
            argv(&["php", "artisan", "schedule:run"]),
            argv(&["sh", "/scripts/backup.sh"]),
            argv(&["pg_dump", "-c", "app"]),
            argv(&["python", "-c", "print(1)"]),
            argv(&["echo", "a; rm -rf /", "$(id)"]),
        ] {
            assert!(validate_job_command(&good).is_ok(), "{good:?}");
        }
        for bad in [
            argv(&[]),
            argv(&["sh", "-c", "echo hi"]),
            argv(&["/bin/bash", "-lc", "echo hi"]),
            argv(&["bash", "-e", "-c", "echo hi"]),
            argv(&["env", "FOO=1", "sh", "-c", "x"]),
            argv(&["busybox", "sh", "-c", "x"]),
            argv(&["nice", "/usr/bin/zsh", "-ec", "x"]),
            argv(&["fish", "--command=echo hi"]),
            argv(&["--privileged"]),
            argv(&["", "x"]),
            argv(&["echo", "a\nb"]),
            argv(&["echo", "a\0b"]),
            vec!["x".to_string(); 65],
            argv(&["echo", &"a".repeat(4097)]),
        ] {
            assert!(validate_job_command(&bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn spec_validation_checks_every_field() {
        let spec = ScheduledJobSpec {
            service_name: "web".into(),
            command: vec!["php".into(), "artisan".into(), "schedule:run".into()],
            schedule: JobSchedule::Cron("* * * * *".into()),
            timezone: "UTC".into(),
            timeout_secs: 300,
        };
        assert_eq!(spec.validate().unwrap(), ["*-*-* *:*:00 UTC"]);
        let bad = [
            ScheduledJobSpec {
                service_name: "-web".into(),
                ..spec.clone()
            },
            ScheduledJobSpec {
                timeout_secs: 0,
                ..spec.clone()
            },
            ScheduledJobSpec {
                timeout_secs: MAX_JOB_TIMEOUT_SECS + 1,
                ..spec.clone()
            },
            ScheduledJobSpec {
                timezone: "Europe/../etc".into(),
                ..spec.clone()
            },
        ];
        for spec in bad {
            assert!(spec.validate().is_err(), "{spec:?}");
        }
    }

    #[test]
    fn schedule_serializes_with_kind_and_expression() {
        let json = serde_json::to_value(JobSchedule::Cron("@daily".into())).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "cron", "expression": "@daily" })
        );
        let parsed: JobSchedule = serde_json::from_value(
            serde_json::json!({ "kind": "calendar", "expression": "weekly" }),
        )
        .unwrap();
        assert_eq!(parsed, JobSchedule::Calendar("weekly".into()));
    }
}
