use std::{
    fs,
    path::{Path, PathBuf},
    process::{ExitStatus, Output, Stdio},
    time::Duration,
};

use chrono::DateTime;
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderValue},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::Semaphore,
    time::timeout,
};

use crate::domain::{
    AccountSnapshot, LimitWindow, SESSION_MINUTES, UsageWindows, WEEK_MINUTES, WarmupOutcome,
};

const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_USAGE_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(30);
const WARMUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const WARMUP_PROMPT: &str = "Reply exactly OK.";
const OAUTH_USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_VERSION: &str = "oauth-2025-04-20";
const REQUIRED_OAUTH_SCOPE: &str = "user:profile";
const OAUTH_OVERRIDING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_AWS_WORKSPACE_ID",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_IDENTITY_TOKEN",
    "ANTHROPIC_IDENTITY_TOKEN_FILE",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_SERVICE_ACCOUNT_ID",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_WORKSPACE_ID",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_SKIP_MANTLE_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
];

const FIRST_PARTY_AUTH_METHODS: &[&str] = &["claude.ai", "oauth_token"];
const FIRST_PARTY_API_PROVIDER: &str = "firstParty";

// Serialize short-lived CLI processes to cap peak memory and avoid auth races.
static CLAUDE_PROCESS: Semaphore = Semaphore::const_new(1);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    logged_in: bool,
    email: Option<String>,
    subscription_type: Option<String>,
    auth_method: Option<String>,
    api_provider: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeCredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAiOauth {
    access_token: String,
    scopes: Option<Vec<String>>,
    subscription_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthUsageResponse {
    five_hour: Option<OAuthUsageWindow>,
    seven_day: Option<OAuthUsageWindow>,
}

#[derive(Debug, Deserialize)]
struct OAuthUsageWindow {
    utilization: f64,
    resets_at: Option<String>,
}

pub async fn login(home: &Path) -> Result<AccountSnapshot, String> {
    let canonical_home = canonical_home(home)?;
    let output = run_cli(
        &canonical_home,
        None,
        &["auth", "login"],
        Stdio::inherit(),
        true,
        LOGIN_TIMEOUT,
        "auth login",
    )
    .await?;
    require_success(&output, "auth login")?;
    fetch_snapshot(&canonical_home).await
}

pub async fn fetch_snapshot(home: &Path) -> Result<AccountSnapshot, String> {
    let canonical_home = canonical_home(home)?;
    let output = run_cli(
        &canonical_home,
        None,
        &["auth", "status"],
        Stdio::null(),
        false,
        STATUS_TIMEOUT,
        "auth status",
    )
    .await?;
    let status = match parse_auth_status(&output.stdout) {
        Ok(status) => status,
        Err(_error) if !output.status.success() => {
            return Err(command_error(&output, "auth status"));
        }
        Err(error) => return Err(error),
    };
    if !output.status.success() {
        return if !status.logged_in {
            Err("This account is not signed in".to_string())
        } else {
            Err(command_error(&output, "auth status"))
        };
    }
    if !status.logged_in {
        return Err("This account is not signed in".to_string());
    }
    validate_auth_status(&status)?;

    let credentials = read_credentials(&canonical_home)?;
    let limits = fetch_oauth_usage(&credentials.access_token).await?;
    snapshot_from_status_and_limits(status, limits, credentials.subscription_type)
}

fn read_credentials(home: &Path) -> Result<ClaudeAiOauth, String> {
    let path = home.join(".credentials.json");
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Claude OAuth credentials file is missing; sign in with Claude first".to_string()
        } else {
            format!("Could not read Claude OAuth credentials: {error}")
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err("Refusing to use aliased Claude OAuth credentials".to_string());
    }
    if !metadata.is_file() {
        return Err("Claude OAuth credentials path is not a regular file".to_string());
    }

    let canonical_path = path.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Claude OAuth credentials file is missing; sign in with Claude first".to_string()
        } else {
            format!("Could not verify Claude OAuth credentials: {error}")
        }
    })?;
    if canonical_path != path {
        return Err("Refusing to use aliased Claude OAuth credentials".to_string());
    }

    let bytes = fs::read(&canonical_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Claude OAuth credentials file is missing; sign in with Claude first".to_string()
        } else {
            format!("Could not read Claude OAuth credentials: {error}")
        }
    })?;
    parse_credentials(&bytes)
}

