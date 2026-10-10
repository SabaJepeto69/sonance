//! Mic calibration: a test sweep from each speaker in turn, recorded at the
//! listening spot, gives its real delay and loudness there. Those replace the
//! room model's estimates for that spot.

use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

use super::{spawn, widgets, App};
use crate::phone::{Phase, Phone};
use crate::room::{plan::route_key, Kind, Measured};

/// The microphone list's first entry: the phone page.
const PHONE: &str = "";

/// A QR code for `text`, drawn black on white with a quiet zone.
fn qr_widget(text: &str) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_width(196).content_height(196).halign(gtk::Align::Center).build();
    let code = qrcode::QrCode::new(text.as_bytes()).ok();
    area.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        let Some(code) = &code else { return };
        let n = code.width();
        let cell = (w.min(h) as f64 / (n + 8) as f64).floor().max(1.0);
        let off = ((w.min(h) as f64) - cell * n as f64) / 2.0;
        cr.set_source_rgb(0.0, 0.0, 0.0);
        for (i, c) in code.to_colors().iter().enumerate() {
            if *c == qrcode::Color::Dark {
                cr.rectangle(off + (i % n) as f64 * cell, off + (i / n) as f64 * cell, cell, cell);
            }
        }
        let _ = cr.fill();
    });
    area
}

struct Job {
    id: String,
    name: String,
    sink: String,
    /// The Sonos room this box plays in, and its IP, for isolating it.
    room: Option<(String, String)>,
    /// LF/RF half of a stereo pair: the other half is muted while measuring.
    channel: Option<String>,
}

