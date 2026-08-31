#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

mod codex;
mod domain;
#[cfg(target_os = "windows")]
mod startup;
mod storage;

use std::str::FromStr;

use chrono::{Local, TimeZone};
use dioxus::desktop::{
    Config, WindowBuilder, WindowCloseBehaviour,
    trayicon::{default_tray_icon, init_tray_icon},
};
use dioxus::prelude::*;
use tokio::time::{Duration, sleep};

use codex::AccountSnapshot;
use domain::{
    Account, DEFAULT_REFRESH_INTERVAL_SECS, DailyTime, LimitWindow, UsageWindows, WarmReason,
    format_reset, next_slot, plan_warmup, schedule_gap_warning,
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
        loop {
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
            sleep(Duration::from_secs(1)).await;
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
                                max: "3600",
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
                                            now: *now.read()
                                        }
                                        LimitPanel {
                                            label: "Weekly".to_string(),
                                            window: limits.weekly,
                                            now: *now.read()
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
fn LimitPanel(label: String, window: Option<LimitWindow>, now: i64) -> Element {
    if let Some(window) = window {
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
        rsx! {
            section { class: "limit-panel",
                div { class: "limit-top",
                    span { "{label}" }
                    strong { "{remaining}%" }
                }
                div {
                    class: "progress-track",
                    role: "progressbar",
                    aria_label: "{label} remaining",
                    aria_valuemin: "0",
                    aria_valuemax: "100",
                    aria_valuenow: "{remaining}",
                    div { class: "{progress_class}", style: "width: {remaining}%" }
                }
                div { class: "limit-meta",
                    span { "Remaining" }
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
        }
    } else {
        rsx! {
            section { class: "limit-panel unavailable",
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
            apply_snapshot(&mut account, &snapshot);
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
    apply_snapshot(&mut account, &initial);
    account.connected = true;
    let decision_time = Local::now();
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
                    apply_snapshot(&mut account, snapshot);
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
                apply_snapshot(&mut account, snapshot);
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

fn apply_snapshot(account: &mut Account, snapshot: &AccountSnapshot) {
    account.email = snapshot.email.clone();
    account.plan = snapshot.plan.clone();
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
    state.write().refresh_interval_secs = seconds.clamp(5, 3600);
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