fn parse_credentials(bytes: &[u8]) -> Result<ClaudeAiOauth, String> {
    let credentials: ClaudeCredentialsFile = serde_json::from_slice(bytes)
        .map_err(|_| "Claude OAuth credentials are invalid".to_string())?;
    let oauth = credentials
        .claude_ai_oauth
        .ok_or_else(|| "Claude OAuth credentials do not contain claudeAiOauth".to_string())?;
    if oauth.access_token.trim().is_empty() {
        return Err("Claude OAuth credentials do not contain an access token".to_string());
    }
    if oauth
        .scopes
        .as_ref()
        .is_some_and(|scopes| !scopes.iter().any(|scope| scope == REQUIRED_OAUTH_SCOPE))
    {
        return Err("Claude OAuth token lacks required user:profile scope".to_string());
    }
    Ok(oauth)
}

async fn fetch_oauth_usage(access_token: &str) -> Result<UsageWindows, String> {
    let authorization = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|_| "Claude OAuth access token is invalid".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(OAUTH_USAGE_TIMEOUT)
        .build()
        .map_err(|_| "Could not prepare Claude OAuth usage request".to_string())?;
    let response = client
        .get(OAUTH_USAGE_ENDPOINT)
        .header(AUTHORIZATION, authorization)
        .header("anthropic-beta", OAUTH_BETA_VERSION)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                format!(
                    "Claude OAuth usage request timed out after {} seconds",
                    OAUTH_USAGE_TIMEOUT.as_secs()
                )
            } else {
                format!("Could not fetch Claude OAuth usage: {error}")
            }
        })?;
    if !response.status().is_success() {
        return Err(oauth_usage_status_error(response.status()));
    }
    let usage = response
        .json::<OAuthUsageResponse>()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                format!(
                    "Claude OAuth usage request timed out after {} seconds",
                    OAUTH_USAGE_TIMEOUT.as_secs()
                )
            } else {
                "Claude OAuth usage response was invalid; endpoint may have changed".to_string()
            }
        })?;
    usage_windows_from_response(usage)
}

fn oauth_usage_status_error(status: StatusCode) -> String {
    let code = status.as_u16();
    match status {
        StatusCode::UNAUTHORIZED => {
            format!("Claude OAuth usage unauthorized (HTTP {code}); sign in again")
        }
        StatusCode::FORBIDDEN => format!(
            "Claude OAuth usage forbidden (HTTP {code}); verify token has {REQUIRED_OAUTH_SCOPE} scope"
        ),
        StatusCode::NOT_FOUND => {
            format!(
                "Claude OAuth usage endpoint unavailable (HTTP {code}); endpoint may have changed"
            )
        }
        _ if status.is_server_error() => {
            format!("Claude OAuth usage service failed (HTTP {code}); retry later")
        }
        _ => format!("Claude OAuth usage request failed with HTTP status {code}"),
    }
}

#[cfg(test)]
fn parse_usage_response(bytes: &[u8]) -> Result<UsageWindows, String> {
    let response: OAuthUsageResponse = serde_json::from_slice(bytes).map_err(|_| {
        "Claude OAuth usage response was invalid; endpoint may have changed".to_string()
    })?;
    usage_windows_from_response(response)
}

fn usage_windows_from_response(response: OAuthUsageResponse) -> Result<UsageWindows, String> {
    let session = response
        .five_hour
        .map(|window| map_usage_window(window, "five_hour", SESSION_MINUTES))
        .transpose()?;
    let weekly = response
        .seven_day
        .map(|window| map_usage_window(window, "seven_day", WEEK_MINUTES))
        .transpose()?;
    Ok(UsageWindows {
        session,
        weekly,
        banked_resets: None,
    })
}

