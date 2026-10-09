//! Bluetooth speakers through BlueZ's D-Bus API.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use zbus::fdo::ObjectManagerProxy;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, Proxy};

use super::{Engine, pw};

/// The A2DP sink profile: the device can play audio we send it.
const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const AGENT_PATH: &str = "/org/sonance/agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtDevice {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    pub audio_sink: bool,
    pub is_sonos: bool,
    /// PipeWire node of its A2DP sink while connected, e.g. `bluez_output.80_4A_F2_94_D2_5A.1`.
    pub sink: Option<String>,
}

type Props = HashMap<String, OwnedValue>;

fn flag(p: &Props, key: &str) -> bool {
    p.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn text<'a>(p: &'a Props, key: &str) -> Option<&'a str> {
    p.get(key).and_then(|v| <&str>::try_from(v).ok())
}

fn uuids(p: &Props) -> Vec<String> {
    p.get("UUIDs")
        .and_then(|v| <&Array>::try_from(v).ok())
        .map(|a| a.iter().filter_map(|u| <&str>::try_from(u).ok().map(str::to_lowercase)).collect())
        .unwrap_or_default()
}

/// Answers every BlueZ question with "yes", so a speaker in pairing mode just pairs.
struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn request_pin_code(&self, _device: ObjectPath<'_>) -> String {
        "0000".into()
    }
    fn display_pin_code(&self, _device: ObjectPath<'_>, _pin: &str) {}
    fn request_passkey(&self, _device: ObjectPath<'_>) -> u32 {
        0
    }
    fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}
    fn request_confirmation(&self, _device: ObjectPath<'_>, _passkey: u32) {}
    fn request_authorization(&self, _device: ObjectPath<'_>) {}
    fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: &str) {}
    fn cancel(&self) {}
}

/// The PipeWire sink node of a connected Bluetooth device, if it has one yet.
fn sink_for(nodes: &[pw::Node], mac: &str) -> Option<String> {
    let underscored = mac.replace(':', "_");
    nodes
        .iter()
        .filter(|n| n.class() == "Audio/Sink")
        .find(|n| {
            n.prop("api.bluez5.address").is_some_and(|a| a.eq_ignore_ascii_case(mac))
                || n.name().starts_with(&format!("bluez_output.{underscored}"))
        })
        .map(|n| n.name().to_string())
}

impl Engine {
    async fn bus(&self) -> Result<&Connection> {
        self.inner
            .bus
            .get_or_try_init(|| async { Connection::system().await.context("couldn't reach the system bus") })
            .await
    }

    async fn proxy(&self, path: &ObjectPath<'_>, iface: &'static str) -> Result<Proxy<'static>> {
        Ok(zbus::proxy::Builder::<Proxy>::new(self.bus().await?)
            .destination("org.bluez")?
            .path(path.to_owned())?
            .interface(iface)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }

