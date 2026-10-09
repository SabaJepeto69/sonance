//! "Play this PC's sound": the Sonance PipeWire output and where it goes.
//!
//! Sonos only → the output is streamed to the selected group over Wi-Fi
//! (Sonos buffers a second or two). Any other Bluetooth speaker switched on →
//! every speaker, the Sonos rooms included, is fed over Bluetooth straight
//! from the PC, each through its own delay line so they line up at the
//! listening spot. Bluetooth latency is a fraction of Sonos's Wi-Fi buffer.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use super::{spawn, widgets, App};
use crate::audio::{BtDevice, Route};
use crate::room::plan;
use crate::sonos::Member;

pub const STREAM_PORT: u16 = 8899;

pub struct OutputSection {
    pub root: gtk::Box,
    pc: adw::SwitchRow,
    bt_list: gtk::ListBox,
    add: gtk::Button,
    /// Rooms that were in the group when Bluetooth mode began; they're
    /// ungrouped while on Bluetooth and regrouped afterwards.
    pub bt_rooms: RefCell<Vec<Member>>,
    pub bt_coordinator: RefCell<Option<Member>>,
    devices: RefCell<Vec<BtDevice>>,
}

/// A Sonos speaker's Bluetooth address is the MAC inside its UUID
/// (RINCON_AABBCCDDEEFF01400 → AA:BB:CC:DD:EE:FF); its Bluetooth name ends
/// in the last four hex digits, which catches models whose radios differ.
pub fn sonos_bt<'a>(uuid: &str, devices: &'a [BtDevice]) -> Option<&'a BtDevice> {
    let hex = uuid.strip_prefix("RINCON_")?.get(..12)?.to_uppercase();
    let mac: String = hex.as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(":");
    devices
        .iter()
        .find(|d| d.mac.eq_ignore_ascii_case(&mac))
        .or_else(|| devices.iter().find(|d| d.is_sonos && d.name.to_uppercase().contains(&hex[8..])))
}

impl OutputSection {
    pub fn new() -> Self {
        let pc = adw::SwitchRow::builder().title("Play this PC's sound").subtitle("Spotify app, YouTube, games…").build();
        let pc_list = widgets::boxed_list();
        pc_list.append(&pc);
        let bt_list = widgets::boxed_list();
        let add = gtk::Button::builder().label("Add Bluetooth speaker…").css_classes(["flat"]).halign(gtk::Align::Start).build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.append(&widgets::heading("This PC"));
        root.append(&pc_list);
        root.append(&widgets::heading("Bluetooth speakers"));
        root.append(&bt_list);
        root.append(&add);
        Self { root, pc, bt_list, add, bt_rooms: RefCell::default(), bt_coordinator: RefCell::default(), devices: RefCell::default() }
    }
}

