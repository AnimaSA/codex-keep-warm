use std::{collections::HashMap, fmt, str::FromStr};

use chrono::{DateTime, Days, LocalResult, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

pub const SESSION_MINUTES: i64 = 300;
pub const WEEK_MINUTES: i64 = 10_080;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 30;
const RESET_FRESH_TOLERANCE_SECS: i64 = 300;
const AUTO_GUARD_SECS: i64 = 120;
const SCHEDULE_GRACE_SECS: i64 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DailyTime(u16);

impl DailyTime {
    pub fn hour(self) -> u32 {
        self.0 as u32 / 60
    }

    pub fn minute(self) -> u32 {
        self.0 as u32 % 60
    }
}

impl fmt::Display for DailyTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())
    }
}

impl FromStr for DailyTime {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (hour, minute) = value
            .split_once(':')
            .ok_or_else(|| "Use HH:MM".to_string())?;
        let hour: u16 = hour.parse().map_err(|_| "Invalid hour".to_string())?;
        let minute: u16 = minute.parse().map_err(|_| "Invalid minute".to_string())?;
        if hour > 23 || minute > 59 {
            return Err("Use a time from 00:00 to 23:59".to_string());
        }
        Ok(Self(hour * 60 + minute))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u8,
    pub refresh_interval_secs: u64,
    pub accounts: Vec<Account>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
            accounts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub connected: bool,
    pub enabled: bool,
    pub warmup_times: Vec<DailyTime>,
    pub ledger: WarmLedger,
    pub usage_history: UsageHistory,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            email: None,
            plan: None,
            connected: false,
            enabled: true,
            warmup_times: Vec::new(),
            ledger: WarmLedger::default(),
            usage_history: UsageHistory::default(),
        }
    }
}

impl Account {
    pub fn new(id: String, label: String) -> Self {
        Self {
            id,
            label: label.trim().to_string(),
            ..Self::default()
        }
    }

    pub fn add_time(&mut self, time: DailyTime) {
        if !self.warmup_times.contains(&time) {
            self.warmup_times.push(time);
            self.warmup_times.sort_unstable();
        }
    }

    pub fn observe_active_windows(&mut self, windows: &UsageWindows, now: i64) {
        if let Some(window) = &windows.session
            && window.resets_at > Some(now)
            && (window.used_percent > 0
                || self
                    .ledger
                    .confirmed_session_key
                    .as_deref()
                    .is_some_and(|key| key.ends_with(":expired")))
        {
            self.ledger.confirmed_session_key = window.key("session");
        }
        if let Some(window) = &windows.weekly
            && window.resets_at > Some(now)
            && (window.used_percent > 0
                || self
                    .ledger
                    .confirmed_weekly_key
                    .as_deref()
                    .is_some_and(|key| key.ends_with(":expired")))
        {
            self.ledger.confirmed_weekly_key = window.key("weekly");
        }
    }

    pub fn confirm_warmed_windows(&mut self, windows: &UsageWindows, now: i64) {
        if let Some(key) = windows
            .session
            .as_ref()
            .filter(|window| window.resets_at > Some(now))
            .and_then(|window| window.key("session"))
        {
            self.ledger.confirmed_session_key = Some(key);
        }
        if let Some(key) = windows
            .weekly
            .as_ref()
            .filter(|window| window.resets_at > Some(now))
            .and_then(|window| window.key("weekly"))
        {
            self.ledger.confirmed_weekly_key = Some(key);
        }
    }

    pub fn record_success(&mut self, plan: &WarmPlan, now: i64) {
        if let Some(key) = &plan.session_key {
            self.ledger.confirmed_session_key = Some(key.clone());
        }
        if let Some(key) = &plan.weekly_key {
            self.ledger.confirmed_weekly_key = Some(key.clone());
        }
        if let Some(key) = &plan.schedule_key {
            self.ledger.last_schedule_key = Some(key.clone());
        }
        self.ledger.retry_after = None;
        self.ledger.last_warmup = Some(WarmRecord {
            at: now,
            reason: plan.reason(),
            success: true,
        });
    }

    pub fn record_failure(&mut self, reason: WarmReason, now: i64) {
        self.ledger.retry_after = Some(now + 60);
        self.ledger.last_warmup = Some(WarmRecord {
            at: now,
            reason,
            success: false,
        });
    }

