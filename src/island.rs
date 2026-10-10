//! The top-bar island: checking that this desktop can show it, and turning
//! the GNOME Shell extension on and off. The extension's files are built into
//! Sonance, so turning it on installs the version that matches this build.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use zbus::zvariant::OwnedValue;

pub const UUID: &str = "sonance-island@sonance.dev";
const FILES: [(&str, &str); 3] = [
    ("metadata.json", include_str!("../extension/sonance-island@sonance.dev/metadata.json")),
    ("extension.js", include_str!("../extension/sonance-island@sonance.dev/extension.js")),
    ("stylesheet.css", include_str!("../extension/sonance-island@sonance.dev/stylesheet.css")),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Support {
    /// GNOME Shell of a version the extension declares.
    Supported { version: String },
    /// Not GNOME at all; holds the desktop's name.
    NotGnome(String),
    Unsupported { version: String, supported: Vec<String> },
    /// GNOME's "Extensions" switch is off: no user extension runs.
    ExtensionsOff { version: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Off,
    On {
        /// The installed files are from another Sonance build.
        outdated: bool,
    },
    /// Turned on, but GNOME only loads new extensions at login (Wayland).
    NeedsLogin,
    Error(String),
}

fn dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("gnome-shell/extensions").join(UUID)
}

/// The GNOME versions the extension's metadata.json lists.
fn supported_versions() -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(FILES[0].1)
        .ok()
        .and_then(|v| v["shell-version"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()))
        .unwrap_or_default()
}

fn installed_matches() -> bool {
    FILES.iter().all(|(name, body)| std::fs::read_to_string(dir().join(name)).is_ok_and(|s| s == *body))
}

fn installed() -> bool {
    dir().join("metadata.json").exists()
}

async fn gsettings(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("gsettings").args(args).output().await.context("gsettings isn't available")?;
    if !out.status.success() {
        bail!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Parses a GVariant string array as printed by gsettings: `['a', 'b']` or `@as []`.
fn parse_list(s: &str) -> Vec<String> {
    s.split('\'').skip(1).step_by(2).map(String::from).collect()
}

fn format_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", "))
}

async fn edit_list(key: &str, add: bool) -> Result<()> {
    let mut list = parse_list(&gsettings(&["get", "org.gnome.shell", key]).await?);
    let has = list.iter().any(|u| u == UUID);
    if add == has {
        return Ok(());
    }
    if add {
        list.push(UUID.into());
    } else {
        list.retain(|u| u != UUID);
    }
    gsettings(&["set", "org.gnome.shell", key, &format_list(&list)]).await.map(|_| ())
}

async fn shell() -> Result<zbus::Connection> {
    Ok(zbus::Connection::session().await?)
}

async fn shell_version(c: &zbus::Connection) -> Option<String> {
    let r = c.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), "Get", &("org.gnome.Shell", "ShellVersion")).await.ok()?;
    let v: OwnedValue = r.body().deserialize().ok()?;
    String::try_from(v).ok()
}

async fn ext_call(c: &zbus::Connection, method: &str, uuid: &str) -> Result<zbus::Message> {
    Ok(c.call_method(Some("org.gnome.Shell.Extensions"), "/org/gnome/Shell/Extensions", Some("org.gnome.Shell.Extensions"), method, &(uuid,)).await?)
}

/// What GNOME Shell reports for the extension; empty if it hasn't loaded it.
async fn info(c: &zbus::Connection) -> std::collections::HashMap<String, OwnedValue> {
    match ext_call(c, "GetExtensionInfo", UUID).await {
        Ok(m) => m.body().deserialize().unwrap_or_default(),
        Err(_) => Default::default(),
    }
}

pub async fn check() -> (Support, State) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let Ok(c) = shell().await else { return (Support::NotGnome(desktop), State::Off) };
    let Some(version) = shell_version(&c).await else {
        let name = if desktop.is_empty() { "this desktop".to_string() } else { desktop };
        return (Support::NotGnome(name), State::Off);
    };
    let major = version.split('.').next().unwrap_or("").to_string();
    let supported = supported_versions();
    let support = if !supported.contains(&major) {
        Support::Unsupported { version, supported }
    } else if gsettings(&["get", "org.gnome.shell", "disable-user-extensions"]).await.is_ok_and(|v| v == "true") {
        Support::ExtensionsOff { version }
    } else {
        Support::Supported { version }
    };
    (support, state(&c).await)
}

async fn state(c: &zbus::Connection) -> State {
    let i = info(c).await;
    let num = |k: &str| i.get(k).and_then(|v| f64::try_from(v.try_clone().ok()?).ok());
    let enabled = i.get("enabled").and_then(|v| bool::try_from(v.try_clone().ok()?).ok());
    let listed = gsettings(&["get", "org.gnome.shell", "enabled-extensions"]).await.is_ok_and(|s| parse_list(&s).iter().any(|u| u == UUID));
    if i.is_empty() {
        // GNOME hasn't seen it: it's either off, or waiting for the next login.
        return if installed() && listed { State::NeedsLogin } else { State::Off };
    }
    match num("state").map(|s| s as i32) {
        Some(3) => {
            let errors = ext_call(c, "GetExtensionErrors", UUID).await.ok().and_then(|m| m.body().deserialize::<Vec<String>>().ok()).unwrap_or_default();
            State::Error(errors.last().cloned().unwrap_or_else(|| "it failed to start".into()))
        }
        Some(4) => State::Error("it doesn't support this GNOME version".into()),
        _ if enabled == Some(true) => State::On { outdated: !installed_matches() },
        _ => State::Off,
    }
}

/// Installs this build's extension and switches it on.
pub async fn turn_on() -> Result<State> {
    let c = shell().await?;
    // Replacing the files under a running copy only takes effect at the next login.
    let was_on = matches!(state(&c).await, State::On { .. });
    let changed = !installed_matches();
    let d = dir();
    std::fs::create_dir_all(&d)?;
    for (name, body) in FILES {
        std::fs::write(d.join(name), body)?;
    }
    if gsettings(&["get", "org.gnome.shell", "disable-user-extensions"]).await.is_ok_and(|v| v == "true") {
        gsettings(&["set", "org.gnome.shell", "disable-user-extensions", "false"]).await?;
    }
    edit_list("disabled-extensions", false).await?;
    // A known extension switches on at once; a new one is listed and loads at the next login.
    let _ = ext_call(&c, "EnableExtension", UUID).await;
    edit_list("enabled-extensions", true).await?;
    Ok(if was_on && changed { State::NeedsLogin } else { state(&c).await })
}

pub async fn turn_off() -> Result<State> {
    let c = shell().await?;
    let _ = ext_call(&c, "DisableExtension", UUID).await;
    edit_list("enabled-extensions", false).await?;
    Ok(state(&c).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        assert_eq!(parse_list("@as []"), Vec::<String>::new());
        assert_eq!(parse_list("['a@x', 'b@y']"), vec!["a@x", "b@y"]);
        assert_eq!(format_list(&["a@x".into(), "b@y".into()]), "['a@x', 'b@y']");
        assert!(supported_versions().contains(&"50".to_string()));
    }
}