impl App {
    pub(super) fn connect_output(self: &Rc<Self>) {
        let o = &self.output;
        o.pc.set_active(self.core.cfg.lock().unwrap().pc_output);
        let w = Rc::downgrade(self);
        o.pc.connect_active_notify(move |r| {
            let Some(app) = w.upgrade() else { return };
            {
                let mut c = app.core.cfg.lock().unwrap();
                c.pc_output = r.is_active();
                c.save();
            }
            app.apply_output();
        });
        let w = Rc::downgrade(self);
        o.add.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.room_pop.popdown();
                app.show_bt_dialog();
            }
        });
        let w = Rc::downgrade(self);
        self.room_pop.connect_show(move |_| {
            if let Some(app) = w.upgrade() {
                app.refresh_bt();
            }
        });
        if self.core.cfg.lock().unwrap().pc_output {
            // Picks up where the last session left off once speakers are known.
            let w = Rc::downgrade(self);
            gtk::glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                if let Some(app) = w.upgrade() {
                    app.apply_output();
                }
            });
        }
    }

    pub fn bt_mode(&self) -> bool {
        let c = self.core.cfg.lock().unwrap();
        c.pc_output && !c.bt_speakers.is_empty()
    }

    pub fn bt_speaker_on(&self, mac: &str) -> bool {
        let c = self.core.cfg.lock().unwrap();
        c.pc_output && c.bt_speakers.iter().any(|m| m.eq_ignore_ascii_case(mac))
    }

    pub fn bt_devices(&self) -> Vec<BtDevice> {
        self.output.devices.borrow().clone()
    }

    /// Re-reads Bluetooth devices and redraws the switches.
    pub fn refresh_bt(self: &Rc<Self>) {
        let audio = self.core.audio.clone();
        let w = Rc::downgrade(self);
        spawn(async move { audio.bt_devices().await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            let Ok(devs) = r else { return };
            *app.output.devices.borrow_mut() = devs;
            app.render_bt();
            app.render_placed();
        });
    }

    fn render_bt(self: &Rc<Self>) {
        let o = &self.output;
        o.bt_list.remove_all();
        let enabled = self.core.cfg.lock().unwrap().bt_speakers.clone();
        let devs: Vec<BtDevice> = o.devices.borrow().iter().filter(|d| d.paired && d.audio_sink && !d.is_sonos).cloned().collect();
        o.bt_list.set_visible(!devs.is_empty());
        for d in devs {
            let row = adw::SwitchRow::builder().title(&d.name).subtitle(if d.connected { "Connected" } else { "Paired" }).build();
            row.set_active(enabled.iter().any(|m| m.eq_ignore_ascii_case(&d.mac)));
            let (w, mac) = (Rc::downgrade(self), d.mac.clone());
            row.connect_active_notify(move |r| {
                let Some(app) = w.upgrade() else { return };
                {
                    let mut c = app.core.cfg.lock().unwrap();
                    c.bt_speakers.retain(|m| !m.eq_ignore_ascii_case(&mac));
                    if r.is_active() {
                        c.bt_speakers.push(mac.clone());
                    }
                    c.save();
                }
                app.apply_output();
            });
            o.bt_list.append(&row);
        }
    }

    /// Brings the speakers in line with the PC-output and Bluetooth switches.
    pub fn apply_output(self: &Rc<Self>) {
        let (on, bt_macs) = {
            let c = self.core.cfg.lock().unwrap();
            (c.pc_output, c.bt_speakers.clone())
        };
        let bt = on && !bt_macs.is_empty();
        let o = &self.output;
        // Entering Bluetooth mode remembers the group; leaving it regroups.
        let was_bt = o.bt_coordinator.borrow().is_some();
        if bt && !was_bt {
            if let Some(g) = self.group() {
                *o.bt_rooms.borrow_mut() = g.members.clone();
                *o.bt_coordinator.borrow_mut() = Some(g.coordinator.clone());
            }
        }
        let regroup = if !bt && was_bt { o.bt_coordinator.borrow_mut().take().map(|c| (c, o.bt_rooms.borrow_mut().drain(..).collect::<Vec<_>>())) } else { None };
        let rooms = o.bt_rooms.borrow().clone();
        let coord = if bt { o.bt_coordinator.borrow().clone() } else { self.coord() };
        let routes_plan = self.route_targets();

        let (audio, sonos) = (self.core.audio.clone(), self.core.sonos.clone());
        let w = Rc::downgrade(self);
        spawn(
            async move {
                // Sonos speakers we hold over Bluetooth go back to Wi-Fi whenever we leave Bluetooth mode.
                let release_sonos = |audio: crate::audio::Engine| async move {
                    for d in audio.bt_devices().await.unwrap_or_default() {
                        if d.is_sonos && d.connected {
                            let _ = audio.bt_disconnect(&d.mac).await;
                        }
                    }
                };
                if !on {
                    audio.clear_routes().await;
                    audio.stop_wifi_stream().await;
                    audio.set_default_output(false).await?;
                    release_sonos(audio.clone()).await;
                    if let Some((c, members)) = regroup {
                        for m in members.iter().filter(|m| m.uuid != c.uuid) {
                            let _ = sonos.join(&m.ip, &c.uuid).await;
                        }
                    }
                    return anyhow::Ok("This PC's sound is off".to_string());
                }
                audio.ensure_sink().await?;
                audio.set_default_output(true).await?;
                let coord = coord.ok_or_else(|| anyhow::anyhow!("No Sonos room selected"))?;

                if !bt {
                    audio.clear_routes().await;
                    release_sonos(audio.clone()).await;
                    if let Some((c, members)) = regroup {
                        for m in members.iter().filter(|m| m.uuid != c.uuid) {
                            let _ = sonos.join(&m.ip, &c.uuid).await;
                        }
                    }
                    let urls = audio.start_wifi_stream(STREAM_PORT).await?;
                    // Measured on the real speakers: plain WAV starts in ~0.4 s and
                    // plays ~0.5 s behind the PC; MP3 radio lags ~3.8 s (Sonos
                    // buffers compressed streams far more) and FLAC never starts.
                    // Metadata stays empty: a music-service DIDL makes Sonos hang
                    // looking up a service that doesn't exist.
                    let wav = urls.wav.ok_or_else(|| anyhow::anyhow!("the WAV stream didn't start"))?;
                    sonos.play_uri(&coord, &wav, "", false).await?;
                    return Ok("Playing this PC's sound on Sonos over Wi-Fi".to_string());
                }

                audio.stop_wifi_stream().await;
                let _ = sonos.pause(&coord.ip).await;
                // Each room takes its own Bluetooth link, so none of them should
                // be relaying a group over Wi-Fi.
                for m in rooms.iter().filter(|m| m.uuid != coord.uuid) {
                    let _ = sonos.leave(&m.ip).await;
                }
                let devices = audio.bt_devices().await?;
                let mut routes = Vec::new();
                for m in &rooms {
                    let dev = sonos_bt(&m.uuid, &devices).ok_or_else(|| {
                        anyhow::anyhow!("{} isn't paired with this PC yet. Hold its Bluetooth button, then use “Add Bluetooth speaker…”.", m.name)
                    })?;
                    let sink = audio.bt_connect(&dev.mac).await?;
                    let (delay_ms, gain_db) = routes_plan.get(&m.uuid).copied().unwrap_or((0.0, 0.0));
                    routes.push(Route { sink, delay_ms, gain_db });
                }
                for mac in &bt_macs {
                    let sink = audio.bt_connect(mac).await?;
                    let (delay_ms, gain_db) = routes_plan.get(mac).copied().unwrap_or((0.0, 0.0));
                    routes.push(Route { sink, delay_ms, gain_db });
                }
                audio.set_routes(&routes).await?;
                Ok(format!("Playing this PC's sound on {} speakers over Bluetooth", routes.len()))
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(msg) => app.toast(&msg),
                    Err(e) => app.toast(&format!("{e:#}")),
                }
                app.refresh_bt();
                app.recompute_plan();
            },
        );
    }

    /// Re-sends delays and gains after the plan changed, without reconnecting anything.
    pub fn update_bt_routes(self: &Rc<Self>) {
        if !self.bt_mode() {
            return;
        }
        let targets = self.route_targets();
        let devices = self.bt_devices();
        let rooms = self.output.bt_rooms.borrow().clone();
        let macs = self.core.cfg.lock().unwrap().bt_speakers.clone();
        let mut routes = Vec::new();
        for m in &rooms {
            if let Some(sink) = sonos_bt(&m.uuid, &devices).and_then(|d| d.sink.clone()) {
                let (delay_ms, gain_db) = targets.get(&m.uuid).copied().unwrap_or((0.0, 0.0));
                routes.push(Route { sink, delay_ms, gain_db });
            }
        }
        for mac in &macs {
            if let Some(sink) = devices.iter().find(|d| d.mac.eq_ignore_ascii_case(mac)).and_then(|d| d.sink.clone()) {
                let (delay_ms, gain_db) = targets.get(mac).copied().unwrap_or((0.0, 0.0));
                routes.push(Route { sink, delay_ms, gain_db });
            }
        }
        if routes.is_empty() {
            return;
        }
        let audio = self.core.audio.clone();
        self.act(async move { audio.set_routes(&routes).await });
    }

    pub fn current_plan(&self) -> Option<plan::Plan> {
        self.room.view.plan.borrow().clone()
    }

    /// Delay and gain per route. Delays always apply, since lining speakers
    /// up is what keeps them in sync; gains only when tuning is on.
    fn route_targets(&self) -> std::collections::HashMap<String, (f32, f32)> {
        let tuned = self.room.layout.borrow().tuned;
        self.current_plan()
            .map(|p| p.bt_routes.into_iter().map(|(k, (d, g))| (k, (d, if tuned { g } else { 0.0 }))).collect())
            .unwrap_or_default()
    }

    /// Scan for and pair Bluetooth speakers, Sonos ones included.
    fn show_bt_dialog(self: &Rc<Self>) {
        let list = widgets::boxed_list();
        let status = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["dim-label"]).label("Put the speaker in pairing mode (on Sonos: hold the Bluetooth button until the light flashes blue), then scan.").build();
        let scan = gtk::Button::builder().label("Scan").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
        let spinner = adw::Spinner::builder().visible(false).height_request(24).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 12);
        col.set_margin_top(12);
        col.set_margin_bottom(18);
        col.set_margin_start(18);
        col.set_margin_end(18);
        col.append(&status);
        col.append(&scan);
        col.append(&spinner);
        col.append(&list);
        let header = adw::HeaderBar::new();
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&widgets::page(&col)));
        let dialog = adw::Dialog::builder().title("Add Bluetooth speaker").content_width(440).content_height(520).child(&tv).build();
        super::glass::dialog(&dialog);

        let fill = {
            let (w, list) = (Rc::downgrade(self), list.clone());
            Rc::new(move |devs: Vec<BtDevice>| {
                let Some(app) = w.upgrade() else { return };
                list.remove_all();
                for d in devs.into_iter().filter(|d| d.audio_sink || !d.paired) {
                    let row = widgets::row(&d.name, if d.paired { "Paired" } else if d.is_sonos { "Sonos · not paired" } else { "Not paired" });
                    if !d.paired {
                        let pair = gtk::Button::builder().label("Pair").valign(gtk::Align::Center).css_classes(["pill"]).build();
                        let (w2, mac, is_sonos, name) = (Rc::downgrade(&app), d.mac.clone(), d.is_sonos, d.name.clone());
                        pair.connect_clicked(move |b| {
                            let Some(app) = w2.upgrade() else { return };
                            b.set_sensitive(false);
                            b.set_label("Pairing…");
                            let (audio, mac2, b2) = (app.core.audio.clone(), mac.clone(), b.clone());
                            let (w3, name, mac) = (Rc::downgrade(&app), name.clone(), mac.clone());
                            spawn(async move { audio.bt_pair(&mac2).await }, move |r| {
                                let Some(app) = w3.upgrade() else { return };
                                match r {
                                    Ok(()) => {
                                        b2.set_label("Paired");
                                        if !is_sonos {
                                            let mut c = app.core.cfg.lock().unwrap();
                                            if !c.bt_speakers.iter().any(|m| m.eq_ignore_ascii_case(&mac)) {
                                                c.bt_speakers.push(mac.clone());
                                            }
                                            c.save();
                                        }
                                        app.toast(&format!("Paired {name}"));
                                        app.refresh_bt();
                                    }
                                    Err(e) => {
                                        b2.set_label("Pair");
                                        b2.set_sensitive(true);
                                        app.toast(&format!("{e:#}"));
                                    }
                                }
                            });
                        });
                        row.add_suffix(&pair);
                    }
                    list.append(&row);
                }
            })
        };
        let (w, sp, f) = (Rc::downgrade(self), spinner.clone(), fill.clone());
        scan.connect_clicked(move |b| {
            let Some(app) = w.upgrade() else { return };
            b.set_sensitive(false);
            sp.set_visible(true);
            let audio = app.core.audio.clone();
            let (b, sp, f) = (b.clone(), sp.clone(), f.clone());
            spawn(
                async move {
                    audio.bt_scan(10).await?;
                    audio.bt_devices().await
                },
                move |r| {
                    b.set_sensitive(true);
                    sp.set_visible(false);
                    if let Ok(devs) = r {
                        f(devs);
                    }
                },
            );
        });
        fill(self.bt_devices());
        dialog.present(Some(&self.window));
    }
}