    /// (path, interfaces→props) of every BlueZ object.
    async fn objects(&self) -> Result<Vec<(OwnedObjectPath, HashMap<String, Props>)>> {
        let om = ObjectManagerProxy::builder(self.bus().await?).destination("org.bluez")?.path("/")?.build().await?;
        let objs = om.get_managed_objects().await.context("BlueZ isn't running")?;
        Ok(objs
            .into_iter()
            .map(|(path, ifaces)| (path, ifaces.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
            .collect())
    }

    async fn adapter(&self) -> Result<OwnedObjectPath> {
        self.objects()
            .await?
            .into_iter()
            .filter(|(_, i)| i.contains_key("org.bluez.Adapter1"))
            .map(|(p, _)| p)
            .min_by(|a, b| a.as_str().cmp(b.as_str()))
            .ok_or_else(|| anyhow!("no Bluetooth adapter"))
    }

    async fn device_path(&self, mac: &str) -> Result<OwnedObjectPath> {
        self.objects()
            .await?
            .into_iter()
            .find(|(_, i)| i.get("org.bluez.Device1").and_then(|p| text(p, "Address")).is_some_and(|a| a.eq_ignore_ascii_case(mac)))
            .map(|(p, _)| p)
            .ok_or_else(|| anyhow!("{mac} isn't known yet; scan for it first"))
    }

    async fn device(&self, mac: &str) -> Result<Proxy<'static>> {
        let path = self.device_path(mac).await?;
        self.proxy(&path, "org.bluez.Device1").await
    }

    async fn register_agent(&self) -> Result<()> {
        self.inner
            .agent
            .get_or_try_init(|| async {
                let bus = self.bus().await?;
                bus.object_server().at(AGENT_PATH, Agent).await?;
                let mgr = self.proxy(&ObjectPath::from_static_str_unchecked("/org/bluez"), "org.bluez.AgentManager1").await?;
                match mgr.call_method("RegisterAgent", &(ObjectPath::from_static_str_unchecked(AGENT_PATH), "NoInputNoOutput")).await {
                    Ok(_) => Ok(()),
                    Err(zbus::Error::MethodError(name, ..)) if name.as_str() == "org.bluez.Error.AlreadyExists" => Ok(()),
                    Err(e) => Err(anyhow::Error::from(e).context("couldn't register the pairing agent")),
                }
            })
            .await?;
        Ok(())
    }

    pub async fn bt_devices(&self) -> Result<Vec<BtDevice>> {
        let objs = self.objects().await?;
        let nodes = pw::nodes().await.unwrap_or_default();
        let mut devs: Vec<BtDevice> = objs
            .iter()
            .filter_map(|(_, i)| i.get("org.bluez.Device1"))
            .filter_map(|p| {
                let mac = text(p, "Address")?.to_uppercase();
                let name = text(p, "Alias").or(text(p, "Name")).unwrap_or(&mac).to_string();
                let is_sonos = [text(p, "Alias"), text(p, "Name")].iter().flatten().any(|n| n.to_lowercase().contains("sonos"));
                let connected = flag(p, "Connected");
                Some(BtDevice {
                    sink: if connected { sink_for(&nodes, &mac) } else { None },
                    name,
                    paired: flag(p, "Paired"),
                    trusted: flag(p, "Trusted"),
                    connected,
                    audio_sink: uuids(p).iter().any(|u| u == A2DP_SINK),
                    is_sonos,
                    mac,
                })
            })
            .collect();
        devs.sort_by(|a, b| (!a.paired, &a.name).cmp(&(!b.paired, &b.name)));
        Ok(devs)
    }

    /// Turns Bluetooth on if it's off. Switching it off in GNOME soft-blocks
    /// the radio (rfkill), and BlueZ can't power an adapter past that, so the
    /// block is lifted first: `rfkill` works for the logged-in user, and
    /// GNOME's own switch is the fallback.
    async fn power_on(&self) -> Result<zbus::Proxy<'static>> {
        let path = self.adapter().await?;
        let adapter = self.proxy(&path, "org.bluez.Adapter1").await?;
        if adapter.get_property::<bool>("Powered").await.unwrap_or(false) {
            return Ok(adapter);
        }
        let unblocked = pw::run("rfkill", &["unblock", "bluetooth"]).await.is_ok();
        if !unblocked {
            let _ = pw::run(
                "gdbus",
                &["call", "--session", "--dest", "org.gnome.SettingsDaemon.Rfkill", "--object-path", "/org/gnome/SettingsDaemon/Rfkill",
                  "--method", "org.freedesktop.DBus.Properties.Set", "org.gnome.SettingsDaemon.Rfkill", "BluetoothAirplaneMode", "<false>"],
            )
            .await;
        }
        // The adapter needs a moment after the block lifts before it accepts Powered.
        let mut last = None;
        for _ in 0..20 {
            match adapter.set_property("Powered", true).await {
                Ok(()) => return Ok(adapter),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(anyhow::anyhow!("{}", last.map(|e| e.to_string()).unwrap_or_default())).context("couldn't turn Bluetooth on")
    }

    /// Discovers nearby devices for `secs` seconds, powering the adapter on if needed.
    pub async fn bt_scan(&self, secs: u64) -> Result<()> {
        let adapter = self.power_on().await?;
        adapter.call_method("StartDiscovery", &()).await.context("couldn't start scanning")?;
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let _ = adapter.call_method("StopDiscovery", &()).await;
        Ok(())
    }

    /// Pairs (if needed) and trusts, so the speaker reconnects without asking next time.
    pub async fn bt_pair(&self, mac: &str) -> Result<()> {
        self.power_on().await?;
        self.register_agent().await?;
        let dev = self.device(mac).await?;
        if !dev.get_property::<bool>("Paired").await.unwrap_or(false) {
            match dev.call_method("Pair", &()).await {
                Ok(_) => {}
                Err(zbus::Error::MethodError(name, ..)) if name.as_str() == "org.bluez.Error.AlreadyExists" => {}
                Err(e) => return Err(anyhow::Error::from(e).context(format!("pairing with {mac} failed (is it in pairing mode?)"))),
            }
        }
        dev.set_property("Trusted", true).await.context("couldn't trust the device")?;
        Ok(())
    }

    /// Connects and returns the PipeWire sink once it shows up (up to ~10 s).
    pub async fn bt_connect(&self, mac: &str) -> Result<String> {
        self.power_on().await?;
        let dev = self.device(mac).await?;
        dev.call_method("Connect", &()).await.with_context(|| format!("couldn't connect to {mac}"))?;
        for _ in 0..40 {
            if let Some(sink) = sink_for(&pw::nodes().await?, mac) {
                return Ok(sink);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("{mac} connected but no audio output appeared")
    }

    pub async fn bt_disconnect(&self, mac: &str) -> Result<()> {
        self.device(mac).await?.call_method("Disconnect", &()).await.with_context(|| format!("couldn't disconnect {mac}"))?;
        Ok(())
    }

    /// Unpairs and forgets the device.
    pub async fn bt_remove(&self, mac: &str) -> Result<()> {
        let path = self.device_path(mac).await?;
        let adapter = path.as_str().rsplit_once('/').map(|(a, _)| a.to_string()).context("odd device path")?;
        let adapter = self.proxy(&ObjectPath::try_from(adapter)?, "org.bluez.Adapter1").await?;
        adapter.call_method("RemoveDevice", &(path,)).await.with_context(|| format!("couldn't remove {mac}"))?;
        Ok(())
    }
}
