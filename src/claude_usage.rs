//! Claude's weekly window, read from the account usage endpoint that Claude
//! Code's `/usage` command calls.
//!
//! The OAuth token of the local Claude Code login is handed to curl through a
//! config on stdin, never through an argument list. The endpoint is not
//! documented, so any answer that does not match the expected shape is reported
//! as unavailable instead of being guessed.

use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::weekly_usage::Quota;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// curl enforces this itself, so a stuck request ends on its own.
const MAX_TIME_SECONDS: u32 = 10;

#[derive(Deserialize)]
struct Usage {
    seven_day: Option<Window>,
    seven_day_opus: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// The weekly window that binds first: the higher of the all-models and the
/// Opus limit. `Ok(None)` means the account reports no weekly window at all.
fn parse_weekly(json: &str) -> Result<Option<Quota>, ()> {
    let usage: Usage = serde_json::from_str(json).map_err(|_| ())?;
    let mut binding: Option<Quota> = None;
    for window in [usage.seven_day, usage.seven_day_opus]
        .into_iter()
        .flatten()
    {
        let used_percent = window.utilization.ok_or(())?;
        if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
            return Err(());
        }
        let quota = Quota {
            used_percent,
            resets_at: window.resets_at.as_deref().and_then(parse_reset),
        };
        if binding.is_none_or(|current| quota.used_percent > current.used_percent) {
            binding = Some(quota);
        }
    }
    Ok(binding)
}

fn parse_reset(text: &str) -> Option<SystemTime> {
    let at =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    let seconds = u64::try_from(at.unix_timestamp()).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Why the account usage could not be read.
#[derive(Debug, PartialEq)]
pub enum Fault {
    /// The endpoint asked us to wait; `Retry-After` in seconds when it sent one.
    Throttled(Option<u64>),
    Failed,
}

/// Opt-out: with `LUNA_NO_CLAUDE_USAGE` set, the login token is never read.
fn disabled() -> bool {
    std::env::var_os("LUNA_NO_CLAUDE_USAGE").is_some_and(|value| !value.is_empty() && value != "0")
}

fn credentials_path() -> Option<PathBuf> {
    Some(crate::agents::claude_config_dir()?.join(".credentials.json"))
}

/// Whether there is a Claude subscription login to ask about. Without one (no
/// Claude Code, an API key, or the opt-out) the limit is not shown at all.
pub fn available() -> bool {
    !disabled()
        && credentials_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| token_from_credentials(&text))
            .is_some()
}

pub fn fetch(cancelled: &AtomicBool) -> Result<Option<Quota>, Fault> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(Fault::Failed);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(Fault::Failed)?;
    let claude_dir = crate::agents::claude_config_dir().ok_or(Fault::Failed)?;
    let credentials =
        fs::read_to_string(claude_dir.join(".credentials.json")).map_err(|_| Fault::Failed)?;
    let token = token_from_credentials(&credentials).ok_or(Fault::Failed)?;
    let version = cli_version(&claude_dir, &home);
    let body = request(&token, &version)?;
    parse_weekly(&body).map_err(|()| Fault::Failed)
}

/// How long to wait before asking again after a throttled answer: the server's
/// `Retry-After`, never less than a minute and never more than fifteen.
pub fn retry_delay(retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(120).clamp(60, 900))
}

fn token_from_credentials(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Credentials {
        #[serde(rename = "claudeAiOauth")]
        oauth: Option<Oauth>,
    }
    #[derive(Deserialize)]
    struct Oauth {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
    }
    let parsed: Credentials = serde_json::from_str(text).ok()?;
    parsed.oauth?.access_token.filter(|token| !token.is_empty())
}

