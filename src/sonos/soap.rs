use anyhow::{bail, Result};
use std::time::Duration;

use super::xml::{escape, tag};

#[derive(Clone, Copy, Debug)]
pub enum Svc {
    AVTransport,
    RenderingControl,
    GroupRenderingControl,
    ContentDirectory,
    ZoneGroupTopology,
    AlarmClock,
    DeviceProperties,
}

impl Svc {
    fn path(self) -> &'static str {
        match self {
            Svc::AVTransport => "/MediaRenderer/AVTransport/Control",
            Svc::RenderingControl => "/MediaRenderer/RenderingControl/Control",
            Svc::GroupRenderingControl => "/MediaRenderer/GroupRenderingControl/Control",
            Svc::ContentDirectory => "/MediaServer/ContentDirectory/Control",
            Svc::ZoneGroupTopology => "/ZoneGroupTopology/Control",
            Svc::AlarmClock => "/AlarmClock/Control",
            Svc::DeviceProperties => "/DeviceProperties/Control",
        }
    }

    fn urn(self) -> String {
        let name = match self {
            Svc::AVTransport => "AVTransport",
            Svc::RenderingControl => "RenderingControl",
            Svc::GroupRenderingControl => "GroupRenderingControl",
            Svc::ContentDirectory => "ContentDirectory",
            Svc::ZoneGroupTopology => "ZoneGroupTopology",
            Svc::AlarmClock => "AlarmClock",
            Svc::DeviceProperties => "DeviceProperties",
        };
        format!("urn:schemas-upnp-org:service:{name}:1")
    }
}

#[derive(Clone)]
pub struct Soap {
    http: reqwest::Client,
}

impl Soap {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(6))
            .connect_timeout(Duration::from_secs(2))
            .build()
            .expect("http client");
        Self { http }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Calls a UPnP action and returns the raw response body.
    /// Sonos cares about argument order, so pass them as declared.
    pub async fn call(&self, ip: &str, svc: Svc, action: &str, args: &[(&str, &str)]) -> Result<String> {
        let urn = svc.urn();
        let mut body = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>"#,
        );
        body.push_str(&format!(r#"<u:{action} xmlns:u="{urn}">"#));
        for (k, v) in args {
            body.push_str(&format!("<{k}>{}</{k}>", escape(v)));
        }
        body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));

        let resp = self
            .http
            .post(format!("http://{ip}:1400{}", svc.path()))
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("SOAPACTION", format!("\"{urn}#{action}\""))
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let code = tag(&text, "errorCode").unwrap_or_else(|| status.as_u16().to_string());
            bail!("{action} failed (UPnP error {code})");
        }
        Ok(text)
    }
}
