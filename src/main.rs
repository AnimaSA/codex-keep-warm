#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

mod codex;
mod domain;
#[cfg(target_os = "windows")]
mod startup;
mod storage;

use std::str::FromStr;

use chrono::{Datelike, Local, TimeZone, Weekday};
use dioxus::desktop::{
    Config, WindowBuilder, WindowCloseBehaviour,
    trayicon::{default_tray_icon, init_tray_icon},
};
use dioxus::prelude::*;
use tokio::time::{Duration, interval};

use codex::AccountSnapshot;
use domain::{
    Account, DEFAULT_REFRESH_INTERVAL_SECS, DailyTime, LimitWindow, UsageWindows, WarmReason,
    WindowHistory, format_reset, next_slot, plan_warmup, schedule_gap_warning,
};
use storage::{Store, new_account_id};

const CSS: &str = include_str!("../assets/main.css");

#[derive(Clone)]
struct AccountView {
    account: Account,
    limits: Option<UsageWindows>,
    busy: Option<String>,
    error: Option<String>,
    draft_time: String,
}

impl AccountView {
    fn new(account: Account) -> Self {
        Self {
            account,
            limits: None,
            busy: None,
            error: None,
            draft_time: "08:00".to_string(),
        }
    }
}

#[derive(Clone)]
struct Notice {
    message: String,
    error: bool,
}

#[derive(Clone)]
struct AppState {
    store: Option<Store>,
    accounts: Vec<AccountView>,
    refresh_interval_secs: u64,
    fatal: Option<String>,
    notice: Option<Notice>,
}

impl AppState {
    fn load() -> Self {
        match Store::discover() {
            Err(error) => Self {
                store: None,
                accounts: Vec::new(),
                refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
                fatal: Some(error),
                notice: None,
            },
            Ok(store) => match store.load() {
                Ok(config) => Self {
                    store: Some(store),
                    accounts: config.accounts.into_iter().map(AccountView::new).collect(),
                    refresh_interval_secs: config.refresh_interval_secs,
                    fatal: None,
                    notice: None,
                },
                Err(error) => Self {
                    store: Some(store),
                    accounts: Vec::new(),
                    refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
                    fatal: Some(error),
                    notice: None,
                },
            },
        }
    }
}

fn main() {
    let starts_hidden = std::env::args_os().any(|arg| arg == "--hidden");
    let config = Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title("Codex Keep Warm")
                .with_visible(!starts_hidden),
        )
        .with_close_behaviour(WindowCloseBehaviour::LastWindowHides);

    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(App);
}

