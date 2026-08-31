use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::domain::{Account, AppConfig};

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
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
        Ok(Self { root })
    }

    pub fn load(&self) -> Result<AppConfig, String> {
        let path = self.settings_path();
        if !path.exists() {
            return Ok(AppConfig {
                version: 1,
                accounts: Vec::new(),
            });
        }
        let bytes = fs::read(&path).map_err(|error| format!("Could not read settings: {error}"))?;
        serde_json::from_slice(&bytes).map_err(|error| format!("Settings are invalid: {error}"))
    }

    pub fn save_accounts(&self, accounts: &[Account]) -> Result<(), String> {
        let config = AppConfig {
            version: 1,
            accounts: accounts.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&config)
            .map_err(|error| format!("Could not serialize settings: {error}"))?;
        fs::write(self.settings_path(), bytes)
            .map_err(|error| format!("Could not save settings: {error}"))
    }

    pub fn account_home(&self, id: &str) -> PathBuf {
        self.root.join("accounts").join(id).join("codex-home")
    }

    pub fn warmup_workspace(&self, id: &str) -> PathBuf {
        self.root.join("accounts").join(id).join("warmup")
    }

    pub fn prepare_account(&self, id: &str) -> Result<(), String> {
        let home = self.account_home(id);
        fs::create_dir_all(&home)
            .and_then(|_| fs::create_dir_all(self.warmup_workspace(id)))
            .map_err(|error| format!("Could not create account storage: {error}"))?;
        let config = home.join("config.toml");
        if !config.exists() {
            fs::write(config, "cli_auth_credentials_store = \"keyring\"\n")
                .map_err(|error| format!("Could not configure account storage: {error}"))?;
        }
        Ok(())
    }

    pub fn remove_account(&self, id: &str) -> Result<(), String> {
        let path = self.root.join("accounts").join(id);
        if path.exists() {
            fs::remove_dir_all(path)
                .map_err(|error| format!("Could not remove account credentials: {error}"))?;
        }
        Ok(())
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }
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
    use super::*;

    #[test]
    fn round_trips_metadata_without_touching_auth_cache() {
        let root = env::temp_dir().join(format!("codex-keep-warm-test-{}", new_account_id()));
        let store = Store::new(root.clone()).unwrap();
        let account = Account::new("account-1".into(), "Personal".into());
        store.prepare_account(&account.id).unwrap();
        store.save_accounts(std::slice::from_ref(&account)).unwrap();
        assert_eq!(store.load().unwrap().accounts[0].label, "Personal");
        assert!(store.account_home(&account.id).join("config.toml").exists());
        assert!(!store.account_home(&account.id).join("auth.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
