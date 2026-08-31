use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::domain::{Account, AppConfig};

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    _instance_lock: Arc<fs::File>,
}

impl Store {
    pub fn discover() -> Result<Self, String> {
        if let Some(path) = env::var_os("CODEX_KEEP_WARM_HOME") {
            return Self::new(path.into());
        }

        #[cfg(target_os = "windows")]
        let root = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("CodexKeepWarm"));

        #[cfg(target_os = "macos")]
        let root = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join("Library/Application Support/CodexKeepWarm"));

        #[cfg(all(unix, not(target_os = "macos")))]
        let root = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|path| path.join("codex-keep-warm"));

        Self::new(root.ok_or_else(|| "Could not find the local app-data directory".to_string())?)
    }

    pub fn new(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(root.join("accounts"))
            .map_err(|error| format!("Could not create app data: {error}"))?;
        let root = root
            .canonicalize()
            .map_err(|error| format!("Could not verify app data: {error}"))?;
        verify_directory(&root.join("accounts"))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("instance.lock"))
            .map_err(|error| format!("Could not open the app lock: {error}"))?;
        lock.try_lock()
            .map_err(|_| "Codex Keep Warm is already running for this profile".to_string())?;
        Ok(Self {
            root,
            _instance_lock: Arc::new(lock),
        })
    }

    pub fn load(&self) -> Result<AppConfig, String> {
        let settings = self.settings_path();
        let temporary = settings.with_extension("json.tmp");
        let backup = settings.with_extension("json.bak");
        let path = [&settings, &temporary, &backup]
            .into_iter()
            .find(|path| path.exists());
        let Some(path) = path else {
            return Ok(AppConfig::default());
        };
        let bytes = fs::read(path).map_err(|error| format!("Could not read settings: {error}"))?;
        let mut config: AppConfig = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Settings are invalid: {error}"))?;
        config.refresh_interval_secs = config.refresh_interval_secs.clamp(5, 3600);
        let mut ids = HashSet::new();
        for account in &mut config.accounts {
            validate_account_id(&account.id)?;
            if !ids.insert(&account.id) {
                return Err("Settings contain a duplicate account ID".to_string());
            }
            account.warmup_times.sort_unstable();
            account.warmup_times.dedup();
        }
        Ok(config)
    }

    pub fn save_accounts(
        &self,
        accounts: &[Account],
        refresh_interval_secs: u64,
    ) -> Result<(), String> {
        let config = AppConfig {
            version: 1,
            refresh_interval_secs,
            accounts: accounts.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&config)
            .map_err(|error| format!("Could not serialize settings: {error}"))?;
        let settings = self.settings_path();
        let temporary = settings.with_extension("json.tmp");
        let backup = settings.with_extension("json.bak");
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("Could not save settings: {error}"))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("Could not save settings: {error}"))?;
        drop(file);

        if settings.exists() {
            if backup.exists() {
                fs::remove_file(&backup)
                    .map_err(|error| format!("Could not rotate settings: {error}"))?;
            }
            fs::rename(&settings, &backup)
                .map_err(|error| format!("Could not rotate settings: {error}"))?;
        }
        if let Err(error) = fs::rename(&temporary, &settings) {
            if backup.exists() {
                let _ = fs::rename(&backup, &settings);
            }
            return Err(format!("Could not replace settings: {error}"));
        }
        if backup.exists() {
            let _ = fs::remove_file(backup);
        }
        Ok(())
    }

    pub fn account_home(&self, id: &str) -> PathBuf {
        self.root.join("accounts").join(id).join("codex-home")
    }

    pub fn warmup_workspace(&self, id: &str) -> PathBuf {
        self.root.join("accounts").join(id).join("warmup")
    }

    pub fn prepare_account(&self, id: &str) -> Result<(), String> {
        validate_account_id(id)?;
        let account = self.root.join("accounts").join(id);
        if !account.exists() {
            fs::create_dir(&account)
                .map_err(|error| format!("Could not create account storage: {error}"))?;
        }
        verify_directory(&account)?;
        let home = self.account_home(id);
        let workspace = self.warmup_workspace(id);
        for path in [&home, &workspace] {
            if !path.exists() {
                fs::create_dir(path)
                    .map_err(|error| format!("Could not create account storage: {error}"))?;
            }
            verify_directory(path)?;
        }
        let config = home.join("config.toml");
        if !config.exists() {
            fs::write(config, "cli_auth_credentials_store = \"keyring\"\n")
                .map_err(|error| format!("Could not configure account storage: {error}"))?;
        }
        Ok(())
    }

    pub fn remove_account(&self, id: &str) -> Result<(), String> {
        validate_account_id(id)?;
        let accounts = self
            .root
            .join("accounts")
            .canonicalize()
            .map_err(|error| format!("Could not verify account storage: {error}"))?;
        let path = self.root.join("accounts").join(id);
        if path.exists() {
            let resolved = path
                .canonicalize()
                .map_err(|error| format!("Could not verify account storage: {error}"))?;
            if resolved != accounts.join(id)
                || fs::symlink_metadata(&path)
                    .map_err(|error| format!("Could not verify account storage: {error}"))?
                    .file_type()
                    .is_symlink()
            {
                return Err(
                    "Refusing to remove account storage outside the app directory".to_string(),
                );
            }
            fs::remove_dir_all(&path)
                .map_err(|error| format!("Could not remove account credentials: {error}"))?;
        }
        Ok(())
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }
}