#[component]
fn App() -> Element {
    let _tray = use_hook(|| init_tray_icon(default_tray_icon(), None));
    let mut state = use_signal(AppState::load);
    let mut now = use_signal(|| Local::now().timestamp());
    let mut show_add = use_signal(|| false);
    let mut new_label = use_signal(String::new);
    let mut new_times = use_signal(|| {
        [
            "08:00".to_string(),
            "13:00".to_string(),
            "18:00".to_string(),
        ]
    });
    let mut form_error = use_signal(|| None::<String>);
    let mut pending_delete = use_signal(|| None::<String>);

    use_future(move || async move {
        let mut tick = 0_u64;
        let mut ticks = interval(Duration::from_secs(1));
        loop {
            ticks.tick().await;
            let current = Local::now().timestamp();
            now.set(current);
            let current_state = state.read();
            let ids = current_state
                .accounts
                .iter()
                .filter(|view| view.account.connected && view.busy.is_none())
                .filter(|view| {
                    tick.is_multiple_of(current_state.refresh_interval_secs)
                        || weekly_reset_needs_probe(view, current)
                })
                .map(|view| view.account.id.clone())
                .collect::<Vec<_>>();
            drop(current_state);
            for id in ids {
                spawn(refresh_account(state, id, true));
            }
            tick += 1;
        }
    });

    let snapshot = state.read().clone();
    if let Some(error) = snapshot.fatal {
        return rsx! {
            document::Title { "Codex Keep Warm" }
            style { {CSS} }
            main { class: "fatal-shell",
                section { class: "fatal-card",
                    div { class: "brand-mark", "CK" }
                    p { class: "eyebrow", "Codex Keep Warm" }
                    h1 { "Settings could not be loaded" }
                    p { class: "fatal-message", "{error}" }
                    p { class: "muted", "Fix or move the settings file, then restart the app." }
                }
            }
        };
    }

    let local_now = Local
        .timestamp_opt(*now.read(), 0)
        .single()
        .unwrap_or_else(Local::now);
    let account_count = snapshot.accounts.len();
    let schedule_count = snapshot
        .accounts
        .iter()
        .map(|view| view.account.warmup_times.len())
        .sum::<usize>();
    let next_warmup = snapshot
        .accounts
        .iter()
        .filter(|view| view.account.enabled)
        .filter_map(|view| next_slot(&local_now, &view.account.warmup_times))
        .min_by_key(|slot| slot.at)
        .map(|slot| format_slot(slot.at))
        .unwrap_or_else(|| "No times set".to_string());

    rsx! {
        document::Title { "Codex Keep Warm" }
        style { {CSS} }
        main { class: "app-shell",
            header { class: "topbar",
                div { class: "brand",
                    div { class: "brand-mark", "CK" }
                    div {
                        strong { "Codex Keep Warm" }
                        span { "Local account scheduler" }
                    }
                }
                div { class: "top-actions",
                    div { class: "scheduler-state",
                        span { class: "pulse-dot" }
                        "Scheduler running"
                    }
                    StartupToggle { state }
                    button {
                        class: "button primary",
                        onclick: move |_| {
                            form_error.set(None);
                            show_add.set(true);
                        },
                        "+ Add account"
                    }
                }
            }

            section { class: "hero",
                div {
                    p { class: "eyebrow", "Quota alignment" }
                    h1 { "Codex limits, lined up." }
                    p { class: "hero-copy",
                        "Scheduled warmups and reset-driven starts share one guard, per account."
                    }
                }
                div { class: "summary-grid",
                    SummaryCard { value: account_count.to_string(), label: "Accounts".to_string() }
                    SummaryCard { value: schedule_count.to_string(), label: "Daily slots".to_string() }
                    SummaryCard { value: next_warmup, label: "Next scheduled".to_string() }
                }
            }

            if let Some(notice) = snapshot.notice {
                div { class: if notice.error { "notice error" } else { "notice" },
                    span { "{notice.message}" }
                    button {
                        aria_label: "Dismiss notification",
                        onclick: move |_| state.write().notice = None,
                        "Close"
                    }
                }
            }

            if snapshot.accounts.is_empty() {
                section { class: "empty-state",
                    div { class: "empty-orbit",
                        div { class: "empty-core", "5h" }
                    }
                    p { class: "eyebrow", "No accounts yet" }
                    h2 { "Add your first ChatGPT account" }
                    p {
                        "A browser sign-in creates a separate Codex credential store. Your active Codex login stays untouched."
                    }
                    button {
                        class: "button primary large",
                        onclick: move |_| show_add.set(true),
                        "Add account"
                    }
                }
            } else {
                section { class: "account-list",
                    div { class: "section-heading",
                        div {
                            p { class: "eyebrow", "Accounts" }
                            h2 { "Windows and warmup times" }
                        }
                        label { class: "poll-setting",
                            "Refresh every"
                            input {
                                r#type: "number",
                                min: "5",
                                max: "{DEFAULT_REFRESH_INTERVAL_SECS}",
                                value: "{snapshot.refresh_interval_secs}",
                                aria_label: "Limit refresh interval in seconds",
                                onchange: move |event| {
                                    if let Ok(seconds) = event.value().parse() {
                                        set_refresh_interval(state, seconds);
                                    }
                                }
                            }
                            "seconds"
                        }
                    }

                    for view in snapshot.accounts {
                        {
                            let id = view.account.id.clone();
                            let refresh_id = id.clone();
                            let warm_id = id.clone();
                            let login_id = id.clone();
                            let toggle_id = id.clone();
                            let draft_time_id = id.clone();
                            let add_time_id = id.clone();
                            let delete_id = id.clone();
                            let confirm_delete_id = id.clone();
                            let busy = view.busy.is_some();
                            let is_pending_delete = pending_delete.read().as_deref() == Some(&id);
                            let initials = initials(&view.account.label);
                            let identity = view.account.email.clone().unwrap_or_else(|| "ChatGPT account".to_string());
                            let plan = view.account.plan.as_deref().map(plan_label).unwrap_or_else(|| "Unknown plan".to_string());
                            let limits = view.limits.clone().unwrap_or_default();
                            let last_activity = last_activity(&view.account);
                            let status_text = account_status(&view, &local_now);
                            let has_gap_warning = schedule_gap_warning(&view.account.warmup_times);

                            rsx! {
                                article { class: "account-card", key: "{id}",
                                    div { class: "account-head",
                                        div { class: "identity",
                                            div { class: "avatar", "{initials}" }
                                            div {
                                                div { class: "name-line",
                                                    h3 { "{view.account.label}" }
                                                    span { class: "plan-badge", "{plan}" }
                                                    if view.busy.as_deref() == Some("Refreshing limits") {
                                                        span { class: "busy-status",
                                                            span { class: "spinner" }
                                                            "Refreshing limits"
                                                        }
                                                    }
                                                }
                                                p { "{identity}" }
                                            }
                                        }
                                        div { class: "card-actions",
                                            button {
                                                class: "button ghost",
                                                disabled: busy || !view.account.connected,
                                                onclick: move |_| {
                                                    spawn(refresh_account(state, refresh_id.clone(), false));
                                                },
                                                "Refresh"
                                            }
                                            button {
                                                class: "button secondary",
                                                disabled: busy || !view.account.connected,
                                                onclick: move |_| {
                                                    spawn(manual_warmup(state, warm_id.clone()));
                                                },
                                                "Warm now"
                                            }
                                            button {
                                                class: "button ghost",
                                                disabled: busy,
                                                onclick: move |_| {
                                                    spawn(login_account(state, login_id.clone()));
                                                },
                                                if view.account.connected { "Reconnect" } else { "Sign in" }
                                            }
                                        }
                                    }

                                    if let Some(action) = &view.busy
                                        && action != "Refreshing limits"
                                    {
                                        div { class: "busy-line",
                                            span { class: "spinner" }
                                            "{action}"
                                        }
                                    }
                                    if let Some(error) = &view.error {
                                        div { class: "account-error", "{error}" }
                                    }

                                    div { class: "limit-grid",
                                        LimitPanel {
                                            label: "5h burst".to_string(),
                                            window: limits.session,
                                            history: view.account.usage_history.session.clone(),
                                            fallback_history: Some(view.account.usage_history.weekly.clone()),
                                            fallback_window: limits.weekly.clone(),
                                            now: *now.read(),
                                            account_id: Some(id.clone()),
                                            show_workweek_lines: view.account.usage_history.session.show_workweek_lines,
                                            weekly_history: false,
                                            state
                                        }
                                        LimitPanel {
                                            label: "Weekly".to_string(),
                                            window: limits.weekly,
                                            history: view.account.usage_history.weekly.clone(),
                                            fallback_history: None,
                                            fallback_window: None,
                                            now: *now.read(),
                                            account_id: Some(id.clone()),
                                            show_workweek_lines: view.account.usage_history.weekly.show_workweek_lines,
                                            weekly_history: true,
                                            state
                                        }
                                    }

                                    div { class: "schedule-panel",
                                        div { class: "schedule-copy",
                                            div { class: "schedule-title-row",
                                                h4 { "Warmup times" }
                                                button {
                                                    class: if view.account.enabled { "switch on" } else { "switch" },
                                                    aria_label: "Toggle automatic warmups",
                                                    aria_pressed: "{view.account.enabled}",
                                                    disabled: busy,
                                                    onclick: move |_| toggle_automatic(state, &toggle_id),
                                                    span {}
                                                }
                                            }
                                            p { "Local time. Weekly resets always take priority." }
                                        }
                                        div { class: "time-editor",
                                            div { class: "time-chips",
                                                if view.account.warmup_times.is_empty() {
                                                    span { class: "empty-chip", "No scheduled times" }
                                                }
                                                for time in view.account.warmup_times.clone() {
                                                    {
                                                        let remove_id = id.clone();
                                                        rsx! {
                                                            span { class: "time-chip", key: "{time}",
                                                                "{time}"
                                                                button {
                                                                    aria_label: "Remove {time}",
                                                                    disabled: busy,
                                                                    onclick: move |_| remove_time(state, &remove_id, time),
                                                                    "×"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            div { class: "add-time",
                                                input {
                                                    r#type: "time",
                                                    aria_label: "New warmup time",
                                                    value: "{view.draft_time}",
                                                    disabled: busy,
                                                    oninput: move |event| update_draft_time(state, &draft_time_id, event.value())
                                                }
                                                button {
                                                    class: "button compact",
                                                    disabled: busy,
                                                    onclick: move |_| add_time(state, &add_time_id),
                                                    "Add time"
                                                }
                                            }
                                        }
                                    }
                                    if has_gap_warning {
                                        p { class: "schedule-warning",
                                            "Two slots are less than five hours apart, so both cannot open distinct burst windows."
                                        }
                                    }

                                    div { class: "card-footer",
                                        div { class: "status-copy",
                                            span { class: if view.account.connected { "status-dot connected" } else { "status-dot" } }
                                            div {
                                                strong { "{status_text}" }
                                                span { "{last_activity}" }
                                            }
                                        }
                                        if is_pending_delete {
                                            div { class: "delete-confirm",
                                                span { "Remove credentials and this account?" }
                                                button {
                                                    class: "button danger compact",
                                                    disabled: busy,
                                                    onclick: move |_| {
                                                        pending_delete.set(None);
                                                        spawn(delete_account(state, confirm_delete_id.clone()));
                                                    },
                                                    "Remove"
                                                }
                                                button {
                                                    class: "button ghost compact",
                                                    onclick: move |_| pending_delete.set(None),
                                                    "Cancel"
                                                }
                                            }
                                        } else {
                                            button {
                                                class: "delete-link",
                                                disabled: busy,
                                                onclick: move |_| pending_delete.set(Some(delete_id.clone())),
                                                "Remove account"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            footer { class: "app-footer",
                span { "Credentials are isolated per account by Codex." }
                span { "Close hides to the tray. Click the tray icon to reopen." }
            }
        }

        if *show_add.read() {
            div { class: "modal-backdrop", role: "presentation",
                section { class: "modal", role: "dialog", aria_modal: "true", aria_labelledby: "add-title",
                    div { class: "modal-head",
                        div {
                            p { class: "eyebrow", "New account" }
                            h2 { id: "add-title", "Sign in separately" }
                        }
                        button {
                            class: "modal-close",
                            aria_label: "Close",
                            onclick: move |_| show_add.set(false),
                            "×"
                        }
                    }
                    p { class: "modal-copy",
                        "Codex opens ChatGPT in your browser and stores this login under its own account directory."
                    }
                    label { class: "field",
                        span { "Account label" }
                        input {
                            autofocus: true,
                            placeholder: "Personal",
                            value: "{new_label}",
                            oninput: move |event| new_label.set(event.value())
                        }
                    }
                    fieldset { class: "field time-fieldset",
                        legend { "Daily warmup times" }
                        p { "Edit or clear these. You can add more later." }
                        div { class: "modal-times",
                            for index in 0..3 {
                                input {
                                    key: "{index}",
                                    r#type: "time",
                                    aria_label: "Warmup time {index + 1}",
                                    value: "{new_times.read()[index]}",
                                    oninput: move |event| new_times.write()[index] = event.value()
                                }
                            }
                        }
                    }
                    if let Some(error) = form_error.read().as_ref() {
                        p { class: "form-error", "{error}" }
                    }
                    div { class: "modal-actions",
                        button {
                            class: "button ghost",
                            onclick: move |_| show_add.set(false),
                            "Cancel"
                        }
                        button {
                            class: "button primary",
                            onclick: move |_| {
                                let label = new_label.read().trim().to_string();
                                if label.is_empty() {
                                    form_error.set(Some("Enter an account label.".to_string()));
                                    return;
                                }
                                let times = new_times
                                    .read()
                                    .iter()
                                    .filter(|value| !value.trim().is_empty())
                                    .map(|value| DailyTime::from_str(value))
                                    .collect::<Result<Vec<_>, _>>();
                                let Ok(times) = times else {
                                    form_error.set(Some("Check the warmup times.".to_string()));
                                    return;
                                };
                                let Some(store) = state.read().store.clone() else { return };
                                let id = new_account_id();
                                if let Err(error) = store.prepare_account(&id) {
                                    form_error.set(Some(error));
                                    return;
                                }
                                let mut account = Account::new(id.clone(), label);
                                for time in times {
                                    account.add_time(time);
                                }
                                state.write().accounts.push(AccountView::new(account));
                                persist(state);
                                new_label.set(String::new());
                                new_times.set(["08:00".into(), "13:00".into(), "18:00".into()]);
                                form_error.set(None);
                                show_add.set(false);
                                spawn(login_account(state, id));
                            },
                            "Add and sign in"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[component]
fn StartupToggle(mut state: Signal<AppState>) -> Element {
    let mut enabled = use_signal(startup::is_enabled);

    rsx! {
        div { class: "scheduler-state",
            "Start with Windows"
            button {
                class: if *enabled.read() { "switch on" } else { "switch" },
                aria_label: "Start with Windows",
                aria_pressed: "{enabled}",
                onclick: move |_| {
                    let next = !*enabled.read();
                    match startup::set_enabled(next) {
                        Ok(()) => {
                            enabled.set(next);
                            state.write().notice = Some(Notice {
                                message: if next {
                                    "Windows startup enabled. The app will open in the tray after sign-in."
                                } else {
                                    "Windows startup disabled."
                                }
                                .to_string(),
                                error: false,
                            });
                        }
                        Err(error) => {
                            state.write().notice = Some(Notice {
                                message: error,
                                error: true,
                            });
                        }
                    }
                },
                span {}
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
#[component]
fn StartupToggle(_state: Signal<AppState>) -> Element {
    None
}

#[component]
fn SummaryCard(value: String, label: String) -> Element {
    rsx! {
        div { class: "summary-card",
            strong { "{value}" }
            span { "{label}" }
        }
    }
}

#[component]
fn LimitPanel(
    label: String,
    window: Option<LimitWindow>,
    history: WindowHistory,
    fallback_history: Option<WindowHistory>,
    fallback_window: Option<LimitWindow>,
    now: i64,
    account_id: Option<String>,
    show_workweek_lines: bool,
    weekly_history: bool,
    state: Signal<AppState>,
) -> Element {
    let mut show_burndown = use_signal(|| false);
    let mut show_today = use_signal(|| false);
    if let Some(window) = window {
        let toggle_id = account_id.clone();
        let remaining = window.remaining_percent();
        let progress_class = if remaining <= 10 {
            "progress-fill critical"
        } else if remaining <= 30 {
            "progress-fill warning"
        } else {
            "progress-fill"
        };
        let reset = format_reset(window.resets_at, now);
        let reset_time = reset.strip_prefix("Resets in ");
        let burndown = burndown(&window, &history);
        let pace = pace(&window, now);
        rsx! {
            div { class: "limit-panel-wrap",
                button {
                    class: "limit-panel",
                    aria_label: "Toggle {label} burndown chart",
                    aria_pressed: "{show_burndown}",
                    onclick: move |_| show_burndown.toggle(),
                div { class: "limit-top",
                    span { "{label}" }
                    div { class: "limit-value",
                        if let Some((pace_label, pace_delta)) = pace {
                            span {
                                class: if pace_delta < 0 { "pace-chip ahead" } else { "pace-chip" },
                                "{pace_label} {pace_delta:+}%"
                            }
                        }
                        strong { "{remaining}%" }
                    }
                }
                if *show_burndown.read() {
                    if let Some(chart) = burndown {
                        {
                            let latest = chart.points.last().copied();
                            let chart_projection = chart.projected_used.clamp(0, 100);
                            let projection_x = chart.projection_x;
                            let history_points = chart.points
                                .iter()
                                .map(|(x, y)| format!("{x},{y}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            let workweek_lines = account_id
                                .as_ref()
                                .filter(|_| show_workweek_lines)
                                .map(|_| {
                                    if weekly_history {
                                        workweek_lines(chart.start, chart.end)
                                    } else {
                                        workday_lines(chart.start, chart.end)
                                    }
                                })
                                .unwrap_or_default();
                            rsx! {
                                svg {
                                    class: "burndown-chart",
                                    role: "img",
                                    title { "{label} quota burndown" }
                                    line { class: "burndown-grid", x1: "0%", y1: "50%", x2: "100%", y2: "50%" }
                                    line { class: "burndown-grid", x1: "50%", y1: "0%", x2: "50%", y2: "100%" }
                                    for (x, class) in workweek_lines {
                                        line { class: "burndown-workweek {class}", x1: "{x}%", y1: "0%", x2: "{x}%", y2: "100%" }
                                    }
                                    svg {
                                        class: "burndown-history",
                                        view_box: "0 0 100 100",
                                        preserve_aspect_ratio: "none",
                                        polyline {
                                            class: "burndown-past",
                                            points: "{history_points}"
                                        }
                                    }
                                    if let Some((x, y)) = latest {
                                        line {
                                            class: "burndown-projection",
                                            x1: "{x}%",
                                            y1: "{y}%",
                                            x2: "{projection_x}%",
                                            y2: "{chart_projection}%"
                                        }
                                        circle { class: "burndown-now", cx: "{x}%", cy: "{y}%", r: "3" }
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "burndown-unavailable", "Reset timing unavailable" }
                    }
                } else {
                    div {
                        class: "progress-track",
                        role: "progressbar",
                        aria_label: "{label} remaining",
                        aria_valuemin: "0",
                        aria_valuemax: "100",
                        aria_valuenow: "{remaining}",
                        div { class: "{progress_class}", style: "width: {remaining}%" }
                    }
                }
                    div { class: "limit-meta",
                        span { if *show_burndown.read() { "Burndown" } else { "Remaining" } }
                        span { class: "reset-time",
                            if let Some(reset_time) = reset_time {
                                span { class: "reset-label", "Resets in" }
                                strong { "{reset_time}" }
                            } else {
                                strong { "{reset}" }
                            }
                        }
                    }
                }
                if *show_burndown.read() && let Some(toggle_id) = toggle_id {
                    button {
                        class: if show_workweek_lines { "workweek-toggle on" } else { "workweek-toggle" },
                        title: "Toggle workweek lines",
                        aria_label: "Toggle workweek lines",
                        aria_pressed: "{show_workweek_lines}",
                        onclick: move |_| toggle_workweek_lines(state, &toggle_id, weekly_history),
                        svg { view_box: "0 0 24 24",
                            path { d: "M7 3v3M17 3v3M4 9h16M5 5h14a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1Z" }
                        }
                    }
                }
            }
        }
    } else if let Some(history) = fallback_history {
        let showing_today = *show_today.read();
        let chart = if showing_today {
            today_usage(&history, now)
        } else {
            rolling_usage(&history, now)
        };
        let history_points = chart
            .as_ref()
            .map(|chart| {
                chart
                    .points
                    .iter()
                    .map(|(x, y)| format!("{x},{y}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let used_percent = chart.as_ref().map(|chart| chart.used_percent);
        let today_projection = showing_today
            .then(|| {
                chart
                    .as_ref()
                    .and_then(|chart| Some((chart.points.last().copied()?, chart.projected_used?)))
            })
            .flatten();
        let daily_pace = showing_today
            .then(|| used_percent.and_then(|used| daily_pace(used, now)))
            .flatten();
        let daily_quota_lines = showing_today
            .then(|| {
                fallback_window
                    .as_ref()
                    .and_then(|window| daily_quota_lines(window, now))
            })
            .flatten();
        let five_hours_ago = showing_today.then(|| five_hours_ago_line(now)).flatten();
        let view_label = if showing_today {
            "Today's usage"
        } else {
            "5h usage"
        };
        let chart_title = if showing_today {
            "Weekly quota usage today"
        } else {
            "Weekly quota usage over the last 5 hours"
        };
        let toggle_label = if showing_today {
            "Show 5-hour rolling usage"
        } else {
            "Show today's usage"
        };
        rsx! {
            button {
                class: "limit-panel",
                aria_label: "{toggle_label}",
                aria_pressed: "{showing_today}",
                onclick: move |_| show_today.toggle(),
                div { class: "limit-top",
                    span { "{view_label}" }
                    div { class: "limit-value",
                        if let Some((pace_label, pace_delta)) = daily_pace {
                            span {
                                class: if pace_delta < 0 { "pace-chip ahead" } else { "pace-chip" },
                                "{pace_label} {pace_delta:+}%"
                            }
                        }
                        if let Some(used_percent) = used_percent {
                            strong { class: "rolling-usage-value", "{used_percent}%" }
                        } else {
                            strong { class: "rolling-usage-value", "—" }
                        }
                    }
                }
                if chart.is_some() {
                    svg {
                        class: "burndown-chart",
                        role: "img",
                        title { "{chart_title}" }
                        line { class: "burndown-grid", x1: "0%", y1: "50%", x2: "100%", y2: "50%" }
                        line { class: "burndown-grid", x1: "50%", y1: "0%", x2: "50%", y2: "100%" }
                        if let Some(x) = five_hours_ago {
                            line { class: "burndown-five-hour", x1: "{x}%", y1: "0%", x2: "{x}%", y2: "100%" }
                        }
                        if let Some((start, end)) = daily_quota_lines {
                            line { class: "burndown-day-quota start", x1: "0%", y1: "{start}%", x2: "100%", y2: "{start}%" }
                            line { class: "burndown-day-quota end", x1: "0%", y1: "{end}%", x2: "100%", y2: "{end}%" }
                        }
                        svg {
                            class: "burndown-history",
                            view_box: "0 0 100 100",
                            preserve_aspect_ratio: "none",
                            polyline { class: "burndown-past", points: "{history_points}" }
                        }
                        if let Some(((x, y), projected_used)) = today_projection {
                            line {
                                class: "burndown-projection",
                                x1: "{x}%",
                                y1: "{y}%",
                                x2: "100%",
                                y2: "{projected_used.clamp(0, 100)}%"
                            }
                            circle { class: "burndown-now", cx: "{x}%", cy: "{y}%", r: "3" }
                        }
                    }
                } else {
                    div { class: "burndown-unavailable", "Usage history unavailable" }
                }
                div { class: "limit-meta",
                    span { if showing_today { "Since local midnight" } else { "Weekly quota change" } }
                }
            }
        }
    } else {
        rsx! {
            button { class: "limit-panel unavailable", disabled: true,
                div { class: "limit-top",
                    span { "{label}" }
                    strong { "—" }
                }
                div { class: "progress-track",
                    div { class: "progress-fill", style: "width: 0%" }
                }
                div { class: "limit-meta",
                    span { "Unavailable" }
                    span { "Refresh after sign-in" }
                }
            }
        }
    }
}

struct UsageChart {
    points: Vec<(f64, i32)>,
    used_percent: i32,
    projected_used: Option<i32>,
}

fn rolling_usage(history: &WindowHistory, now: i64) -> Option<UsageChart> {
    const FIVE_HOURS: i64 = 5 * 60 * 60;

    usage_since(history, now - FIVE_HOURS, now, now)
}

fn today_usage(history: &WindowHistory, now: i64) -> Option<UsageChart> {
    let (start, end) = local_day_bounds(now)?;
    usage_since(history, start, now, end)
}

fn local_day_bounds(now: i64) -> Option<(i64, i64)> {
    let date = Local.timestamp_opt(now, 0).single()?.date_naive();
    let tomorrow = date.checked_add_days(chrono::Days::new(1))?;
    Some((
        Local
            .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .timestamp(),
        Local
            .from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .timestamp(),
    ))
}

fn usage_since(history: &WindowHistory, start: i64, now: i64, end: i64) -> Option<UsageChart> {
    let duration = end - start;
    if duration <= 0 || history.points.iter().rfind(|point| point.at <= now)?.at < start {
        return None;
    }
    let baseline = history
        .points
        .iter()
        .take_while(|point| point.at <= start)
        .last()
        .or_else(|| history.points.iter().find(|point| point.at <= now))?;
    let observations = std::iter::once(baseline)
        .chain(
            history
                .points
                .iter()
                .filter(|point| point.at > baseline.at && point.at <= now),
        )
        .collect::<Vec<_>>();
    let latest = observations.last()?;
    let elapsed = latest.at - start;
    let used_percent = (latest.used_percent - baseline.used_percent).max(0);
    Some(UsageChart {
        points: observations
            .iter()
            .map(|point| {
                (
                    ((point.at - start) as f64 * 100.0 / duration as f64).clamp(0.0, 100.0),
                    point.used_percent,
                )
            })
            .collect(),
        used_percent,
        projected_used: (elapsed > 0)
            .then(|| baseline.used_percent + (i64::from(used_percent) * duration / elapsed) as i32),
    })
}

fn daily_quota_lines(window: &LimitWindow, now: i64) -> Option<(f64, f64)> {
    let duration = window.window_duration_mins? * 60;
    let reset = window.resets_at?;
    let window_start = reset - duration;
    let (day_start, day_end) = local_day_bounds(now)?;
    let y = |at| ((at - window_start) as f64 * 100.0 / duration as f64).clamp(0.0, 100.0);
    Some((y(day_start), y(day_end)))
}

fn daily_pace(used_percent: i32, now: i64) -> Option<(&'static str, i32)> {
    let (start, end) = local_day_bounds(now)?;
    let expected_used = (now - start) as f64 * 100.0 / 7.0 / (end - start) as f64;
    let delta = (expected_used - used_percent as f64).round() as i32;
    Some(if delta < 0 {
        ("DEFICIT", delta)
    } else {
        ("SURPLUS", delta)
    })
}

fn five_hours_ago_line(now: i64) -> Option<f64> {
    let (start, end) = local_day_bounds(now)?;
    let at = now - 5 * 60 * 60;
    (at >= start).then(|| (at - start) as f64 * 100.0 / (end - start) as f64)
}

struct Burndown {
    points: Vec<(f64, i32)>,
    projected_used: i32,
    projection_x: f64,
    start: i64,
    end: i64,
}

fn burndown(window: &LimitWindow, history: &WindowHistory) -> Option<Burndown> {
    let duration = window.window_duration_mins? * 60;
    if duration <= 0 {
        return None;
    }
    let reset = window.resets_at?;
    let start = reset - duration;
    let active_start = history
        .points
        .windows(2)
        .position(|pair| pair[1].used_percent > pair[0].used_percent)
        .unwrap_or_else(|| history.points.len().saturating_sub(1));
    let observations = &history.points[active_start..];
    let points = observations
        .iter()
        .map(|point| {
            (
                ((point.at - start) as f64 * 100.0 / duration as f64).clamp(0.0, 100.0),
                point.used_percent,
            )
        })
        .collect::<Vec<_>>();
    let first = observations.first()?;
    let last = observations.last()?;
    let observed_seconds = last.at - first.at;
    let observed_usage = last.used_percent - first.used_percent;
    let projected_used = if observed_seconds > 0 && observed_usage > 0 {
        last.used_percent
            + (i64::from(observed_usage) * (reset - last.at).max(0) / observed_seconds) as i32
    } else {
        last.used_percent
    }
    .max(last.used_percent);
    let runout = history
        .points
        .iter()
        .find(|point| point.used_percent >= 100)
        .map(|point| point.at)
        .or_else(|| {
            (observed_seconds > 0 && observed_usage > 0).then(|| {
                last.at
                    + i64::from(100 - last.used_percent).max(0) * observed_seconds
                        / i64::from(observed_usage)
            })
        });
    let projection_x = runout
        .map(|runout| ((runout - start) as f64 * 100.0 / duration as f64).clamp(0.0, 100.0))
        .unwrap_or(100.0);
    Some(Burndown {
        points,
        projected_used,
        projection_x,
        start,
        end: reset,
    })
}

fn workweek_lines(start: i64, end: i64) -> Vec<(f64, &'static str)> {
    work_lines(start, end, true)
}

fn workday_lines(start: i64, end: i64) -> Vec<(f64, &'static str)> {
    work_lines(start, end, false)
}

fn work_lines(start: i64, end: i64, weekly: bool) -> Vec<(f64, &'static str)> {
    let duration = end - start;
    let Some(first_date) = Local
        .timestamp_opt(start, 0)
        .single()
        .map(|at| at.date_naive())
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for date in (0..=7).filter_map(|offset| first_date.checked_add_days(chrono::Days::new(offset)))
    {
        for (hour, class) in [(9, "start"), (17, "end")] {
            if weekly
                && !matches!(
                    (date.weekday(), hour),
                    (Weekday::Mon, 9) | (Weekday::Fri, 17)
                )
            {
                continue;
            }
            let Some(time) = date.and_hms_opt(hour, 0, 0) else {
                continue;
            };
            let Some(at) = Local.from_local_datetime(&time).earliest() else {
                continue;
            };
            let at = at.timestamp();
            if at >= start && at <= end {
                lines.push(((at - start) as f64 * 100.0 / duration as f64, class));
            }
        }
    }
    lines
}

fn pace(window: &LimitWindow, now: i64) -> Option<(&'static str, i32)> {
    let duration = window.window_duration_mins? * 60;
    let reset = window.resets_at?;
    if duration <= 0 {
        return None;
    }
    let expected_remaining = ((reset - now) as f64 * 100.0 / duration as f64)
        .round()
        .clamp(0.0, 100.0) as i32;
    let remaining = window.remaining_percent();
    let delta = remaining - expected_remaining;
    Some(if remaining == 0 {
        ("EMPTY", delta)
    } else if delta < 0 {
        ("DEFICIT", delta)
    } else {
        ("SURPLUS", delta)
    })
}

#[cfg(test)]
mod chart_tests {
    use super::*;

    #[test]
    fn projects_burndown_from_elapsed_window_time() {
        let mut account = Account::new("one".into(), "One".into());
        let mut window = LimitWindow {
            used_percent: 10,
            window_duration_mins: Some(100),
            resets_at: Some(6_000),
        };
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            500,
        );
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            1_000,
        );
        window.used_percent = 30;
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            3_000,
        );
        let chart = burndown(&window, &account.usage_history.session).unwrap();
        assert_eq!(chart.points, [(16.666666666666668, 10), (50.0, 30)]);
        assert_eq!(chart.projected_used, 60);
        assert_eq!(chart.projection_x, 100.0);
        assert_eq!(pace(&window, 3_000), Some(("SURPLUS", 20)));

        window.used_percent = 70;
        assert_eq!(pace(&window, 3_000), Some(("DEFICIT", -20)));

        window.used_percent = 80;
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            4_000,
        );
        let chart = burndown(&window, &account.usage_history.session).unwrap();
        assert!(chart.projected_used > 100);
        assert!(chart.projection_x < 100.0);
        assert_eq!(pace(&window, 4_000), Some(("DEFICIT", -13)));

        window.used_percent = 100;
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            4_500,
        );
        let empty_pace = pace(&window, 4_500);
        account.record_usage(
            &UsageWindows {
                session: Some(window.clone()),
                weekly: None,
            },
            5_000,
        );
        assert_eq!(pace(&window, 4_500), empty_pace);
        assert_eq!(empty_pace, Some(("EMPTY", -25)));

        window.resets_at = Some(12_000);
        account.record_usage(
            &UsageWindows {
                session: Some(window),
                weekly: None,
            },
            6_000,
        );
        assert_eq!(account.usage_history.session.points.len(), 1);
    }

    #[test]
    fn charts_weekly_usage_over_the_last_five_hours() {
        let history = WindowHistory {
            window_key: None,
            points: vec![
                domain::UsagePoint {
                    at: 1_000,
                    used_percent: 10,
                },
                domain::UsagePoint {
                    at: 10_000,
                    used_percent: 12,
                },
                domain::UsagePoint {
                    at: 19_000,
                    used_percent: 15,
                },
            ],
            ..WindowHistory::default()
        };

        let chart = rolling_usage(&history, 20_000).unwrap();
        assert_eq!(chart.used_percent, 5);
        assert_eq!(
            chart.points,
            [(0.0, 10), (44.44444444444444, 12), (94.44444444444444, 15)]
        );
    }

    #[test]
    fn charts_weekly_usage_since_local_midnight() {
        let midnight = Local.with_ymd_and_hms(2026, 9, 8, 0, 0, 0).unwrap();
        let start = midnight.timestamp();
        let history = WindowHistory {
            window_key: None,
            points: vec![
                domain::UsagePoint {
                    at: start - 3_600,
                    used_percent: 10,
                },
                domain::UsagePoint {
                    at: start + 6 * 3_600,
                    used_percent: 14,
                },
                domain::UsagePoint {
                    at: start + 12 * 3_600,
                    used_percent: 18,
                },
            ],
            ..WindowHistory::default()
        };

        let chart = today_usage(&history, start + 12 * 3_600).unwrap();
        assert_eq!(chart.used_percent, 8);
        assert_eq!(chart.projected_used, Some(26));
        assert_eq!(chart.points, [(0.0, 10), (25.0, 14), (50.0, 18)]);

        let window = LimitWindow {
            used_percent: 18,
            window_duration_mins: Some(domain::WEEK_MINUTES),
            resets_at: Some(start + domain::WEEK_MINUTES * 60),
        };
        let lines = daily_quota_lines(&window, start + 12 * 3_600).unwrap();
        assert_eq!(lines.0, 0.0);
        assert!((lines.1 - 100.0 / 7.0).abs() < 0.001);
        assert_eq!(
            daily_pace(chart.used_percent, start + 12 * 3_600),
            Some(("DEFICIT", -1))
        );
        assert_eq!(
            five_hours_ago_line(start + 12 * 3_600),
            Some(100.0 * 7.0 / 24.0)
        );
        assert_eq!(five_hours_ago_line(start + 4 * 3_600), None);
    }

    #[test]
    fn marks_local_workweek_boundaries() {
        let start = Local.with_ymd_and_hms(2026, 9, 6, 0, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2026, 9, 13, 0, 0, 0).unwrap();
        let lines = workweek_lines(start.timestamp(), end.timestamp());
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].1, lines[1].1), ("start", "end"));
        assert!(lines[0].0 < lines[1].0);
    }

    #[test]
    fn marks_daily_boundaries_in_five_hour_windows() {
        let morning = Local.with_ymd_and_hms(2026, 9, 8, 7, 0, 0).unwrap();
        let afternoon = Local.with_ymd_and_hms(2026, 9, 8, 14, 0, 0).unwrap();
        let midday = Local.with_ymd_and_hms(2026, 9, 8, 10, 0, 0).unwrap();
        let five_hours = 5 * 60 * 60;

        assert_eq!(
            workday_lines(morning.timestamp(), morning.timestamp() + five_hours),
            [(40.0, "start")]
        );
        assert_eq!(
            workday_lines(afternoon.timestamp(), afternoon.timestamp() + five_hours),
            [(60.0, "end")]
        );
        assert!(workday_lines(midday.timestamp(), midday.timestamp() + five_hours).is_empty());
    }
}

async fn login_account(mut state: Signal<AppState>, id: String) {
    let Some((store, mut account)) = begin_operation(state, &id, "Waiting for browser sign-in")
    else {
        return;
    };
    if let Err(error) = store.prepare_account(&id) {
        finish_operation(state, &id, account, None, Some(error));
        return;
    }
    match codex::login(&store.account_home(&id)).await {
        Ok(snapshot) => {
            apply_snapshot(&mut account, &snapshot, Local::now().timestamp());
            account.connected = true;
            account.observe_active_windows(&snapshot.limits, Local::now().timestamp());
            finish_operation(state, &id, account, Some(snapshot.limits), None);
            state.write().notice = Some(Notice {
                message:
                    "Account connected. Limits will stay isolated from your active Codex login."
                        .to_string(),
                error: false,
            });
        }
        Err(error) => finish_operation(state, &id, account, None, Some(error)),
    }
}

async fn refresh_account(mut state: Signal<AppState>, id: String, automatic: bool) {
    let Some((store, mut account)) = begin_operation(state, &id, "Refreshing limits") else {
        return;
    };
    let initial = match codex::fetch_snapshot(&store.account_home(&id)).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if error.contains("not signed in") {
                account.connected = false;
            }
            finish_operation(state, &id, account, None, Some(error));
            return;
        }
    };
    let decision_time = Local::now();
    apply_snapshot(&mut account, &initial, decision_time.timestamp());
    account.connected = true;
    account.observe_active_windows(&initial.limits, decision_time.timestamp());

    if !automatic {
        finish_operation(state, &id, account, Some(initial.limits), None);
        return;
    }

    let plan = plan_warmup(decision_time, &account, &initial.limits);
    if plan.is_empty() {
        finish_operation(state, &id, account, Some(initial.limits), None);
        return;
    }

    set_busy(&mut state, &id, format!("Starting {}", plan.reason()));
    match codex::warm_and_fetch(&store.account_home(&id), &store.warmup_workspace(&id)).await {
        Ok(outcome) => {
            let completed_at = Local::now().timestamp();
            account.record_success(&plan, completed_at);
            let limits = outcome
                .snapshot
                .as_ref()
                .map(|snapshot| {
                    apply_snapshot(&mut account, snapshot, completed_at);
                    account.observe_active_windows(&snapshot.limits, completed_at);
                    account.confirm_warmed_windows(&snapshot.limits, completed_at);
                    snapshot.limits.clone()
                })
                .unwrap_or(initial.limits);
            finish_operation(state, &id, account, Some(limits), outcome.refresh_error);
        }
        Err(error) => {
            account.record_failure(plan.reason(), Local::now().timestamp());
            finish_operation(state, &id, account, Some(initial.limits), Some(error));
        }
    }
}

async fn manual_warmup(mut state: Signal<AppState>, id: String) {
    let Some((store, mut account)) = begin_operation(state, &id, "Sending a minimal warmup") else {
        return;
    };
    match codex::warm_and_fetch(&store.account_home(&id), &store.warmup_workspace(&id)).await {
        Ok(outcome) => {
            let completed_at = Local::now().timestamp();
            account.record_manual_success(completed_at);
            let limits = outcome.snapshot.as_ref().map(|snapshot| {
                apply_snapshot(&mut account, snapshot, completed_at);
                account.observe_active_windows(&snapshot.limits, completed_at);
                account.confirm_warmed_windows(&snapshot.limits, completed_at);
                snapshot.limits.clone()
            });
            finish_operation(state, &id, account, limits, outcome.refresh_error);
            state.write().notice = Some(Notice {
                message: "Warmup completed.".to_string(),
                error: false,
            });
        }
        Err(error) => {
            account.record_failure(WarmReason::Manual, Local::now().timestamp());
            finish_operation(state, &id, account, None, Some(error));
        }
    }
}

async fn delete_account(mut state: Signal<AppState>, id: String) {
    let Some((store, account)) = begin_operation(state, &id, "Removing account credentials") else {
        return;
    };
    if let Err(error) = codex::logout(&store.account_home(&id)).await {
        finish_operation(state, &id, account, None, Some(error));
        return;
    }
    if let Err(error) = store.remove_account(&id) {
        finish_operation(state, &id, account, None, Some(error));
        return;
    }
    state.write().accounts.retain(|view| view.account.id != id);
    persist(state);
    state.write().notice = Some(Notice {
        message: "Account and its isolated credentials were removed.".to_string(),
        error: false,
    });
}

fn begin_operation(
    mut state: Signal<AppState>,
    id: &str,
    action: &str,
) -> Option<(Store, Account)> {
    let mut current = state.write();
    let store = current.store.clone()?;
    let view = current
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)?;
    if view.busy.is_some() {
        return None;
    }
    view.busy = Some(action.to_string());
    view.error = None;
    Some((store, view.account.clone()))
}

fn set_busy(state: &mut Signal<AppState>, id: &str, action: String) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        view.busy = Some(action);
    }
}

fn finish_operation(
    mut state: Signal<AppState>,
    id: &str,
    account: Account,
    limits: Option<UsageWindows>,
    error: Option<String>,
) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        view.account = account;
        if limits.is_some() {
            view.limits = limits;
        }
        view.busy = None;
        view.error = error;
    }
    persist(state);
}

fn apply_snapshot(account: &mut Account, snapshot: &AccountSnapshot, now: i64) {
    account.email = snapshot.email.clone();
    account.plan = snapshot.plan.clone();
    account.record_usage(&snapshot.limits, now);
}

fn persist(mut state: Signal<AppState>) {
    let data = {
        let current = state.read();
        current.store.clone().map(|store| {
            let accounts = current
                .accounts
                .iter()
                .map(|view| view.account.clone())
                .collect::<Vec<_>>();
            (store, accounts, current.refresh_interval_secs)
        })
    };
    if let Some((store, accounts, refresh_interval_secs)) = data
        && let Err(error) = store.save_accounts(&accounts, refresh_interval_secs)
    {
        state.write().notice = Some(Notice {
            message: error,
            error: true,
        });
    }
}

fn set_refresh_interval(mut state: Signal<AppState>, seconds: u64) {
    state.write().refresh_interval_secs = seconds.clamp(5, DEFAULT_REFRESH_INTERVAL_SECS);
    persist(state);
}

fn toggle_automatic(mut state: Signal<AppState>, id: &str) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        view.account.enabled = !view.account.enabled;
    }
    persist(state);
}

fn toggle_workweek_lines(mut state: Signal<AppState>, id: &str, weekly_history: bool) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        let history = if weekly_history {
            &mut view.account.usage_history.weekly
        } else {
            &mut view.account.usage_history.session
        };
        history.show_workweek_lines = !history.show_workweek_lines;
    }
    persist(state);
}

fn update_draft_time(mut state: Signal<AppState>, id: &str, value: String) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        view.draft_time = value;
    }
}

fn add_time(mut state: Signal<AppState>, id: &str) {
    let result = state
        .read()
        .accounts
        .iter()
        .find(|view| view.account.id == id)
        .map(|view| DailyTime::from_str(&view.draft_time));
    match result {
        Some(Ok(time)) => {
            if let Some(view) = state
                .write()
                .accounts
                .iter_mut()
                .find(|view| view.account.id == id)
            {
                view.account.add_time(time);
                view.error = None;
            }
            persist(state);
        }
        Some(Err(error)) => {
            if let Some(view) = state
                .write()
                .accounts
                .iter_mut()
                .find(|view| view.account.id == id)
            {
                view.error = Some(error);
            }
        }
        None => {}
    }
}

fn remove_time(mut state: Signal<AppState>, id: &str, time: DailyTime) {
    if let Some(view) = state
        .write()
        .accounts
        .iter_mut()
        .find(|view| view.account.id == id)
    {
        view.account.warmup_times.retain(|value| *value != time);
    }
    persist(state);
}

fn initials(label: &str) -> String {
    label
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>()
}

fn plan_label(plan: &str) -> String {
    plan.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(char::to_uppercase)
                .into_iter()
                .flatten()
                .chain(chars)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_slot(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%a %H:%M").to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn last_activity(account: &Account) -> String {
    let Some(record) = &account.ledger.last_warmup else {
        return "No warmup sent yet".to_string();
    };
    let when = Local
        .timestamp_opt(record.at, 0)
        .single()
        .map(|time| time.format("%b %-d, %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_string());
    if record.success {
        format!("Last {} · {when}", record.reason)
    } else {
        format!("Last {} failed · {when}", record.reason)
    }
}

fn account_status(view: &AccountView, now: &chrono::DateTime<Local>) -> String {
    if let Some(action) = &view.busy {
        return action.clone();
    }
    if !view.account.connected {
        return "Sign in required".to_string();
    }
    if !view.account.enabled {
        return "Automatic warmups paused".to_string();
    }
    next_slot(now, &view.account.warmup_times)
        .map(|slot| format!("Next scheduled {}", format_slot(slot.at)))
        .unwrap_or_else(|| "Watching for reset windows".to_string())
}

fn weekly_reset_needs_probe(view: &AccountView, now: i64) -> bool {
    let Some(window) = view
        .limits
        .as_ref()
        .and_then(|limits| limits.weekly.as_ref())
    else {
        return false;
    };
    let Some(base_key) = window.key("weekly") else {
        return false;
    };
    let handled_key = format!("{base_key}:expired");
    window.resets_at <= Some(now)
        && view
            .account
            .ledger
            .retry_after
            .is_none_or(|retry| retry <= now)
        && view.account.ledger.confirmed_weekly_key.as_deref() != Some(&handled_key)
}
