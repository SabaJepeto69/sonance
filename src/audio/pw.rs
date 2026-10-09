//! Thin helpers around the PipeWire / pipewire-pulse command-line tools.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use tokio::process::Command;

/// A child that cannot outlive the app: killed when its handle is dropped, and sent SIGTERM by
/// the kernel if we crash (PDEATHSIG fires when the spawning thread dies; tokio workers live as
/// long as the runtime, so that is effectively the process).
pub fn command(prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.kill_on_drop(true).stdin(Stdio::null());
    // SAFETY: prctl is async-signal-safe and touches no Rust state.
    unsafe {
        c.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c
}

/// Runs a short-lived tool and returns its stdout, turning a non-zero exit into its stderr.
pub async fn run(prog: &str, args: &[&str]) -> Result<String> {
    let out = command(prog)
        .args(args)
        .output()
        .await
        .with_context(|| format!("couldn't run {prog}"))?;
    if !out.status.success() {
        bail!("{prog} {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: u64,
    pub props: Map<String, Value>,
}

impl Node {
    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).and_then(Value::as_str)
    }
    pub fn num(&self, key: &str) -> Option<u64> {
        let v = self.props.get(key)?;
        v.as_u64().or_else(|| v.as_str()?.parse().ok())
    }
    pub fn name(&self) -> &str {
        self.prop("node.name").unwrap_or("")
    }
    pub fn class(&self) -> &str {
        self.prop("media.class").unwrap_or("")
    }
}

/// Every object in the graph, as (type, id, props).
async fn dump() -> Result<Vec<(String, u64, Map<String, Value>)>> {
    let json: Value = serde_json::from_str(&run("pw-dump", &[]).await?).context("pw-dump output")?;
    Ok(json
        .as_array()
        .map(|objs| {
            objs.iter()
                .filter_map(|o| {
                    let ty = o.get("type")?.as_str()?.to_string();
                    let id = o.get("id")?.as_u64()?;
                    let props = o.get("info")?.get("props")?.as_object()?.clone();
                    Some((ty, id, props))
                })
                .collect()
        })
        .unwrap_or_default())
}

pub async fn nodes() -> Result<Vec<Node>> {
    Ok(dump()
        .await?
        .into_iter()
        .filter(|(ty, ..)| ty == "PipeWire:Interface:Node")
        .map(|(_, id, props)| Node { id, props })
        .collect())
}

pub async fn find_node(name: &str) -> Result<Option<Node>> {
    Ok(nodes().await?.into_iter().find(|n| n.name() == name))
}

/// Polls until a node named `name` exists, for up to `ms` milliseconds.
pub async fn wait_node(name: &str, ms: u64) -> Result<Option<Node>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        if let Some(n) = find_node(name).await? {
            return Ok(Some(n));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// `node:port` names of all output (`out = true`) or input ports, as pw-link prints them.
pub async fn ports(out: bool) -> Result<Vec<String>> {
    Ok(run("pw-link", &[if out { "-o" } else { "-i" }])
        .await?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Kills helper processes a crashed previous run left behind. Matched by our node-name prefixes, and only if the owning process is one of the tools we
/// spawn, so nothing else can ever be hit.
pub async fn kill_stale() {
    let Ok(objs) = dump().await else { return };
    let me = std::process::id() as u64;
    let pid_of = |client: u64| {
        objs.iter().find(|(ty, id, _)| ty == "PipeWire:Interface:Client" && *id == client).and_then(|(.., p)| {
            let v = p.get("application.process.id")?;
            v.as_u64().or_else(|| v.as_str()?.parse().ok())
        })
    };
    let mut pids: Vec<u64> = objs
        .iter()
        .filter(|(ty, ..)| ty == "PipeWire:Interface:Node")
        .filter(|(.., p)| {
            let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
            s("node.name").starts_with("sonance-route-")
                || s("node.name").starts_with("sonance-measure-")
                || s("node.name").starts_with("sonance-wifi-")
        })
        .filter_map(|(.., p)| {
            let c = p.get("client.id")?;
            pid_of(c.as_u64().or_else(|| c.as_str()?.parse().ok())?)
        })
        .filter(|&pid| pid != me)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let ppid = stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1)?.parse::<u64>().ok());
        if ppid != Some(me) && matches!(comm.trim(), "pw-loopback" | "pw-cat" | "pw-record" | "pw-play" | "ffmpeg") {
            // SAFETY: plain syscall on a pid we just verified is one of our helper tools.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
    }
}