    pub fn record_manual_success(&mut self, now: i64) {
        self.ledger.retry_after = None;
        self.ledger.last_warmup = Some(WarmRecord {
            at: now,
            reason: WarmReason::Manual,
            success: true,
        });
    }

    pub fn record_usage(&mut self, windows: &UsageWindows, now: i64) {
        self.usage_history
            .session
            .record(windows.session.as_ref(), "session", now);
        self.usage_history
            .weekly
            .record(windows.weekly.as_ref(), "weekly", now);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageHistory {
    pub session: WindowHistory,
    pub weekly: WindowHistory,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowHistory {
    pub window_key: Option<String>,
    pub points: Vec<UsagePoint>,
    pub show_workweek_lines: bool,
}

impl WindowHistory {
    fn record(&mut self, window: Option<&LimitWindow>, kind: &str, now: i64) {
        let Some(window) = window else { return };
        let Some(duration) = window.window_duration_mins else {
            return;
        };
        let Some(reset) = window.resets_at else {
            return;
        };
        let Some(key) = window.key(kind) else { return };
        let prefix = format!("{kind}:{duration}:");
        let same_window = self.window_key.as_deref().is_some_and(|previous| {
            previous
                .strip_prefix(&prefix)
                .and_then(|value| value.parse::<i64>().ok())
                .is_some_and(|previous_reset| {
                    previous_reset.abs_diff(reset) <= RESET_FRESH_TOLERANCE_SECS as u64
                })
        });
        if !same_window {
            self.window_key = Some(key);
            self.points.clear();
        }
        let point = UsagePoint {
            at: now,
            used_percent: window.used_percent.clamp(0, 100),
        };
        if self.points.last() != Some(&point) {
            self.points.push(point);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePoint {
    pub at: i64,
    pub used_percent: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WarmLedger {
    pub confirmed_session_key: Option<String>,
    pub confirmed_weekly_key: Option<String>,
    pub last_schedule_key: Option<String>,
    pub retry_after: Option<i64>,
    pub last_warmup: Option<WarmRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WarmRecord {
    pub at: i64,
    pub reason: WarmReason,
    pub success: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WarmReason {
    WeeklyReset,
    Scheduled,
    SafeGap,
    Manual,
}

impl fmt::Display for WarmReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WeeklyReset => "weekly reset",
            Self::Scheduled => "scheduled warmup",
            Self::SafeGap => "safe gap",
            Self::Manual => "manual warmup",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    #[serde(default)]
    pub used_percent: i32,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<i64>,
}

impl LimitWindow {
    pub fn remaining_percent(&self) -> i32 {
        (100 - self.used_percent).clamp(0, 100)
    }

    pub fn key(&self, kind: &str) -> Option<String> {
        Some(format!(
            "{kind}:{}:{}",
            self.window_duration_mins?, self.resets_at?
        ))
    }

    fn is_fresh(&self, now: i64) -> bool {
        let (Some(duration), Some(reset)) = (self.window_duration_mins, self.resets_at) else {
            return false;
        };
        reset - now >= duration * 60 - RESET_FRESH_TOLERANCE_SECS
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    pub limit_id: Option<String>,
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResponse {
    pub rate_limits: RateLimitSnapshot,
    pub rate_limits_by_limit_id: Option<HashMap<String, RateLimitSnapshot>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageWindows {
    pub session: Option<LimitWindow>,
    pub weekly: Option<LimitWindow>,
}

impl UsageWindows {
    pub fn from_response(response: RateLimitResponse) -> Self {
        let snapshot = response
            .rate_limits_by_limit_id
            .as_ref()
            .and_then(|limits| {
                limits.get("codex").or_else(|| {
                    limits
                        .values()
                        .find(|limit| limit.limit_id.as_deref() == Some("codex"))
                })
            })
            .unwrap_or(&response.rate_limits);

        let all = [snapshot.primary.clone(), snapshot.secondary.clone()];
        let matches_duration = |window: &LimitWindow, minutes: i64| {
            window
                .window_duration_mins
                .is_some_and(|duration| (duration - minutes).abs() <= minutes / 20)
        };
        let by_duration = |minutes| {
            all.iter()
                .flatten()
                .find(|window| matches_duration(window, minutes))
                .cloned()
        };

        let session = by_duration(SESSION_MINUTES);
        let weekly = by_duration(WEEK_MINUTES);

        Self { session, weekly }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WarmPlan {
    pub session_key: Option<String>,
    pub weekly_key: Option<String>,
    pub schedule_key: Option<String>,
}

impl WarmPlan {
    pub fn is_empty(&self) -> bool {
        self.session_key.is_none() && self.weekly_key.is_none() && self.schedule_key.is_none()
    }

    pub fn reason(&self) -> WarmReason {
        if self.weekly_key.is_some() {
            WarmReason::WeeklyReset
        } else if self.schedule_key.is_some() {
            WarmReason::Scheduled
        } else {
            WarmReason::SafeGap
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSlot {
    pub at: i64,
    pub key: String,
}

pub fn plan_warmup<Tz: TimeZone>(
    now: DateTime<Tz>,
    account: &Account,
    windows: &UsageWindows,
) -> WarmPlan {
    if !account.enabled {
        return WarmPlan::default();
    }

    let now_ts = now.timestamp();
    let can_retry = account
        .ledger
        .retry_after
        .is_none_or(|retry| retry <= now_ts);
    let weekly_blocked = windows
        .weekly
        .as_ref()
        .is_some_and(|window| window.used_percent >= 100 && window.resets_at > Some(now_ts));

    let mut plan = WarmPlan::default();
    if can_retry && !weekly_blocked {
        plan.weekly_key = candidate_key(
            windows.weekly.as_ref(),
            "weekly",
            account.ledger.confirmed_weekly_key.as_deref(),
            now_ts,
        );
    }

    if can_retry
        && !weekly_blocked
        && let Some(slot) = due_slot(&now, &account.warmup_times)
        && account.ledger.last_schedule_key.as_deref() != Some(&slot.key)
        && schedule_window_ready(&slot, windows.session.as_ref(), now_ts)
    {
        plan.schedule_key = Some(slot.key);
    }

    let session_key = if can_retry && !weekly_blocked {
        candidate_key(
            windows.session.as_ref(),
            "session",
            account.ledger.confirmed_session_key.as_deref(),
            now_ts,
        )
    } else {
        None
    };

    if session_key.is_some()
        && (plan.weekly_key.is_some()
            || plan.schedule_key.is_some()
            || filler_is_safe(&now, &account.warmup_times, windows.session.as_ref()))
    {
        plan.session_key = session_key;
    }

    plan
}

fn candidate_key(
    window: Option<&LimitWindow>,
    kind: &str,
    confirmed: Option<&str>,
    now: i64,
) -> Option<String> {
    let window = window?;
    let mut key = window.key(kind)?;
    let expired = window.resets_at.is_some_and(|reset| reset <= now);
    if expired {
        key.push_str(":expired");
    }
    (confirmed != Some(&key) && (expired || window.used_percent == 0 && window.is_fresh(now)))
        .then_some(key)
}

fn filler_is_safe<Tz: TimeZone>(
    now: &DateTime<Tz>,
    times: &[DailyTime],
    session: Option<&LimitWindow>,
) -> bool {
    let Some(duration) = session.and_then(|window| window.window_duration_mins) else {
        return false;
    };
    next_slot(now, times)
        .is_none_or(|slot| now.timestamp() + duration * 60 + AUTO_GUARD_SECS <= slot.at)
}

fn schedule_window_ready(slot: &ScheduleSlot, session: Option<&LimitWindow>, now: i64) -> bool {
    !session
        .and_then(|window| window.resets_at)
        .is_some_and(|reset| reset > now && reset <= slot.at + SCHEDULE_GRACE_SECS)
}

pub fn due_slot<Tz: TimeZone>(now: &DateTime<Tz>, times: &[DailyTime]) -> Option<ScheduleSlot> {
    let now_ts = now.timestamp();
    times.iter().find_map(|time| {
        let at = resolve_local(&now.timezone(), now.date_naive(), *time)?;
        (now_ts >= at && now_ts <= at + SCHEDULE_GRACE_SECS).then(|| ScheduleSlot {
            at,
            key: format!("{}@{time}", now.date_naive()),
        })
    })
}

pub fn next_slot<Tz: TimeZone>(now: &DateTime<Tz>, times: &[DailyTime]) -> Option<ScheduleSlot> {
    for offset in 0..=2 {
        let date = now.date_naive().checked_add_days(Days::new(offset))?;
        for time in times {
            let Some(at) = resolve_local(&now.timezone(), date, *time) else {
                continue;
            };
            if at > now.timestamp() {
                return Some(ScheduleSlot {
                    at,
                    key: format!("{date}@{time}"),
                });
            }
        }
    }
    None
}

fn resolve_local<Tz: TimeZone>(timezone: &Tz, date: NaiveDate, time: DailyTime) -> Option<i64> {
    let local = date.and_hms_opt(time.hour(), time.minute(), 0)?;
    match timezone.from_local_datetime(&local) {
        LocalResult::Single(value) => Some(value.timestamp()),
        LocalResult::Ambiguous(first, second) => Some(first.timestamp().min(second.timestamp())),
        LocalResult::None => None,
    }
}

pub fn schedule_gap_warning(times: &[DailyTime]) -> bool {
    if times.len() < 2 {
        return false;
    }
    times
        .windows(2)
        .any(|pair| pair[1].0 - pair[0].0 < SESSION_MINUTES as u16)
        || 1_440 - times.last().unwrap().0 + times.first().unwrap().0 < SESSION_MINUTES as u16
}

pub fn format_reset(reset_at: Option<i64>, now: i64) -> String {
    let Some(reset) = reset_at else {
        return "Reset unavailable".to_string();
    };
    let seconds = reset - now;
    if seconds <= 0 {
        return "Resetting now".to_string();
    }
    if seconds < 3_600 {
        return format!("Resets in {}m", (seconds + 59) / 60);
    }
    if seconds < 86_400 {
        return format!("Resets in {}h {}m", seconds / 3_600, seconds % 3_600 / 60);
    }
    format!(
        "Resets in {}d {}h",
        seconds / 86_400,
        seconds % 86_400 / 3_600
    )
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};

    use super::*;

    fn local_time(hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .unwrap()
    }

    fn account_with_schedule() -> Account {
        let mut account = Account::new("one".into(), "One".into());
        for value in ["08:00", "13:00", "18:00"] {
            account.add_time(value.parse().unwrap());
        }
        account
    }

    fn reset_window(now: i64, reset_in_minutes: i64) -> UsageWindows {
        UsageWindows {
            session: Some(LimitWindow {
                used_percent: 0,
                window_duration_mins: Some(SESSION_MINUTES),
                resets_at: Some(now + reset_in_minutes * 60),
            }),
            weekly: None,
        }
    }

    #[test]
    fn parses_and_sorts_daily_times() {
        let mut account = Account::new("one".into(), "One".into());
        account.add_time("18:00".parse().unwrap());
        account.add_time("08:00".parse().unwrap());
        account.add_time("08:00".parse().unwrap());
        assert_eq!(
            account
                .warmup_times
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["08:00", "18:00"]
        );
        assert!("24:00".parse::<DailyTime>().is_err());
    }

    #[test]
    fn usage_history_tolerates_reset_jitter_but_clears_for_a_new_window() {
        let mut account = Account::new("one".into(), "One".into());
        let mut window = LimitWindow {
            used_percent: 10,
            window_duration_mins: Some(SESSION_MINUTES),
            resets_at: Some(20_000),
        };
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            10_000,
        );

        window.used_percent = 20;
        window.resets_at = Some(20_030);
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            10_030,
        );
        assert_eq!(account.usage_history.session.points.len(), 2);

        window.used_percent = 0;
        window.resets_at = Some(38_000);
        account.record_usage(
            &UsageWindows {
                session: Some(window),
                weekly: None,
            },
            20_000,
        );
        assert_eq!(account.usage_history.session.points.len(), 1);
    }

    #[test]
    fn scheduled_slot_fires_once_inside_its_minute() {
        let now = local_time(13, 0);
        let account = account_with_schedule();
        let plan = plan_warmup(now, &account, &UsageWindows::default());
        assert_eq!(plan.schedule_key.as_deref(), Some("2026-08-30@13:00"));
    }

    #[test]
    fn scheduled_slot_waits_for_a_slightly_late_reset() {
        let scheduled = local_time(13, 0);
        let account = account_with_schedule();
        let mut windows = reset_window(scheduled.timestamp(), 0);
        windows.session.as_mut().unwrap().resets_at = Some(scheduled.timestamp() + 5);
        assert!(plan_warmup(scheduled, &account, &windows).is_empty());

        let after_reset = local_time(13, 0) + chrono::Duration::seconds(30);
        windows.session.as_mut().unwrap().resets_at =
            Some(after_reset.timestamp() + SESSION_MINUTES * 60);
        assert!(
            plan_warmup(after_reset, &account, &windows)
                .schedule_key
                .is_some()
        );
    }

    #[test]
    fn safe_gap_allows_early_reset_and_protects_next_slot() {
        let early = local_time(1, 0);
        let account = account_with_schedule();
        let safe = reset_window(early.timestamp(), SESSION_MINUTES);
        assert!(plan_warmup(early, &account, &safe).session_key.is_some());

        let late = local_time(3, 0);
        let unsafe_window = reset_window(late.timestamp(), SESSION_MINUTES);
        assert!(plan_warmup(late, &account, &unsafe_window).is_empty());
    }

    #[test]
    fn weekly_reset_overrides_schedule_protection() {
        let now = local_time(12, 0);
        let mut account = account_with_schedule();
        let windows = UsageWindows {
            session: reset_window(now.timestamp(), SESSION_MINUTES).session,
            weekly: Some(LimitWindow {
                used_percent: 0,
                window_duration_mins: Some(WEEK_MINUTES),
                resets_at: Some(now.timestamp() + WEEK_MINUTES * 60),
            }),
        };
        let plan = plan_warmup(now, &account, &windows);
        assert!(plan.weekly_key.is_some());
        assert!(plan.session_key.is_some());
        account.record_success(&plan, now.timestamp());
        assert!(plan_warmup(now, &account, &windows).is_empty());
    }

    #[test]
    fn expired_full_weekly_window_starts_once() {
        let now = local_time(12, 0);
        let mut account = account_with_schedule();
        let window = LimitWindow {
            used_percent: 100,
            window_duration_mins: Some(WEEK_MINUTES),
            resets_at: Some(now.timestamp() - 1),
        };
        account.ledger.confirmed_weekly_key = window.key("weekly");
        let windows = UsageWindows {
            session: None,
            weekly: Some(window),
        };
        let plan = plan_warmup(now, &account, &windows);
        assert!(plan.weekly_key.as_deref().unwrap().ends_with(":expired"));
        account.record_success(&plan, now.timestamp());
        account.confirm_warmed_windows(&windows, now.timestamp());
        assert!(plan_warmup(now, &account, &windows).is_empty());

        let fresh = UsageWindows {
            session: None,
            weekly: Some(LimitWindow {
                used_percent: 0,
                window_duration_mins: Some(WEEK_MINUTES),
                resets_at: Some(now.timestamp() + WEEK_MINUTES * 60),
            }),
        };
        account.observe_active_windows(&fresh, now.timestamp());
        assert!(plan_warmup(now, &account, &fresh).is_empty());
    }

    #[test]
    fn normalizes_reversed_windows_by_duration() {
        let response = RateLimitResponse {
            rate_limits: RateLimitSnapshot {
                primary: Some(LimitWindow {
                    used_percent: 20,
                    window_duration_mins: Some(WEEK_MINUTES - 1),
                    resets_at: Some(2),
                }),
                secondary: Some(LimitWindow {
                    used_percent: 10,
                    window_duration_mins: Some(SESSION_MINUTES + 1),
                    resets_at: Some(1),
                }),
                ..RateLimitSnapshot::default()
            },
            rate_limits_by_limit_id: None,
        };
        let normalized = UsageWindows::from_response(response);
        assert_eq!(normalized.session.unwrap().resets_at, Some(1));
        assert_eq!(normalized.weekly.unwrap().resets_at, Some(2));
    }

    #[test]
    fn ignores_unrelated_limit_windows() {
        let response = RateLimitResponse {
            rate_limits: RateLimitSnapshot {
                primary: Some(LimitWindow {
                    used_percent: 10,
                    window_duration_mins: Some(15),
                    resets_at: Some(1),
                }),
                ..RateLimitSnapshot::default()
            },
            rate_limits_by_limit_id: None,
        };
        assert_eq!(
            UsageWindows::from_response(response),
            UsageWindows::default()
        );
    }

    #[test]
    fn warns_when_daily_slots_cannot_open_distinct_windows() {
        let close = ["08:00".parse().unwrap(), "12:00".parse().unwrap()];
        let spaced = ["08:00".parse().unwrap(), "13:00".parse().unwrap()];
        assert!(schedule_gap_warning(&close));
        assert!(!schedule_gap_warning(&spaced));
    }
}
