//! The `sonance` null sink and the system default output.

use anyhow::{Result, bail};

use super::{Engine, SINK, pw};

async fn sink_exists(name: &str) -> Result<bool> {
    Ok(pw::run("pactl", &["list", "short", "sinks"])
        .await?
        .lines()
        .any(|l| l.split('\t').nth(1) == Some(name)))
}

async fn default_sink() -> Result<String> {
    Ok(pw::run("pactl", &["get-default-sink"]).await?.trim().to_string())
}

impl Engine {
    /// Creates the `sonance` null sink (description "Sonance") if it doesn't exist. Idempotent.
    /// Loaded through pipewire-pulse so it outlives a crash of the app, like any other output.
    pub async fn ensure_sink(&self) -> Result<()> {
        self.reap_stale().await;
        let mut st = self.inner.state.lock().await;
        if sink_exists(SINK).await? {
            return Ok(());
        }
        let module = pw::run(
            "pactl",
            &[
                "load-module",
                "module-null-sink",
                &format!("sink_name={SINK}"),
                // Lowest clock priority: a linked Bluetooth speaker then drives the
                // graph, so its delay line isn't bridging two drifting clocks.
                "sink_properties=device.description=Sonance priority.driver=1",
                "rate=48000",
                "channels=2",
                "channel_map=front-left,front-right",
            ],
        )
        .await?;
        st.sink_module = Some(module.trim().to_string());
        if pw::wait_node(SINK, 3000).await?.is_none() {
            bail!("the Sonance output didn't appear in PipeWire");
        }
        Ok(())
    }

    /// Makes `sonance` the default output (remembering the previous default), or restores it.
    pub async fn set_default_output(&self, on: bool) -> Result<()> {
        let mut st = self.inner.state.lock().await;
        let current = default_sink().await?;
        if on {
            if current != SINK {
                st.prev_default = Some(current);
            }
            pw::run("pactl", &["set-default-sink", SINK]).await?;
        } else if let Some(prev) = st.prev_default.take() {
            // Only hand it back if nobody picked another output in the meantime.
            if current == SINK {
                pw::run("pactl", &["set-default-sink", &prev]).await?;
            }
        }
        Ok(())
    }
}