impl App {
    pub(super) fn show_calibrate(self: &Rc<Self>) {
        let Some(spot_name) = self.room.layout.borrow().active().map(|s| s.name.clone()) else {
            self.toast("Add a listening spot first");
            return;
        };
        let mic = adw::ComboRow::builder().title("Microphone").build();
        let mic_list = widgets::boxed_list();
        mic_list.append(&mic);
        let info = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .label(format!(
                "Put the microphone where your head is at “{spot_name}”, pause anything playing, and keep the room quiet. Each speaker plays a short sweep in turn."
            ))
            .build();
        let start = gtk::Button::builder().label("Start").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
        let progress = gtk::Label::builder().wrap(true).xalign(0.0).build();
        let results = widgets::boxed_list();
        results.set_visible(false);
        // Shown while "Phone" is picked: the QR code to open the page, and whether it's connected.
        let phone_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        phone_box.set_visible(false);
        let phone_status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["heading"]).build();
        let phone_help = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label", "caption"])
            .label("Scan with the phone's camera (same Wi-Fi). The page uses its own certificate, so the browser warns once: choose to continue anyway. Then tap “Start microphone”.")
            .build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 12);
        col.set_margin_top(12);
        col.set_margin_start(18);
        col.set_margin_end(18);
        col.set_margin_bottom(18);
        for w in [info.upcast_ref::<gtk::Widget>(), mic_list.upcast_ref(), phone_box.upcast_ref(), start.upcast_ref(), progress.upcast_ref(), results.upcast_ref()] {
            col.append(w);
        }
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        tv.set_content(Some(&widgets::page(&col)));
        let dialog = adw::Dialog::builder().title("Calibrate").content_width(460).content_height(560).child(&tv).build();
        super::glass::dialog(&dialog);

        // Fill the microphone list.
        let mics: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
        let (audio, m2, mic2) = (self.core.audio.clone(), mics.clone(), mic.clone());
        spawn(async move { audio.mics().await }, move |r| {
            let mut list = r.unwrap_or_default();
            list.insert(0, (PHONE.to_string(), "Phone (scan a QR code)".to_string()));
            let names: Vec<&str> = list.iter().map(|(_, d)| d.as_str()).collect();
            *m2.borrow_mut() = list.iter().map(|(n, _)| n.clone()).collect();
            mic2.set_model(Some(&gtk::StringList::new(&names)));
            // A webcam mic is the PC mic most likely to sit at the listening spot; failing that, the phone.
            let webcam = list.iter().position(|(_, d)| ["webcam", "camera"].iter().any(|k| d.to_lowercase().contains(k)));
            mic2.set_selected(webcam.unwrap_or(0) as u32);
        });

        // Picking "Phone" starts the page and shows its QR code.
        let w = Rc::downgrade(self);
        let (m3, pb, ps, ph) = (mics.clone(), phone_box.clone(), phone_status.clone(), phone_help.clone());
        mic.connect_selected_notify(move |row| {
            let Some(app) = w.upgrade() else { return };
            let is_phone = m3.borrow().get(row.selected() as usize).is_some_and(|n| n == PHONE);
            pb.set_visible(is_phone);
            if !is_phone || pb.first_child().is_some() {
                return;
            }
            ps.set_label("Starting…");
            let (pb, ps, ph) = (pb.clone(), ps.clone(), ph.clone());
            let w = Rc::downgrade(&app);
            spawn(Phone::start(), move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(phone) => {
                        let url = gtk::Label::builder().label(&phone.url).selectable(true).wrap(true).wrap_mode(gtk::pango::WrapMode::Char).css_classes(["caption", "monospace"]).build();
                        pb.append(&qr_widget(&phone.url));
                        pb.append(&url);
                        pb.append(&ps);
                        pb.append(&ph);
                        *app.phone.borrow_mut() = Some(phone);
                    }
                    Err(e) => {
                        pb.append(&ps);
                        ps.set_label(&format!("Couldn't start the phone page: {e:#}"));
                    }
                }
            });
        });
        // Keep the phone's status fresh while the dialog is open.
        let w = Rc::downgrade(self);
        let ps = phone_status.clone();
        let tick = glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            let Some(app) = w.upgrade() else { return glib::ControlFlow::Break };
            if let Some(p) = app.phone.borrow().as_ref() {
                ps.set_label(if p.connected() { "Phone connected and listening" } else { "Waiting for the phone…" });
            }
            glib::ControlFlow::Continue
        });
        let tick = std::cell::Cell::new(Some(tick));
        let w = Rc::downgrade(self);
        dialog.connect_closed(move |_| {
            if let Some(t) = tick.take() {
                t.remove();
            }
            // Close the page so it isn't left listening on the network.
            if let Some(app) = w.upgrade() {
                if let Some(p) = app.phone.borrow_mut().take() {
                    p.set_phase(Phase::Done, "Calibration closed in Sonance.");
                    let p2 = p.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || p2.stop());
                }
            }
        });

        let w = Rc::downgrade(self);
        start.connect_clicked(move |b| {
            let Some(app) = w.upgrade() else { return };
            let Some(mic_node) = mics.borrow().get(mic.selected() as usize).cloned() else {
                app.toast("No microphone found");
                return;
            };
            let phone = if mic_node == PHONE {
                match app.phone.borrow().clone() {
                    Some(p) if p.connected() => Some(p),
                    _ => {
                        progress.set_label("Open the page on your phone and tap “Start microphone” first.");
                        return;
                    }
                }
            } else {
                None
            };
            b.set_sensitive(false);
            app.run_calibration(mic_node, phone, progress.clone(), results.clone(), b.clone());
        });
        dialog.present(Some(&self.window));
    }

    /// Phone latencies share an unknown offset (the two clocks); pin the earliest
    /// speaker to what the room model expects for it, which keeps every gap exact.
    fn anchor_phone_latencies(&self, out: &mut [(String, anyhow::Result<Measured>)]) {
        let l = self.room.layout.borrow();
        let Some(spot) = l.active() else { return };
        let Some((id, first)) = out.iter().filter_map(|(id, r)| r.as_ref().ok().map(|m| (id, m.latency_ms))).min_by(|a, b| a.1.total_cmp(&b.1)) else { return };
        let Some(s) = l.speakers.iter().find(|s| &s.id == id) else { return };
        let shift = crate::room::plan::model_arrival_ms(s, spot) - first;
        for (_, r) in out.iter_mut() {
            if let Ok(m) = r {
                m.latency_ms += shift;
            }
        }
    }

    fn calibration_jobs(&self) -> anyhow::Result<(Vec<Job>, u32)> {
        let bt = self.bt_mode();
        if !self.core.cfg.lock().unwrap().pc_output {
            anyhow::bail!("Turn on “Play this PC's sound” first, so test tones can reach the speakers.");
        }
        let devices = self.bt_devices();
        let live = self.live_speakers();
        let l = self.room.layout.borrow();
        let mut jobs = Vec::new();
        for s in l.speakers.iter().filter(|s| live.contains(&s.id)) {
            let (room, channel) = match &s.kind {
                Kind::Sonos { room, channel, .. } => (self.room_ip(room).map(|ip| (room.clone(), ip)), channel.clone()),
                Kind::Bluetooth { .. } => (None, None),
            };
            // Over Bluetooth each speaker has its own output; over Wi-Fi the tone goes through the stream.
            let sink = if bt {
                let key = route_key(s);
                let dev = match &s.kind {
                    Kind::Sonos { room, .. } => super::output::sonos_bt(room, &devices).cloned(),
                    Kind::Bluetooth { mac } => devices.iter().find(|d| d.mac.eq_ignore_ascii_case(mac)).cloned(),
                };
                match dev.and_then(|d| d.sink) {
                    Some(sink) => sink,
                    None => anyhow::bail!("{} ({key}) isn't connected over Bluetooth", s.name),
                }
            } else {
                "sonance".to_string()
            };
            jobs.push(Job { id: s.id.clone(), name: s.name.clone(), sink, room, channel });
        }
        if jobs.is_empty() {
            anyhow::bail!("No placed speakers are playing. Place the selected room's speakers in the room first.");
        }
        // Sonos buffers its Wi-Fi stream for a few seconds; Bluetooth answers quickly.
        Ok((jobs, if bt { 1500 } else { 8000 }))
    }

    fn run_calibration(self: &Rc<Self>, mic: String, phone: Option<Phone>, progress: gtk::Label, results: gtk::ListBox, start: gtk::Button) {
        let (jobs, max_ms) = match self.calibration_jobs() {
            Ok(j) => j,
            Err(e) => {
                progress.set_label(&format!("{e:#}"));
                start.set_sensitive(true);
                return;
            }
        };
        // Measure the speakers as they normally play, then re-tune afterwards.
        let was_tuned = self.room.layout.borrow().tuned;
        if was_tuned {
            self.set_tuned_switch(false);
        }
        progress.set_label("Measuring… keep quiet");
        if let Some(p) = &phone {
            p.restart_take();
            p.set_phase(Phase::Measuring, "Measuring… keep still and quiet.");
        }
        let others: Vec<(String, String)> = self.group().map(|g| g.members.iter().map(|m| (m.uuid.clone(), m.ip.clone())).collect()).unwrap_or_default();
        let (audio, sonos) = (self.core.audio.clone(), self.core.sonos.clone());
        let w = Rc::downgrade(self);
        let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
        spawn(
            async move {
                // Give "regular" a moment to land if tuning was just switched off.
                tokio::time::sleep(std::time::Duration::from_millis(if was_tuned { 2000 } else { 300 })).await;
                let mut out = Vec::new();
                // With the phone, each sweep's send time is noted now and found in its take at the end.
                let mut sent = Vec::new();
                for (n, job) in jobs.iter().enumerate() {
                    if let Some(p) = &phone {
                        p.set_phase(Phase::Measuring, &format!("Measuring {} ({} of {})… keep quiet.", job.name, n + 1, jobs.len()));
                    }
                    // Silence everything except this one box, then put it all back.
                    let mut restore_vol = Vec::new();
                    let mut restore_ch = None;
                    if let Some((room, ip)) = &job.room {
                        for (uuid, oip) in others.iter().filter(|(u, _)| u != room) {
                            if let Ok(v) = sonos.volume(oip).await {
                                restore_vol.push((uuid.clone(), oip.clone(), v));
                                let _ = sonos.set_volume(oip, 0).await;
                            }
                        }
                        if let Some(ch) = &job.channel {
                            let other = if ch == "LF" { "RF" } else { "LF" };
                            if let Ok(v) = sonos.channel_volume(ip, other).await {
                                restore_ch = Some((ip.clone(), other, v));
                                let _ = sonos.set_channel_volume(ip, other, 0).await;
                            }
                        }
                    }
                    // Measure the speaker itself, not the user's tone settings: they're
                    // put back afterwards and tuning adds its correction on top.
                    let mut restore_tone = None;
                    if let Some((_, ip)) = &job.room {
                        if let Ok(eq) = sonos.eq(ip).await {
                            restore_tone = Some((ip.clone(), eq.bass, eq.treble));
                            let _ = sonos.set_bass(ip, 0).await;
                            let _ = sonos.set_treble(ip, 0).await;
                        }
                    }
                    let at_volume = match &job.room {
                        Some((_, ip)) => sonos.volume(ip).await.unwrap_or(0),
                        None => 0,
                    };
                    if job.sink != "sonance" {
                        let _ = audio.wake(&job.sink, 3.0).await;
                    }
                    let m = match &phone {
                        None => audio.measure(&job.sink, &mic, max_ms).await,
                        Some(_) => {
                            let s = audio.play_marked(&job.sink).await;
                            // Let the sweep finish in the room before the next speaker.
                            tokio::time::sleep(std::time::Duration::from_millis(max_ms as u64 + 1800)).await;
                            sent.push(s.as_ref().ok().copied());
                            s.map(|_| crate::audio::Measurement { latency_ms: 0.0, level_db: 0.0, confidence: 0.0, bands_db: Vec::new() })
                        }
                    };
                    if let Some((ip, bass, treble)) = restore_tone {
                        let _ = sonos.set_bass(&ip, bass).await;
                        let _ = sonos.set_treble(&ip, treble).await;
                    }
                    for (_, ip, v) in restore_vol {
                        let _ = sonos.set_volume(&ip, v).await;
                    }
                    if let Some((ip, ch, v)) = restore_ch {
                        let _ = sonos.set_channel_volume(&ip, ch, v).await;
                    }
                    out.push((job.id.clone(), m.map(|m| Measured { latency_ms: m.latency_ms, level_db: m.level_db, at_volume, bands_db: m.bands_db })));
                }
                if let Some(p) = &phone {
                    // The last chunks are still on their way.
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    let take = p.take();
                    let mut sent = sent.into_iter();
                    for (_, r) in out.iter_mut() {
                        let Ok(m) = r else { continue };
                        let Some(Some(at)) = sent.next() else { continue };
                        *r = match &take {
                            Some((rate, samples, origin)) => crate::audio::analyse_remote(samples, *rate, *origin, at, max_ms)
                                .map(|x| Measured { latency_ms: x.latency_ms, level_db: x.level_db, at_volume: m.at_volume, bands_db: x.bands_db }),
                            None => Err(anyhow::anyhow!("nothing arrived from the phone")),
                        };
                    }
                    p.set_phase(Phase::Done, "Done. The results are in Sonance; you can close this page.");
                }
                (out, phone.is_some())
            },
            move |(mut out, by_phone)| {
                let Some(app) = w.upgrade() else { return };
                if by_phone {
                    app.anchor_phone_latencies(&mut out);
                }
                results.remove_all();
                results.set_visible(true);
                let mut ok = 0;
                {
                    let mut l = app.room.layout.borrow_mut();
                    let active = l.active_spot.clone();
                    let spot = l.spots.iter_mut().find(|s| Some(&s.id) == active.as_ref());
                    let mut spot = spot;
                    for ((id, r), name) in out.into_iter().zip(&names) {
                        let sub = match &r {
                            Ok(m) => format!("arrives after {:.0} ms · {:.1} dB", m.latency_ms, m.level_db),
                            Err(e) => format!("{e:#}"),
                        };
                        results.append(&widgets::row(name, &sub));
                        if let (Ok(m), Some(spot)) = (r, spot.as_deref_mut()) {
                            spot.measured.insert(id, m);
                            ok += 1;
                        }
                    }
                }
                app.room.layout.borrow().save();
                progress.set_label(&format!("Measured {ok} of {} speakers. The plan now uses these.", names.len()));
                start.set_sensitive(true);
                start.set_label("Measure again");
                app.recompute_plan();
                app.update_bt_routes();
                if was_tuned {
                    app.set_tuned_switch(true);
                }
            },
        );
    }
}
