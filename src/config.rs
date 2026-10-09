use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Speaker IPs seen last time, tried before any network scan.
    pub known_ips: Vec<String>,
    /// Coordinator UUID of the room that was selected on exit.
    pub last_room: Option<String>,
    pub spotify_client_id: String,
    pub spotify: Option<Tokens>,
    /// Play this PC's sound on the speakers through the Sonance output.
    pub pc_output: bool,
    /// MACs of non-Sonos Bluetooth speakers switched on for the PC output.
    pub bt_speakers: Vec<String>,
    /// Closing the window quits, instead of carrying on in the background.
    pub quit_on_close: bool,
}

fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sonance").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
                }
                let _ = std::fs::rename(tmp, p);
            }
        }
    }
}