fn map_usage_window(
    window: OAuthUsageWindow,
    name: &str,
    duration_mins: i64,
) -> Result<LimitWindow, String> {
    if !window.utilization.is_finite() {
        return Err(format!(
            "Claude OAuth usage returned invalid {name} utilization"
        ));
    }
    let resets_at = window
        .resets_at
        .as_deref()
        .map(|value| parse_reset_timestamp(value, name))
        .transpose()?;
    Ok(LimitWindow {
        used_percent: window.utilization.round().clamp(0.0, 100.0) as i32,
        window_duration_mins: Some(duration_mins),
        resets_at,
    })
}

fn parse_reset_timestamp(value: &str, name: &str) -> Result<i64, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp())
        .map_err(|_| format!("Claude OAuth usage returned invalid {name} reset timestamp"))
}

pub async fn warm_and_fetch(home: &Path, workspace: &Path) -> Result<WarmupOutcome, String> {
    let canonical_home = canonical_home(home)?;
    let canonical_workspace = canonical_workspace(workspace)?;
    let output = run_cli(
        &canonical_home,
        Some(&canonical_workspace),
        &[
            "--safe-mode",
            "--no-session-persistence",
            "--tools",
            "",
            "--print",
            WARMUP_PROMPT,
            "--output-format",
            "text",
        ],
        Stdio::null(),
        false,
        WARMUP_TIMEOUT,
        "warmup",
    )
    .await?;
    require_success(&output, "warmup")?;
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err("Claude warmup returned no output".to_string());
    }

    let (snapshot, refresh_error) = match fetch_snapshot(&canonical_home).await {
        Ok(snapshot) => (Some(snapshot), None),
        Err(error) => (None, Some(error)),
    };
    Ok(WarmupOutcome {
        snapshot,
        refresh_error,
    })
}

pub async fn logout(home: &Path) -> Result<(), String> {
    let canonical_home = canonical_home(home)?;
    let output = run_cli(
        &canonical_home,
        None,
        &["auth", "logout"],
        Stdio::null(),
        false,
        LOGOUT_TIMEOUT,
        "auth logout",
    )
    .await?;
    require_success(&output, "auth logout")
}

async fn run_cli(
    home: &Path,
    workspace: Option<&Path>,
    args: &[&str],
    stdin: Stdio,
    interactive: bool,
    limit: Duration,
    operation: &str,
) -> Result<Output, String> {
    let _permit = CLAUDE_PROCESS
        .acquire()
        .await
        .map_err(|_| "Claude process queue closed".to_string())?;
    let mut command = Command::new("claude");
    command
        .args(args)
        .env("CLAUDE_CONFIG_DIR", home)
        .current_dir(workspace.unwrap_or(home))
        .kill_on_drop(true);
    if interactive {
        command
            .stdin(stdin)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        show_window(&mut command);
    } else {
        command
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_window(&mut command);
    }
    for &variable in OAUTH_OVERRIDING_ENV {
        command.env_remove(variable);
    }

    let child = command
        .spawn()
        .map_err(|error| format!("Could not start Claude CLI for {operation}: {error}"))?;
    wait_for_output(child, limit, operation).await
}

