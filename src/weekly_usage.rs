use serde::de::{DeserializeOwned, IgnoredAny, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Which coding agent a weekly quota belongs to. The poller is shared; only the
/// fetch and the label differ.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Codex,
    Claude,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Codex => "CODEX",
            Self::Claude => "CLAUDE",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }

    /// How often the weekly window is asked again. The Claude endpoint is
    /// rate limited and the window moves slowly, so it is asked less often.
    fn interval(self) -> Duration {
        match self {
            Self::Codex => Duration::from_secs(300),
            Self::Claude => Duration::from_secs(600),
        }
    }

    /// Whether this agent exists on the machine at all. One that does not is
    /// not shown, rather than shown as unreachable.
    fn present(self) -> bool {
        match self {
            Self::Codex => super::agents::codex_present(),
            Self::Claude => super::claude_usage::available(),
        }
    }

    fn fetch(self, cancelled: &AtomicBool) -> Outcome {
        match self {
            Self::Codex => match codex_weekly_quota(cancelled) {
                Ok(Some(quota)) => Outcome::Quota(quota),
                Ok(None) => Outcome::NoWeeklyWindow,
                Err(()) => Outcome::Unavailable,
            },
            Self::Claude => match super::claude_usage::fetch(cancelled) {
                Ok(Some(quota)) => Outcome::Quota(quota),
                Ok(None) => Outcome::NoWeeklyWindow,
                Err(super::claude_usage::Fault::Throttled(retry_after)) => {
                    Outcome::Throttled(retry_after)
                }
                Err(super::claude_usage::Fault::Failed) => Outcome::Unavailable,
            },
        }
    }
}

