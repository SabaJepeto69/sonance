//! Mic calibration: a test sweep from each speaker in turn, recorded at the
//! listening spot, gives its real delay and loudness there. Those replace the
//! room model's estimates for that spot.

use adw::prelude::*;
use std::rc::Rc;

use super::{spawn, widgets, App};
use crate::room::{plan::route_key, Kind, Measured};

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
        let col = gtk::Box::new(gtk::Orientation::Vertical, 12);
        col.set_margin_top(12);
        col.set_margin_start(18);
        col.set_margin_end(18);
        col.set_margin_bottom(18);
        for w in [info.upcast_ref::<gtk::Widget>(), mic_list.upcast_ref(), start.upcast_ref(), progress.upcast_ref(), results.upcast_ref()] {
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
            let list = r.unwrap_or_default();
            let names: Vec<&str> = list.iter().map(|(_, d)| d.as_str()).collect();
            mic2.set_model(Some(&gtk::StringList::new(&names)));
            // A webcam mic is usually the one that can sit at the listening spot.
            if let Some(i) = list.iter().position(|(_, d)| ["webcam", "camera"].iter().any(|k| d.to_lowercase().contains(k))) {
                mic2.set_selected(i as u32);
            }
            *m2.borrow_mut() = list.into_iter().map(|(n, _)| n).collect();
        });

        let w = Rc::downgrade(self);
        start.connect_clicked(move |b| {
            let Some(app) = w.upgrade() else { return };
            let Some(mic_node) = mics.borrow().get(mic.selected() as usize).cloned() else {
                app.toast("No microphone found");
                return;
            };
            b.set_sensitive(false);
            app.run_calibration(mic_node, progress.clone(), results.clone(), b.clone());
        });
        dialog.present(Some(&self.window));
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

    fn run_calibration(self: &Rc<Self>, mic: String, progress: gtk::Label, results: gtk::ListBox, start: gtk::Button) {
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
        let others: Vec<(String, String)> = self.group().map(|g| g.members.iter().map(|m| (m.uuid.clone(), m.ip.clone())).collect()).unwrap_or_default();
        let (audio, sonos) = (self.core.audio.clone(), self.core.sonos.clone());
        let w = Rc::downgrade(self);
        let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
        spawn(
            async move {
                // Give "regular" a moment to land if tuning was just switched off.
                tokio::time::sleep(std::time::Duration::from_millis(if was_tuned { 2000 } else { 300 })).await;
                let mut out = Vec::new();
                for job in &jobs {
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
                    let at_volume = match &job.room {
                        Some((_, ip)) => sonos.volume(ip).await.unwrap_or(0),
                        None => 0,
                    };
                    if job.sink != "sonance" {
                        let _ = audio.wake(&job.sink, 3.0).await;
                    }
                    let m = audio.measure(&job.sink, &mic, max_ms).await;
                    for (_, ip, v) in restore_vol {
                        let _ = sonos.set_volume(&ip, v).await;
                    }
                    if let Some((ip, ch, v)) = restore_ch {
                        let _ = sonos.set_channel_volume(&ip, ch, v).await;
                    }
                    out.push((job.id.clone(), m.map(|m| Measured { latency_ms: m.latency_ms, level_db: m.level_db, at_volume })));
                }
                out
            },
            move |out| {
                let Some(app) = w.upgrade() else { return };
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