async fn wait_for_output(
    mut child: Child,
    limit: Duration,
    operation: &str,
) -> Result<Output, String> {
    let stdout_task = tokio::spawn(read_output(child.stdout.take()));
    let stderr_task = tokio::spawn(read_output(child.stderr.take()));
    let status = match timeout(limit, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(format!(
                "Claude {operation} timed out after {} seconds",
                limit.as_secs()
            ));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| format!("Could not read Claude {operation} output: {error}"))?
        .map_err(|error| format!("Could not read Claude {operation} output: {error}"))?;
    let stderr = stderr_task
        .await
        .map_err(|error| format!("Could not read Claude {operation} output: {error}"))?
        .map_err(|error| format!("Could not read Claude {operation} output: {error}"))?;
    let status =
        status.map_err(|error| format!("Could not read Claude {operation} output: {error}"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
async fn read_output<R>(reader: Option<R>) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    reader.read_to_end(&mut output).await?;
    Ok(output)
}

fn parse_auth_status(stdout: &[u8]) -> Result<AuthStatus, String> {
    serde_json::from_slice(stdout)
        .map_err(|error| format!("Could not understand Claude auth status: {error}"))
}

#[cfg(test)]
fn snapshot_from_status(status: AuthStatus) -> Result<AccountSnapshot, String> {
    snapshot_from_status_and_limits(status, UsageWindows::default(), None)
}

fn snapshot_from_status_and_limits(
    status: AuthStatus,
    limits: UsageWindows,
    credential_subscription_type: Option<String>,
) -> Result<AccountSnapshot, String> {
    validate_auth_status(&status)?;
    Ok(AccountSnapshot {
        email: status.email,
        plan: status.subscription_type.or(credential_subscription_type),
        limits,
    })
}

fn validate_auth_status(status: &AuthStatus) -> Result<(), String> {
    let valid_auth_method = status
        .auth_method
        .as_deref()
        .is_some_and(|method| FIRST_PARTY_AUTH_METHODS.contains(&method));
    if !valid_auth_method || status.api_provider.as_deref() != Some(FIRST_PARTY_API_PROVIDER) {
        return Err(format!(
            "Claude account uses unsupported authentication (authMethod={}, apiProvider={}); expected authMethod=claude.ai or oauth_token and apiProvider={FIRST_PARTY_API_PROVIDER}",
            status.auth_method.as_deref().unwrap_or("<missing>"),
            status.api_provider.as_deref().unwrap_or("<missing>"),
        ));
    }
    Ok(())
}

fn require_success(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(output, operation))
    }
}

fn command_error(output: &Output, operation: &str) -> String {
    let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if details.is_empty() {
        format!(
            "Claude {operation} failed with exit status {}",
            exit_status(&output.status)
        )
    } else {
        format!("Claude {operation} failed: {details}")
    }
}

fn exit_status(status: &ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string())
}

fn canonical_home(home: &Path) -> Result<PathBuf, String> {
    let canonical_home = home
        .canonicalize()
        .map_err(|error| format!("Could not open Claude account storage: {error}"))?;
    if canonical_home != home {
        return Err("Refusing to use an aliased Claude account storage".to_string());
    }
    Ok(canonical_home)
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, String> {
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("Could not open the Claude warmup workspace: {error}"))?;
    if canonical_workspace != workspace {
        return Err("Refusing to use an aliased Claude warmup workspace".to_string());
    }
    Ok(canonical_workspace)
}

#[cfg(target_os = "windows")]
fn hide_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_window(_: &mut Command) {}

#[cfg(target_os = "windows")]
fn show_window(command: &mut Command) {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    command.creation_flags(CREATE_NEW_CONSOLE);
}

