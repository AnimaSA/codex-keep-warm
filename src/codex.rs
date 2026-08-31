use std::{collections::VecDeque, path::Path, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

use crate::domain::{RateLimitResponse, UsageWindows};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const WARMUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Debug, Default)]
pub struct AccountSnapshot {
    pub email: Option<String>,
    pub plan: Option<String>,
    pub limits: UsageWindows,
}

#[derive(Clone, Debug, Default)]
pub struct WarmupOutcome {
    pub snapshot: Option<AccountSnapshot>,
    pub refresh_error: Option<String>,
}

struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    pending: VecDeque<Value>,
    next_id: u64,
}

impl AppServer {
    async fn start(codex_home: &Path) -> Result<Self, String> {
        let canonical_home = codex_home
            .canonicalize()
            .map_err(|error| format!("Could not open account storage: {error}"))?;
        if canonical_home != codex_home {
            return Err("Refusing to use aliased account storage".to_string());
        }
        let mut command = Command::new("codex");
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", canonical_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        hide_window(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start Codex CLI: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Codex CLI stdin was unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Codex CLI stdout was unavailable".to_string())?;
        let mut server = Self {
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            pending: VecDeque::new(),
            next_id: 1,
        };
        server
            .request(
                "initialize",
                Some(json!({
                    "clientInfo": {
                        "name": "codex_keep_warm",
                        "title": "Codex Keep Warm",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
            )
            .await?;
        server.notify("initialized", Some(json!({}))).await?;
        Ok(server)
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message).await?;

        loop {
            let message = timeout(RPC_TIMEOUT, self.read_message())
                .await
                .map_err(|_| format!("Codex timed out while handling {method}"))??;
            if message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id)
            {
                if let Some(error) = message.get("error") {
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown Codex error");
                    return Err(detail.to_string());
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("Codex returned no result for {method}"));
            }
            self.pending.push_back(message);
        }
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), String> {
        let mut message = json!({ "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message).await
    }

    async fn wait_notification(&mut self, method: &str, wait: Duration) -> Result<Value, String> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|message| message.get("method").and_then(Value::as_str) == Some(method))
        {
            return Ok(self.pending.remove(index).unwrap());
        }

        timeout(wait, async {
            loop {
                let message = self.read_message().await?;
                if message.get("method").and_then(Value::as_str) == Some(method) {
                    return Ok(message);
                }
                self.pending.push_back(message);
            }
        })
        .await
        .map_err(|_| format!("Timed out waiting for {method}"))?
    }

    async fn send(&mut self, value: &Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "Codex CLI input was closed".to_string())?;
        let mut bytes = serde_json::to_vec(value)
            .map_err(|error| format!("Could not encode Codex request: {error}"))?;
        bytes.push(b'\n');
        stdin
            .write_all(&bytes)
            .await
            .map_err(|error| format!("Could not send request to Codex: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("Could not flush request to Codex: {error}"))
    }

    async fn read_message(&mut self) -> Result<Value, String> {
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|error| format!("Could not read Codex response: {error}"))?
                .ok_or_else(|| "Codex app server stopped unexpectedly".to_string())?;
            if !line.trim().is_empty() {
                let message: Value = serde_json::from_str(&line)
                    .map_err(|error| format!("Codex returned invalid JSON: {error}"))?;
                if message.get("method").is_some()
                    && let Some(id) = message.get("id")
                {
                    self.send(&json!({
                        "id": id,
                        "error": { "code": -32601, "message": "Unsupported server request" }
                    }))
                    .await?;
                    continue;
                }
                return Ok(message);
            }
        }
    }

    async fn close(mut self) {
        self.stdin.take();
        if timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

pub async fn login(codex_home: &Path) -> Result<AccountSnapshot, String> {
    let mut server = AppServer::start(codex_home).await?;
    let result = server
        .request(
            "account/login/start",
            Some(json!({
                "type": "chatgpt",
                "useHostedLoginSuccessPage": true,
                "appBrand": "chatgpt"
            })),
        )
        .await?;
    let url = result
        .get("authUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex did not return a sign-in URL".to_string())?;
    open_url(url)?;

    let completed = server
        .wait_notification("account/login/completed", LOGIN_TIMEOUT)
        .await?;
    let params = completed
        .get("params")
        .ok_or_else(|| "Codex returned an invalid login result".to_string())?;
    if !params
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(params
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("ChatGPT sign-in failed")
            .to_string());
    }

    let snapshot = read_snapshot(&mut server).await;
    server.close().await;
    snapshot
}

// ponytail: one short-lived process per operation; keep one server per account if startup load matters.
pub async fn fetch_snapshot(codex_home: &Path) -> Result<AccountSnapshot, String> {
    let mut server = AppServer::start(codex_home).await?;
    let snapshot = read_snapshot(&mut server).await;
    server.close().await;
    snapshot
}

pub async fn warm_and_fetch(codex_home: &Path, workspace: &Path) -> Result<WarmupOutcome, String> {
    let mut server = AppServer::start(codex_home).await?;
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("Could not open the warmup workspace: {error}"))?;
    if canonical_workspace != workspace {
        return Err("Refusing to use an aliased warmup workspace".to_string());
    }
    let thread = server
        .request(
            "thread/start",
            Some(json!({
                "cwd": canonical_workspace,
                "ephemeral": true,
                "approvalPolicy": "never",
                "sandbox": "read-only",
                "baseInstructions": "Reply exactly OK. Do not use tools.",
                "developerInstructions": "",
                "config": {
                    "model_reasoning_effort": "low",
                    "model_verbosity": "low"
                }
            })),
        )
        .await?;
    let thread_id = thread
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex did not create the warmup thread".to_string())?;
    server
        .request(
            "turn/start",
            Some(json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": "Reply exactly OK." }],
                "effort": "low",
                "summary": "none"
            })),
        )
        .await?;

    let completed = server
        .wait_notification("turn/completed", WARMUP_TIMEOUT)
        .await?;
    let turn = completed
        .pointer("/params/turn")
        .ok_or_else(|| "Codex returned an invalid warmup result".to_string())?;
    if turn.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(turn
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Warmup request failed")
            .to_string());
    }

    let (snapshot, refresh_error) = match read_snapshot(&mut server).await {
        Ok(snapshot) => (Some(snapshot), None),
        Err(error) => (None, Some(error)),
    };
    server.close().await;
    Ok(WarmupOutcome {
        snapshot,
        refresh_error,
    })
}

pub async fn logout(codex_home: &Path) -> Result<(), String> {
    let mut server = AppServer::start(codex_home).await?;
    let result = server.request("account/logout", None).await.map(|_| ());
    server.close().await;
    result
}

async fn read_snapshot(server: &mut AppServer) -> Result<AccountSnapshot, String> {
    let account = server
        .request("account/read", Some(json!({ "refreshToken": false })))
        .await?;
    let account = account
        .get("account")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "This account is not signed in".to_string())?;
    if account.get("type").and_then(Value::as_str) != Some("chatgpt") {
        return Err("This app needs a ChatGPT account".to_string());
    }

    let limits: RateLimitResponse =
        serde_json::from_value(server.request("account/rateLimits/read", None).await?)
            .map_err(|error| format!("Could not understand Codex limits: {error}"))?;
    Ok(AccountSnapshot {
        email: account
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        plan: account
            .get("planType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limits: UsageWindows::from_response(limits),
    })
}

fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("Codex returned an unsafe sign-in URL".to_string());
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the sign-in page: {error}"))
}

#[cfg(target_os = "windows")]
fn hide_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_window(_: &mut Command) {}