/// The CLI version the endpoint expects in `User-Agent`.
fn cli_version(claude_dir: &Path, home: &Path) -> String {
    #[derive(Deserialize)]
    struct LastUpdate {
        version_to: Option<String>,
    }
    let recorded = fs::read_to_string(claude_dir.join(".last-update-result.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<LastUpdate>(&text).ok())
        .and_then(|last| last.version_to);
    recorded
        .or_else(|| {
            let mut names: Vec<String> = fs::read_dir(home.join(".local/share/claude/versions"))
                .ok()?
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect();
            names.sort();
            names.pop()
        })
        .unwrap_or_else(|| "2.0.0".into())
}

fn request(token: &str, version: &str) -> Result<String, Fault> {
    // Without `fail`, curl hands back the body of a throttled answer as well, so
    // the status code and `Retry-After` are written after the body.
    let config = format!(
        "silent\nshow-error\nmax-time = {MAX_TIME_SECONDS}\nurl = \"{USAGE_URL}\"\n\
         write-out = \"\\n%{{http_code}} %header{{retry-after}}\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         header = \"anthropic-beta: oauth-2025-04-20\"\n\
         header = \"User-Agent: claude-code/{version}\"\n\
         header = \"Content-Type: application/json\"\n"
    );
    let mut child = Command::new("curl")
        .args(["-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Fault::Failed)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|_| Fault::Failed)?;
    }
    let output = child.wait_with_output().map_err(|_| Fault::Failed)?;
    if !output.status.success() {
        return Err(Fault::Failed);
    }
    let text = String::from_utf8(output.stdout).map_err(|_| Fault::Failed)?;
    split_status(&text)
}

/// Splits the body from curl's trailing status line, such as `200 ` or `429 19`.
fn split_status(text: &str) -> Result<String, Fault> {
    let (body, status) = text.rsplit_once('\n').ok_or(Fault::Failed)?;
    let mut parts = status.split_whitespace();
    let code: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or(Fault::Failed)?;
    match code {
        200..=299 => Ok(body.to_owned()),
        429 => Err(Fault::Throttled(
            parts.next().and_then(|seconds| seconds.parse().ok()),
        )),
        _ => Err(Fault::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_higher_of_the_two_weekly_windows_binds() {
        let quota = parse_weekly(
            r#"{
                "five_hour": {"utilization": 9.0, "resets_at": "2026-11-18T12:00:00Z"},
                "seven_day": {"utilization": 20.0, "resets_at": "2026-11-20T10:00:00Z"},
                "seven_day_opus": {"utilization": 45.5, "resets_at": "2026-11-21T10:00:00Z"}
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(quota.used_percent, 45.5);
        assert_eq!(
            quota.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_795_255_200))
        );
    }

    #[test]
    fn a_missing_weekly_window_is_reported_as_none() {
        let quota = parse_weekly(r#"{"five_hour": {"utilization": 9.0}, "seven_day": null}"#);

        assert_eq!(quota, Ok(None));
    }

    #[test]
    fn a_window_without_a_reset_time_keeps_its_percentage() {
        let quota = parse_weekly(r#"{"seven_day": {"utilization": 12.0, "resets_at": null}}"#)
            .unwrap()
            .unwrap();

        assert_eq!(quota.used_percent, 12.0);
        assert_eq!(quota.resets_at, None);
    }

    #[test]
    fn out_of_range_or_malformed_answers_are_unavailable() {
        assert!(parse_weekly(r#"{"seven_day": {"utilization": 101.0}}"#).is_err());
        assert!(parse_weekly(r#"{"seven_day": {"utilization": null}}"#).is_err());
        assert!(parse_weekly("<html>gateway</html>").is_err());
    }

    #[test]
    fn the_oauth_access_token_is_read_from_the_claude_login() {
        assert_eq!(
            token_from_credentials(
                r#"{"claudeAiOauth": {"accessToken": "tok-123", "refreshToken": "x"}}"#
            ),
            Some("tok-123".into())
        );
        assert_eq!(
            token_from_credentials(r#"{"claudeAiOauth": {"accessToken": ""}}"#),
            None
        );
        assert_eq!(token_from_credentials(r#"{"other": 1}"#), None);
    }

    #[test]
    fn reset_times_parse_as_rfc3339_with_offsets() {
        assert_eq!(
            parse_reset("2000-01-01T01:00:00+01:00"),
            Some(UNIX_EPOCH + Duration::from_secs(946_684_800))
        );
        assert_eq!(parse_reset("not a time"), None);
    }

    #[test]
    fn a_throttled_answer_keeps_its_retry_after_and_is_not_a_failure() {
        assert_eq!(
            split_status("{\"error\":{}}\n429 19"),
            Err(Fault::Throttled(Some(19)))
        );
        assert_eq!(
            split_status("{\"error\":{}}\n429 "),
            Err(Fault::Throttled(None))
        );
    }

    #[test]
    fn a_successful_answer_returns_the_body_and_other_statuses_fail() {
        assert_eq!(
            split_status("{\"seven_day\":null}\n200 "),
            Ok("{\"seven_day\":null}".to_owned())
        );
        assert_eq!(split_status("<html>\n500 "), Err(Fault::Failed));
        assert_eq!(split_status("no status line"), Err(Fault::Failed));
    }

    #[test]
    fn retry_waits_are_at_least_a_minute_and_at_most_fifteen() {
        assert_eq!(retry_delay(Some(19)), Duration::from_secs(60));
        assert_eq!(retry_delay(None), Duration::from_secs(120));
        assert_eq!(retry_delay(Some(10_000)), Duration::from_secs(900));
    }
}