#[cfg(not(target_os = "windows"))]
fn show_window(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_status(auth_method: &str, api_provider: &str) -> AuthStatus {
        let payload = serde_json::json!({
            "loggedIn": true,
            "email": "user@example.com",
            "subscriptionType": "pro",
            "authMethod": auth_method,
            "apiProvider": api_provider,
        })
        .to_string();
        parse_auth_status(payload.as_bytes()).expect("valid Claude auth status")
    }

    fn snapshot(auth_method: &str, api_provider: &str) -> Result<AccountSnapshot, String> {
        snapshot_from_status(auth_status(auth_method, api_provider))
    }

    fn credentials(payload: serde_json::Value) -> Result<ClaudeAiOauth, String> {
        let payload = payload.to_string();
        parse_credentials(payload.as_bytes())
    }

    #[test]
    fn accepts_first_party_claude_ai_auth() {
        let snapshot = snapshot("claude.ai", "firstParty").expect("first-party auth");

        assert_eq!(snapshot.email.as_deref(), Some("user@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.limits, UsageWindows::default());
    }

    #[test]
    fn accepts_first_party_oauth_token_auth() {
        let snapshot = snapshot("oauth_token", "firstParty").expect("first-party auth");

        assert_eq!(snapshot.email.as_deref(), Some("user@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.limits, UsageWindows::default());
    }

    #[test]
    fn rejects_api_key_auth() {
        let error = snapshot("api_key", "firstParty").expect_err("API key auth must be rejected");

        assert!(error.contains("unsupported authentication"), "{error}");
    }

    #[test]
    fn rejects_non_first_party_auth_provider() {
        let error =
            snapshot("claude.ai", "thirdParty").expect_err("third-party auth must be rejected");

        assert!(error.contains("unsupported authentication"), "{error}");
    }

    #[test]
    fn accepts_oauth_credentials_without_scopes() {
        let credentials = credentials(serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "oauth-token",
                "subscriptionType": "pro",
            }
        }))
        .expect("credentials without scopes");

        assert_eq!(credentials.access_token, "oauth-token");
        assert_eq!(credentials.subscription_type.as_deref(), Some("pro"));
        assert!(credentials.scopes.is_none());
    }

    #[test]
    fn accepts_oauth_credentials_with_required_scope() {
        let credentials = credentials(serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "oauth-token",
                "scopes": ["user:profile"],
            }
        }))
        .expect("credentials with required scope");

        assert_eq!(
            credentials.scopes.expect("scope list"),
            vec!["user:profile".to_string()]
        );
    }

    #[test]
    fn rejects_oauth_credentials_without_required_scope() {
        let error = credentials(serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "oauth-token",
                "scopes": ["user:inference"],
            }
        }))
        .err()
        .expect("credentials without profile scope must be rejected");

        assert!(error.contains("user:profile"), "{error}");
        assert!(!error.contains("oauth-token"), "{error}");
    }

    #[test]
    fn maps_and_clamps_oauth_usage_windows() {
        let windows = parse_usage_response(
            br#"{
                "five_hour": {
                    "utilization": 42.5,
                    "resets_at": "2026-09-12T10:15:30+02:00"
                },
                "seven_day": {
                    "utilization": 150.4,
                    "resets_at": null
                }
            }"#,
        )
        .expect("valid OAuth usage");
        let expected_reset = DateTime::parse_from_rfc3339("2026-09-12T10:15:30+02:00")
            .expect("valid test reset")
            .timestamp();

        let session = windows.session.as_ref().expect("session window");
        assert_eq!(session.used_percent, 43);
        assert_eq!(session.window_duration_mins, Some(SESSION_MINUTES));
        assert_eq!(session.resets_at, Some(expected_reset));
        let weekly = windows.weekly.as_ref().expect("weekly window");
        assert_eq!(weekly.used_percent, 100);
        assert_eq!(weekly.window_duration_mins, Some(WEEK_MINUTES));
        assert_eq!(weekly.resets_at, None);

        let low = map_usage_window(
            OAuthUsageWindow {
                utilization: -2.5,
                resets_at: None,
            },
            "five_hour",
            SESSION_MINUTES,
        )
        .expect("finite utilization");
        assert_eq!(low.used_percent, 0);
    }

    #[test]
    fn rejects_malformed_oauth_reset_timestamp() {
        let error = parse_usage_response(
            br#"{
                "five_hour": {
                    "utilization": 1,
                    "resets_at": "not-rfc3339"
                }
            }"#,
        )
        .expect_err("malformed reset must fail");

        assert!(error.contains("five_hour reset timestamp"), "{error}");
        assert!(!error.contains("not-rfc3339"), "{error}");
    }

    #[test]
    fn treats_absent_and_null_oauth_windows_as_empty() {
        assert_eq!(
            parse_usage_response(br#"{}"#).expect("empty usage"),
            UsageWindows::default()
        );
        assert_eq!(
            parse_usage_response(
                br#"{"five_hour":null,"seven_day":null,"seven_day_opus":{"utilization":"ignored"},"extra_usage":{"is_enabled":true}}"#,
            )
            .expect("null usage"),
            UsageWindows::default()
        );
    }
}