fn validate_account_id(id: &str) -> Result<(), String> {
    ((1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value)))
    .then_some(())
    .ok_or_else(|| "Settings contain an invalid account ID".to_string())
}

fn verify_directory(path: &Path) -> Result<(), String> {
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("Could not verify app storage: {error}"))?;
    let is_symlink = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not verify app storage: {error}"))?
        .file_type()
        .is_symlink();
    if resolved != path || is_symlink {
        return Err("Refusing to use aliased app storage".to_string());
    }
    Ok(())
}

pub fn new_account_id() -> String {
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[cfg(test)]
mod tests {
    use crate::domain::{LimitWindow, UsageWindows};

    use super::*;

    #[test]
    fn round_trips_metadata_without_touching_auth_cache() {
        let root = env::temp_dir().join(format!("codex-keep-warm-test-{}", new_account_id()));
        let store = Store::new(root.clone()).unwrap();
        let mut account = Account::new("abc001".into(), "Personal".into());
        account.record_usage(
            &UsageWindows {
                session: Some(LimitWindow {
                    used_percent: 20,
                    window_duration_mins: Some(300),
                    resets_at: Some(20_000),
                }),
                weekly: None,
            },
            10_000,
        );
        account.usage_history.weekly.show_workweek_lines = true;
        store.prepare_account(&account.id).unwrap();
        store
            .save_accounts(std::slice::from_ref(&account), 45)
            .unwrap();
        assert_eq!(store.load().unwrap().accounts[0].label, "Personal");
        assert!(
            store.load().unwrap().accounts[0]
                .usage_history
                .weekly
                .show_workweek_lines
        );
        assert_eq!(
            store.load().unwrap().accounts[0]
                .usage_history
                .session
                .points
                .len(),
            1
        );
        assert_eq!(store.load().unwrap().refresh_interval_secs, 45);
        assert!(store.account_home(&account.id).join("config.toml").exists());
        assert!(!store.account_home(&account.id).join("auth.json").exists());
        assert!(validate_account_id("../../target").is_err());
        assert!(Store::new(root.clone()).is_err());
        let settings = store.settings_path();
        fs::rename(&settings, settings.with_extension("json.bak")).unwrap();
        assert_eq!(store.load().unwrap().accounts[0].label, "Personal");
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
