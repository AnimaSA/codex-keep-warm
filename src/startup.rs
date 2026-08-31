use std::{
    env,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "CodexKeepWarm";

pub fn is_enabled() -> bool {
    reg_command()
        .args(["query", RUN_KEY, "/v", VALUE_NAME])
        .status()
        .is_ok_and(|status| status.success())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let mut command = reg_command();
    if enabled {
        let executable =
            env::current_exe().map_err(|error| format!("Could not find this app: {error}"))?;
        command
            .args(["add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d"])
            .arg(startup_command(&executable))
            .arg("/f");
    } else {
        command.args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
    }

    let output = command
        .output()
        .map_err(|error| format!("Could not update Windows startup: {error}"))?;
    if output.status.success() {
        return Ok(());
    }

    let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if details.is_empty() {
        "Could not update Windows startup.".to_string()
    } else {
        format!("Could not update Windows startup: {details}")
    })
}

fn reg_command() -> Command {
    let windows = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut command = Command::new(windows.join("System32").join("reg.exe"));
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn startup_command(executable: &Path) -> String {
    format!(r#""{}" --hidden"#, executable.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_launch_is_quoted_and_hidden() {
        assert_eq!(
            startup_command(Path::new(
                r"C:\Program Files\Codex Keep Warm\codex-keep-warm.exe"
            )),
            r#""C:\Program Files\Codex Keep Warm\codex-keep-warm.exe" --hidden"#
        );
    }
}