/// One weekly quota as the dashboard draws it: numbers, not text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UsageView {
    Loading,
    NoWindow,
    Unavailable,
    Throttled,
    /// The agent is not on this machine: nothing is drawn.
    Absent,
    Quota {
        used_percent: f64,
        resets_at: Option<SystemTime>,
        stale: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Quota {
    pub(super) used_percent: f64,
    /// `None` when the provider reports the window without a reset time.
    pub(super) resets_at: Option<SystemTime>,
}

enum State {
    Loading,
    Available(Quota),
    Stale(Quota),
    NoWeeklyWindow,
    Unavailable,
    Throttled,
    Absent,
}

enum Outcome {
    Quota(Quota),
    NoWeeklyWindow,
    Unavailable,
    Throttled(Option<u64>),
}

pub struct WeeklyUsage {
    source: Source,
    state: State,
    sender: Sender<Outcome>,
    receiver: Receiver<Outcome>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    next_refresh: Instant,
}

impl WeeklyUsage {
    pub fn new(source: Source) -> Self {
        Self::with_presence(source, source.present())
    }

    fn with_presence(source: Source, present: bool) -> Self {
        let (sender, receiver) = mpsc::channel();
        let state = if present {
            State::Loading
        } else {
            State::Absent
        };
        Self {
            source,
            state,
            sender,
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            worker: None,
            next_refresh: Instant::now(),
        }
    }

    pub fn refresh(&mut self) {
        if self.worker.is_some() || matches!(self.state, State::Absent) {
            return;
        }
        self.next_refresh = Instant::now() + self.source.interval();
        let sender = self.sender.clone();
        let cancelled = Arc::clone(&self.cancelled);
        let source = self.source;
        self.worker = Some(thread::spawn(move || {
            let outcome = source.fetch(&cancelled);
            if !cancelled.load(Ordering::Relaxed) {
                let _ = sender.send(outcome);
            }
        }));
    }

    pub fn refresh_if_due(&mut self) {
        if Instant::now() >= self.next_refresh {
            self.refresh();
        }
    }

    pub fn poll(&mut self) -> bool {
        match self.receiver.try_recv() {
            Ok(outcome) => {
                self.join_worker();
                self.apply(outcome);
                true
            }
            Err(TryRecvError::Empty)
                if self.worker.as_ref().is_some_and(JoinHandle::is_finished) =>
            {
                self.join_worker();
                self.apply(Outcome::Unavailable);
                true
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
        }
    }

    pub fn view(&self) -> UsageView {
        match &self.state {
            State::Loading => UsageView::Loading,
            State::NoWeeklyWindow => UsageView::NoWindow,
            State::Unavailable => UsageView::Unavailable,
            State::Throttled => UsageView::Throttled,
            State::Absent => UsageView::Absent,
            State::Available(quota) | State::Stale(quota) => UsageView::Quota {
                used_percent: quota.used_percent,
                resets_at: quota.resets_at,
                stale: matches!(self.state, State::Stale(_)),
            },
        }
    }

    /// The one-line form for the phone view. `None` when the agent is absent.
    pub fn line(&self) -> Option<String> {
        let label = self.source.label();
        let title = self.source.title();
        Some(match &self.state {
            State::Absent => return None,
            State::Loading => format!("{label}  — · Wochenlimit wird geladen"),
            State::NoWeeklyWindow => format!("{label}  — · kein Wochenfenster verfügbar"),
            State::Unavailable => format!("{label}  — · {title}-Limit nicht erreichbar"),
            State::Throttled => format!("{label}  — · Anfrage gedrosselt, neuer Versuch folgt"),
            State::Available(quota) => quota_line(label, *quota, false),
            State::Stale(quota) => quota_line(label, *quota, true),
        })
    }

    fn apply(&mut self, outcome: Outcome) {
        self.state = match outcome {
            Outcome::Quota(quota) => State::Available(quota),
            Outcome::NoWeeklyWindow => State::NoWeeklyWindow,
            Outcome::Throttled(retry_after) => {
                // Ask again after the server's wait; keep the last known value
                // visible, marked as stale, instead of claiming it is unreachable.
                self.next_refresh = Instant::now() + super::claude_usage::retry_delay(retry_after);
                match &self.state {
                    State::Available(quota) | State::Stale(quota) => State::Stale(*quota),
                    _ => State::Throttled,
                }
            }
            Outcome::Unavailable => match &self.state {
                State::Available(quota) | State::Stale(quota) => State::Stale(*quota),
                State::Loading
                | State::NoWeeklyWindow
                | State::Unavailable
                | State::Absent
                | State::Throttled => State::Unavailable,
            },
        };
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WeeklyUsage {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.join_worker();
    }
}

fn quota_line(label: &str, quota: Quota, stale: bool) -> String {
    let free_percent = (100.0 - quota.used_percent).clamp(0.0, 100.0);
    let mut parts = vec![
        format!("{free_percent:.1}% frei"),
        format!("{:.1}% genutzt", quota.used_percent),
    ];
    if stale {
        parts.push("veraltet".into());
    }
    if let Some(resets_at) = quota.resets_at {
        let reset_in = resets_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            .as_secs();
        parts.push(format!(
            "zurück in {}",
            super::format_reset(reset_in, false)
        ));
    }
    format!("{label}  {}", parts.join(" · "))
}

/// The Codex weekly window. The running app-server is asked first, which costs
/// one local socket round trip. Without one, the CLI starts its own short-lived
/// app-server to answer.
fn codex_weekly_quota(cancelled: &AtomicBool) -> Result<Option<Quota>, ()> {
    if let Some(Ok(value)) = super::agents::codex_rate_limits() {
        if let Ok(response) = serde_json::from_value::<RateLimitsResponse>(value) {
            return parse_codex_weekly_quota(&response);
        }
    }
    request_rate_limits(cancelled)
}

fn request_rate_limits(cancelled: &AtomicBool) -> Result<Option<Quota>, ()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(());
    }
    let mut child = Command::new("codex")
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let (mut stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
        (Some(stdin), Some(stdout)) => (stdin, stdout),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        }
    };
    let (line_sender, line_receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if line_sender.send(line).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    let result = (|| {
        send_message(
            &mut stdin,
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "luna",
                        "title": "Luna",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        )?;
        let initialized: RpcResponse<IgnoredAny> =
            read_response(&mut child, &line_receiver, 1, cancelled)?;
        if initialized.error.is_some() || initialized.result.is_none() {
            return Err(());
        }

        send_message(&mut stdin, json!({ "method": "initialized" }))?;
        send_message(
            &mut stdin,
            json!({ "id": 2, "method": "account/rateLimits/read" }),
        )?;
        let response: RpcResponse<RateLimitsResponse> =
            read_response(&mut child, &line_receiver, 2, cancelled)?;
        if response.error.is_some() {
            return Err(());
        }
        response
            .result
            .as_ref()
            .map(parse_codex_weekly_quota)
            .ok_or(())?
    })();

    let _ = child.kill();
    let _ = child.wait();
    drop(stdin);
    let _ = reader.join();
    result
}

fn send_message(writer: &mut impl Write, message: Value) -> Result<(), ()> {
    serde_json::to_writer(&mut *writer, &message).map_err(|_| ())?;
    writer.write_all(b"\n").map_err(|_| ())?;
    writer.flush().map_err(|_| ())
}

fn read_response<T: DeserializeOwned>(
    child: &mut Child,
    lines: &Receiver<String>,
    request_id: u64,
    cancelled: &AtomicBool,
) -> Result<RpcResponse<T>, ()> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(());
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        match lines.recv_timeout(wait) {
            Ok(line) => {
                let header = match serde_json::from_str::<RpcHeader>(&line) {
                    Ok(header) => header,
                    Err(_) => continue,
                };
                if header.id != Some(request_id) {
                    continue;
                }
                let response: RpcResponse<T> = serde_json::from_str(&line).map_err(|_| ())?;
                return (response.id == Some(request_id))
                    .then_some(response)
                    .ok_or(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if child.try_wait().map_err(|_| ())?.is_some() {
                    return Err(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(()),
        }
    }
}

fn parse_codex_weekly_quota(response: &RateLimitsResponse) -> Result<Option<Quota>, ()> {
    let bucket = match &response.rate_limits_by_limit_id {
        Some(buckets) => buckets.0.as_ref().or_else(|| {
            response
                .rate_limits
                .as_ref()
                .filter(|limit| limit.limit_id.as_deref() == Some("codex"))
        }),
        None => response.rate_limits.as_ref().filter(|limit| {
            limit
                .limit_id
                .as_deref()
                .is_none_or(|limit_id| limit_id == "codex")
        }),
    };
    let Some(bucket) = bucket else {
        return Ok(None);
    };
    let weekly = bucket
        .secondary
        .as_ref()
        .filter(|window| window.window_duration_mins == Some(10_080))
        .or_else(|| {
            bucket
                .primary
                .as_ref()
                .filter(|window| window.window_duration_mins == Some(10_080))
        });
    let Some(weekly) = weekly else {
        return Ok(None);
    };
    let used_percent = weekly
        .used_percent
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .ok_or(())?;
    let reset_seconds = weekly.resets_at.ok_or(())?;
    let resets_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(reset_seconds))
        .ok_or(())?;
    Ok(Some(Quota {
        used_percent,
        resets_at: Some(resets_at),
    }))
}

#[derive(Deserialize)]
struct RpcHeader {
    id: Option<u64>,
}

#[derive(Deserialize)]
struct RpcResponse<T> {
    id: Option<u64>,
    result: Option<T>,
    error: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResponse {
    rate_limits: Option<RateLimitBucket>,
    rate_limits_by_limit_id: Option<CodexBucketMap>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitBucket {
    limit_id: Option<String>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: Option<f64>,
    window_duration_mins: Option<u64>,
    resets_at: Option<u64>,
}

struct CodexBucketMap(Option<RateLimitBucket>);

impl<'de> Deserialize<'de> for CodexBucketMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CodexBucketVisitor;

        impl<'de> Visitor<'de> for CodexBucketVisitor {
            type Value = CodexBucketMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a rate-limit bucket map")
            }

            fn visit_map<A>(self, mut buckets: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut codex = None;
                while let Some(bucket_id) = buckets.next_key::<String>()? {
                    if bucket_id == "codex" {
                        codex = buckets.next_value::<Option<RateLimitBucket>>()?;
                    } else {
                        buckets.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(CodexBucketMap(codex))
            }
        }

        deserializer.deserialize_map(CodexBucketVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quota_without_a_reset_time_names_its_agent_and_nothing_else() {
        let quota = Quota {
            used_percent: 45.5,
            resets_at: None,
        };

        assert_eq!(
            quota_line("CLAUDE", quota, false),
            "CLAUDE  54.5% frei · 45.5% genutzt"
        );
        assert_eq!(
            quota_line("CLAUDE", quota, true),
            "CLAUDE  54.5% frei · 45.5% genutzt · veraltet"
        );
    }

    #[test]
    fn a_quota_with_a_reset_keeps_the_codex_wording() {
        let resets_at = SystemTime::now() + Duration::from_secs(4 * 86_400 + 22 * 3_600 + 1_800);
        let quota = Quota {
            used_percent: 42.0,
            resets_at: Some(resets_at),
        };

        assert_eq!(
            quota_line("CODEX", quota, false),
            "CODEX  58.0% frei · 42.0% genutzt · zurück in 4 T 22 Std"
        );
        assert_eq!(
            quota_line("CODEX", quota, true),
            "CODEX  58.0% frei · 42.0% genutzt · veraltet · zurück in 4 T 22 Std"
        );
    }

    #[test]
    fn a_fresh_poller_has_no_numbers_to_draw() {
        let usage = WeeklyUsage::with_presence(Source::Claude, true);

        assert_eq!(usage.view(), UsageView::Loading);
        assert_eq!(
            usage.line().as_deref(),
            Some("CLAUDE  — · Wochenlimit wird geladen")
        );
    }

    #[test]
    fn an_agent_that_is_not_installed_is_absent_and_never_asked() {
        let mut usage = WeeklyUsage::with_presence(Source::Codex, false);
        usage.refresh();

        assert_eq!(usage.view(), UsageView::Absent);
        assert_eq!(usage.line(), None);
        assert!(usage.worker.is_none(), "an absent agent must not be polled");
        assert!(!usage.poll());
    }

    fn parse_limits(json: &str) -> Result<Option<Quota>, ()> {
        let response = serde_json::from_str::<RateLimitsResponse>(json).map_err(|_| ())?;
        parse_codex_weekly_quota(&response)
    }

    #[test]
    fn reads_only_the_codex_weekly_window() {
        let quota = parse_limits(
            r#"{
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 2000000000},
                        "secondary": {"usedPercent": 64, "windowDurationMins": 10080, "resetsAt": 2000000123}
                    },
                    "codex_other": {
                        "secondary": {"usedPercent": 99, "windowDurationMins": 10080, "resetsAt": 2000000456}
                    }
                }
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(quota.used_percent, 64.0);
        assert_eq!(
            quota.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(2_000_000_123))
        );
    }

    #[test]
    fn does_not_use_another_bucket_as_the_codex_weekly_window() {
        let quota = parse_limits(
            r#"{
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 2000000000}
                    },
                    "codex_other": {
                        "secondary": {"usedPercent": 99, "windowDurationMins": 10080, "resetsAt": 2000000456}
                    }
                }
            }"#,
        )
        .unwrap();

        assert!(quota.is_none());
    }

    #[test]
    fn supports_the_single_codex_bucket_response() {
        let quota = parse_limits(
            r#"{
                "rateLimits": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 45, "windowDurationMins": 10080, "resetsAt": 2000000000}
                }
            }"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(quota.used_percent, 45.0);
    }

    #[test]
    fn rejects_invalid_usage_percentages() {
        let result = parse_limits(
            r#"{
                "rateLimitsByLimitId": {
                    "codex": {
                        "secondary": {"usedPercent": 101, "windowDurationMins": 10080, "resetsAt": 2000000000}
                    }
                }
            }"#,
        );

        assert!(result.is_err());
    }
}
