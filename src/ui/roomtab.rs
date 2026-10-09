//! The Room tab: the 3D view plus a side panel for spots, placement and the
//! tuning toggle, and the code that pushes a plan onto the speakers.

use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use super::room3d::{Pick, Room3d};
use super::{spawn, widgets, App};
use crate::room::{plan, Baseline, Kind, Layout, Speaker};
use crate::sonos::Unit;

pub struct RoomTab {
    pub root: gtk::Box,
    pub view: Rc<Room3d>,
    pub layout: Rc<RefCell<Layout>>,
    tuned: adw::SwitchRow,
    spots: gtk::ListBox,
    add_spot: gtk::Button,
    selected: gtk::Box,
    placed: gtk::ListBox,
    results: gtk::ListBox,
    results_note: gtk::Label,
    size: [adw::SpinRow; 3],
    pub calibrate: gtk::Button,
    units: RefCell<Vec<Unit>>,
    apply_pending: RefCell<Option<glib::SourceId>>,
    save_pending: RefCell<Option<glib::SourceId>>,
    syncing: Cell<bool>,
}

fn group(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.append(&widgets::heading(title));
    b.append(child);
    b
}

impl RoomTab {
    pub fn new() -> Self {
        let layout = Rc::new(RefCell::new(Layout::load()));
        let view = Room3d::new(layout.clone());
        let hint = gtk::Label::builder()
            .label("Drag to orbit · drag a speaker or spot to move it · scroll over a speaker to turn it")
            .css_classes(["caption", "dim-label", "room-hint"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(10)
            .build();
        let stage = gtk::Overlay::new();
        stage.set_child(Some(&view.area));
        stage.add_overlay(&hint);
        hint.set_can_target(false);

        let tuned = adw::SwitchRow::builder().title("Tune for listening spot").subtitle("Off plays your regular sound").build();
        let tuned_list = widgets::boxed_list();
        tuned_list.append(&tuned);
        let calibrate = gtk::Button::builder().label("Calibrate with microphone…").css_classes(["pill"]).margin_top(4).build();

        let spots = widgets::boxed_list();
        let add_spot = gtk::Button::builder().label("Add listening spot").css_classes(["flat"]).halign(gtk::Align::Start).build();
        let spots_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        spots_box.append(&spots);
        spots_box.append(&add_spot);

        let selected = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let placed = widgets::boxed_list();
        let results = widgets::boxed_list();
        let results_note = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["caption", "dim-label"]).build();
        let results_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        results_box.append(&results);
        results_box.append(&results_note);

        let mk = |t: &str, lo: f64, hi: f64| {
            let r = adw::SpinRow::with_range(lo, hi, 0.1);
            r.set_title(t);
            r.set_digits(1);
            r
        };
        let size = [mk("Width (m)", 1.5, 20.0), mk("Depth (m)", 1.5, 20.0), mk("Height (m)", 2.0, 6.0)];
        let size_list = widgets::boxed_list();
        for r in &size {
            size_list.append(r);
        }

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 4);
        panel.set_margin_start(14);
        panel.set_margin_end(14);
        panel.set_margin_bottom(130);
        panel.append(&group("Tuning", &tuned_list));
        panel.append(&calibrate);
        panel.append(&group("Listening spots", &spots_box));
        panel.append(&selected);
        panel.append(&group("For this spot", &results_box));
        panel.append(&group("Speakers in the room", &placed));
        panel.append(&group("Room size", &size_list));
        let scroll = gtk::ScrolledWindow::builder().child(&panel).hscrollbar_policy(gtk::PolicyType::Never).width_request(360).build();
        scroll.add_css_class("room-panel");

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&stage);
        root.append(&scroll);
        Self {
            root,
            view,
            layout,
            tuned,
            spots,
            add_spot,
            selected,
            placed,
            results,
            results_note,
            size,
            calibrate,
            units: RefCell::default(),
            apply_pending: RefCell::default(),
            save_pending: RefCell::default(),
            syncing: Cell::new(false),
        }
    }
}

impl App {
    pub(super) fn connect_room(self: &Rc<Self>) {
        let t = &self.room;
        t.syncing.set(true);
        {
            let l = t.layout.borrow();
            t.tuned.set_active(l.tuned);
            t.size[0].set_value(l.room.width as f64);
            t.size[1].set_value(l.room.depth as f64);
            t.size[2].set_value(l.room.height as f64);
        }
        t.syncing.set(false);

        let w = Rc::downgrade(self);
        t.view.set_on_change(move || {
            if let Some(app) = w.upgrade() {
                app.room_changed();
            }
        });
        let w = Rc::downgrade(self);
        t.view.set_on_select(move || {
            if let Some(app) = w.upgrade() {
                app.render_selected();
            }
        });
        let w = Rc::downgrade(self);
        t.tuned.connect_active_notify(move |row| {
            let Some(app) = w.upgrade() else { return };
            if app.room.syncing.get() {
                return;
            }
            app.set_tuned(row.is_active());
        });
        let w = Rc::downgrade(self);
        t.add_spot.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let n = app.room.layout.borrow().spots.len() + 1;
            let id = app.room.layout.borrow_mut().add_spot(&format!("Spot {n}"));
            *app.room.view.selected.borrow_mut() = Some(Pick::Spot(id));
            app.render_spots();
            app.render_selected();
            app.room_changed();
        });
        for (i, row) in t.size.iter().enumerate() {
            let w = Rc::downgrade(self);
            row.connect_notify_local(Some("value"), move |r, _| {
                let Some(app) = w.upgrade() else { return };
                if app.room.syncing.get() {
                    return;
                }
                {
                    let mut l = app.room.layout.borrow_mut();
                    let v = r.value() as f32;
                    match i {
                        0 => l.room.width = v,
                        1 => l.room.depth = v,
                        _ => l.room.height = v,
                    }
                    l.clamp();
                }
                app.room.view.recenter();
                app.room_changed();
            });
        }
        let w = Rc::downgrade(self);
        t.calibrate.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.show_calibrate();
            }
        });
        self.render_spots();
        self.render_selected();
    }

    /// Called when the Room tab comes on screen.
    pub(super) fn room_shown(self: &Rc<Self>) {
        let Some(ip) = self.any_ip() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(async move { sonos.units(&ip).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            let Ok(units) = r else { return };
            {
                // Layouts saved before models were recorded pick them up here.
                let mut l = app.room.layout.borrow_mut();
                for s in l.speakers.iter_mut().filter(|s| s.model.is_empty()) {
                    if let Some(u) = units.iter().find(|u| format!("sonos:{}", u.uuid) == s.id) {
                        s.model = u.model.clone();
                    }
                }
            }
            *app.room.units.borrow_mut() = units;
            app.place_defaults();
            app.refresh_bt();
            app.render_placed();
            app.render_spots();
            app.recompute_plan();
        });
    }

    fn unit_name(u: &Unit) -> String {
        match u.channel.as_deref() {
            Some("LF") => format!("{} · Left", u.name),
            Some("RF") => format!("{} · Right", u.name),
            _ => u.name.clone(),
        }
    }

    /// First visit: put every Sonos box and one spot somewhere sensible.
    fn place_defaults(&self) {
        let mut l = self.room.layout.borrow_mut();
        if !l.speakers.is_empty() || !l.spots.is_empty() {
            return;
        }
        let (w, d) = (l.room.width, l.room.depth);
        for u in self.room.units.borrow().iter() {
            let (pos, yaw) = match u.channel.as_deref() {
                Some("LF") => ([w * 0.28, 0.9, 0.35], 180.0),
                Some("RF") => ([w * 0.72, 0.9, 0.35], 180.0),
                _ => ([0.4, 0.5, d * 0.7], 90.0),
            };
            l.speakers.push(Speaker {
                id: format!("sonos:{}", u.uuid),
                name: Self::unit_name(u),
                kind: Kind::Sonos { uuid: u.uuid.clone(), room: u.room.clone(), channel: u.channel.clone() },
                pos,
                yaw,
                model: u.model.clone(),
            });
        }
        l.add_spot("Couch");
        l.save();
    }

    fn room_changed(self: &Rc<Self>) {
        self.recompute_plan();
        // Saving and pushing to the speakers wait for the user to pause.
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if let Some(app) = w.upgrade() {
                app.room.save_pending.borrow_mut().take();
                app.room.layout.borrow().save();
            }
        });
        if let Some(old) = self.room.save_pending.borrow_mut().replace(id) {
            old.remove();
        }
        // Pushing to the speakers also waits for a pause in editing.
        self.schedule_apply();
    }

    /// Speakers counted in the plan: the selected group's Sonos boxes (or, on
    /// Bluetooth, the rooms that were grouped when it started), plus Bluetooth ones.
    pub(super) fn live_speakers(&self) -> Vec<String> {
        let bt_rooms: Vec<String> = self.output.bt_rooms.borrow().iter().map(|m| m.uuid.clone()).collect();
        let rooms: Vec<String> = if self.bt_mode() && !bt_rooms.is_empty() {
            bt_rooms
        } else {
            self.group().map(|g| g.members.iter().map(|m| m.uuid.clone()).collect()).unwrap_or_default()
        };
        self.room
            .layout
            .borrow()
            .speakers
            .iter()
            .filter(|s| match &s.kind {
                Kind::Sonos { room, .. } => rooms.contains(room),
                Kind::Bluetooth { mac } => self.bt_speaker_on(mac),
            })
            .map(|s| s.id.clone())
            .collect()
    }

    pub fn recompute_plan(self: &Rc<Self>) {
        let live = self.live_speakers();
        let plan = {
            let l = self.room.layout.borrow();
            l.active().map(|spot| plan::compute(&l, spot, |s| live.contains(&s.id)))
        };
        *self.room.view.live.borrow_mut() = live;
        *self.room.view.plan.borrow_mut() = plan.clone();
        self.room.view.area.queue_draw();
        self.render_results(plan.as_ref());
    }

    fn render_results(&self, plan: Option<&plan::Plan>) {
        let t = &self.room;
        t.results.remove_all();
        let Some(plan) = plan.filter(|p| !p.speakers.is_empty()) else {
            t.results_note.set_label("Place a speaker from the selected room and pick a listening spot.");
            return;
        };
        let l = t.layout.borrow();
        for sp in &plan.speakers {
            let Some(s) = l.speaker(&sp.id) else { continue };
            let key = plan::route_key(s);
            let mut bits = vec![format!("{:.1} m", sp.distance)];
            if let Some(steps) = plan.volume_steps.get(&key).filter(|v| **v != 0) {
                bits.push(format!("volume {steps:+}"));
            }
            if let Some((lf, rf)) = plan.balance.get(&key) {
                if let Kind::Sonos { channel: Some(ch), .. } = &s.kind {
                    let v = if ch == "LF" { lf } else { rf };
                    if *v < 100 {
                        bits.push(format!("this side {v}%"));
                    }
                }
            }
            if let Some(b) = plan.bass_steps.get(&key).filter(|b| **b != 0) {
                bits.push(format!("bass {b:+}"));
            }
            if sp.turn.abs() >= 10.0 {
                bits.push(format!("turn {:.0}° {}", sp.turn.abs(), if sp.turn > 0.0 { "right" } else { "left" }));
            }
            if sp.measured {
                bits.push("measured".into());
            }
            let row = widgets::row(&sp.name, &bits.join(" · "));
            row.set_subtitle_lines(2);
            t.results.append(&row);
        }
        let mut note = String::new();
        if plan.spread_ms >= 2.0 {
            note = format!(
                "Sound from the nearest and farthest speaker arrives {:.0} ms apart. Sonos can't delay single speakers over Wi-Fi; with Bluetooth speakers in the mix, every speaker gets its own delay.",
                plan.spread_ms
            );
        }
        if !plan.speakers.iter().any(|s| s.measured) {
            note.push_str(if note.is_empty() { "" } else { " " });
            note.push_str("These are estimates from the room model; calibrating with a microphone replaces them with measurements.");
        }
        t.results_note.set_label(&note);
    }

    fn render_spots(self: &Rc<Self>) {
        let t = &self.room;
        t.spots.remove_all();
        let (spots, active) = {
            let l = t.layout.borrow();
            (l.spots.clone(), l.active_spot.clone())
        };
        let mut first: Option<gtk::CheckButton> = None;
        for spot in spots {
            let row = adw::EntryRow::builder().title("Name").text(&spot.name).show_apply_button(true).build();
            let radio = gtk::CheckButton::builder().active(active.as_deref() == Some(spot.id.as_str())).valign(gtk::Align::Center).build();
            if let Some(f) = &first {
                radio.set_group(Some(f));
            } else {
                first = Some(radio.clone());
            }
            row.add_prefix(&radio);
            let del = gtk::Button::builder().icon_name("user-trash-symbolic").css_classes(["flat"]).valign(gtk::Align::Center).tooltip_text("Remove spot").build();
            row.add_suffix(&del);

            let (w, id) = (Rc::downgrade(self), spot.id.clone());
            radio.connect_toggled(move |r| {
                let Some(app) = w.upgrade() else { return };
                if r.is_active() {
                    app.room.layout.borrow_mut().active_spot = Some(id.clone());
                    app.room_changed();
                }
            });
            let (w, id) = (Rc::downgrade(self), spot.id.clone());
            row.connect_apply(move |r| {
                let Some(app) = w.upgrade() else { return };
                if let Some(s) = app.room.layout.borrow_mut().spots.iter_mut().find(|s| s.id == id) {
                    s.name = r.text().to_string();
                }
                app.room_changed();
            });
            let (w, id) = (Rc::downgrade(self), spot.id.clone());
            del.connect_clicked(move |_| {
                let Some(app) = w.upgrade() else { return };
                {
                    let mut l = app.room.layout.borrow_mut();
                    l.spots.retain(|s| s.id != id);
                    if l.active_spot.as_deref() == Some(id.as_str()) {
                        l.active_spot = l.spots.first().map(|s| s.id.clone());
                    }
                }
                app.render_spots();
                app.room_changed();
            });
            t.spots.append(&row);
        }
    }

    fn render_selected(self: &Rc<Self>) {
        let t = &self.room;
        while let Some(c) = t.selected.first_child() {
            t.selected.remove(&c);
        }
        let Some(pick) = t.view.selected.borrow().clone() else { return };
        let list = widgets::boxed_list();
        let (title, height, yaw) = {
            let l = t.layout.borrow();
            match &pick {
                Pick::Speaker(id) => match l.speaker(id) {
                    Some(s) => (s.name.clone(), s.pos[1], Some(s.yaw)),
                    None => return,
                },
                Pick::Spot(id) => match l.spots.iter().find(|s| &s.id == id) {
                    Some(s) => (s.name.clone(), s.pos[1], None),
                    None => return,
                },
            }
        };
        let h = adw::SpinRow::with_range(0.0, 3.0, 0.05);
        h.set_title(if yaw.is_some() { "Height (m)" } else { "Ear height (m)" });
        h.set_digits(2);
        h.set_value(height as f64);
        list.append(&h);
        let (w, p) = (Rc::downgrade(self), pick.clone());
        h.connect_notify_local(Some("value"), move |r, _| {
            let Some(app) = w.upgrade() else { return };
            {
                let mut l = app.room.layout.borrow_mut();
                let v = r.value() as f32;
                match &p {
                    Pick::Speaker(id) => l.speakers.iter_mut().filter(|s| &s.id == id).for_each(|s| s.pos[1] = v),
                    Pick::Spot(id) => l.spots.iter_mut().filter(|s| &s.id == id).for_each(|s| s.pos[1] = v),
                }
            }
            app.room.view.area.queue_draw();
            app.room_changed();
        });
        if let Some(yaw) = yaw {
            let f = adw::SpinRow::with_range(0.0, 355.0, 5.0);
            f.set_title("Facing (°)");
            f.set_subtitle("Or scroll over it in the view");
            f.set_wrap(true);
            f.set_value(yaw as f64);
            list.append(&f);
            let (w, p) = (Rc::downgrade(self), pick.clone());
            f.connect_notify_local(Some("value"), move |r, _| {
                let Some(app) = w.upgrade() else { return };
                if let Pick::Speaker(id) = &p {
                    app.room.layout.borrow_mut().speakers.iter_mut().filter(|s| &s.id == id).for_each(|s| s.yaw = r.value() as f32);
                }
                app.room.view.area.queue_draw();
                app.room_changed();
            });
        }
        t.selected.append(&group(&title, &list));
    }

    pub(super) fn render_placed(self: &Rc<Self>) {
        let t = &self.room;
        t.placed.remove_all();
        for u in t.units.borrow().iter() {
            let id = format!("sonos:{}", u.uuid);
            let placed = t.layout.borrow().speaker(&id).is_some();
            let row = adw::SwitchRow::builder().title(Self::unit_name(u)).subtitle("Sonos").active(placed).build();
            let (w, u) = (Rc::downgrade(self), u.clone());
            row.connect_active_notify(move |r| {
                let Some(app) = w.upgrade() else { return };
                {
                    let mut l = app.room.layout.borrow_mut();
                    let id = format!("sonos:{}", u.uuid);
                    if r.is_active() {
                        if l.speaker(&id).is_none() {
                            let pos = l.free_spot_for_speaker();
                            l.speakers.push(Speaker {
                                id,
                                name: App::unit_name(&u),
                                kind: Kind::Sonos { uuid: u.uuid.clone(), room: u.room.clone(), channel: u.channel.clone() },
                                pos,
                                yaw: 180.0,
                                model: u.model.clone(),
                            });
                        }
                    } else {
                        l.speakers.retain(|s| s.id != id);
                    }
                }
                app.room_changed();
            });
            t.placed.append(&row);
        }
        // Paired Bluetooth speakers can be placed too.
        for d in self.bt_devices().into_iter().filter(|d| d.paired && d.audio_sink && !d.is_sonos) {
            let id = format!("bt:{}", d.mac);
            let placed = t.layout.borrow().speaker(&id).is_some();
            let row = adw::SwitchRow::builder().title(&d.name).subtitle("Bluetooth").active(placed).build();
            let (w, d) = (Rc::downgrade(self), d.clone());
            row.connect_active_notify(move |r| {
                let Some(app) = w.upgrade() else { return };
                {
                    let mut l = app.room.layout.borrow_mut();
                    let id = format!("bt:{}", d.mac);
                    if r.is_active() {
                        if l.speaker(&id).is_none() {
                            let pos = l.free_spot_for_speaker();
                            l.speakers.push(Speaker { id, name: d.name.clone(), kind: Kind::Bluetooth { mac: d.mac.clone() }, pos, yaw: 180.0, model: String::new() });
                        }
                    } else {
                        l.speakers.retain(|s| s.id != id);
                    }
                }
                app.room_changed();
            });
            t.placed.append(&row);
        }
    }

    fn schedule_apply(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(600), move || {
            if let Some(app) = w.upgrade() {
                app.room.apply_pending.borrow_mut().take();
                app.apply_plan();
                app.update_bt_routes();
            }
        });
        if let Some(old) = self.room.apply_pending.borrow_mut().replace(id) {
            old.remove();
        }
    }

    pub(super) fn set_tuned_switch(&self, on: bool) {
        self.room.tuned.set_active(on);
    }

    pub(super) fn room_ip(&self, room_uuid: &str) -> Option<String> {
        self.all_members().into_iter().find(|m| m.uuid == room_uuid).map(|m| m.ip)
    }

    fn set_tuned(self: &Rc<Self>, on: bool) {
        self.room.layout.borrow_mut().tuned = on;
        if on {
            // Remember bass and balance before the first change.
            if self.room.layout.borrow().baseline.is_none() {
                let rooms: Vec<(String, String)> = self.group().map(|g| g.members.iter().map(|m| (m.uuid.clone(), m.ip.clone())).collect()).unwrap_or_default();
                let sonos = self.core.sonos.clone();
                let w = Rc::downgrade(self);
                spawn(
                    async move {
                        let mut b = Baseline::default();
                        for (uuid, ip) in rooms {
                            if let Ok(eq) = sonos.eq(&ip).await {
                                b.bass.insert(uuid.clone(), eq.bass);
                            }
                            if let (Ok(lf), Ok(rf)) = (sonos.channel_volume(&ip, "LF").await, sonos.channel_volume(&ip, "RF").await) {
                                b.balance.insert(uuid, (lf, rf));
                            }
                        }
                        b
                    },
                    move |b| {
                        let Some(app) = w.upgrade() else { return };
                        app.room.layout.borrow_mut().baseline = Some(b);
                        app.room.layout.borrow().save();
                        app.apply_plan();
                    },
                );
                return;
            }
            self.apply_plan();
        } else {
            self.restore_regular();
        }
        self.room.layout.borrow().save();
    }

    /// Pushes the active spot's plan onto the speakers, relative to what tuning already did.
    fn apply_plan(self: &Rc<Self>) {
        let (plan, baseline) = {
            let l = self.room.layout.borrow();
            if !l.tuned {
                return;
            }
            let live = self.live_speakers();
            let Some(spot) = l.active() else { return };
            (plan::compute(&l, spot, |s| live.contains(&s.id)), l.baseline.clone().unwrap_or_default())
        };
        let mut jobs: Vec<(String, String, i32, i32, Option<(u8, u8)>)> = Vec::new();
        for (room, steps) in &plan.volume_steps {
            let Some(ip) = self.room_ip(room) else { continue };
            let before = baseline.applied.get(room).copied().unwrap_or(0);
            let bass = baseline.bass.get(room).copied().unwrap_or(0) + plan.bass_steps.get(room).copied().unwrap_or(0);
            jobs.push((room.clone(), ip, *steps - before, bass, plan.balance.get(room).copied()));
        }
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        let steps: HashMap<String, i32> = plan.volume_steps.clone();
        spawn(
            async move {
                for (_, ip, delta, bass, balance) in &jobs {
                    if *delta != 0 {
                        let v = sonos.volume(ip).await? as i32;
                        sonos.set_volume(ip, (v + delta).clamp(0, 100) as u8).await?;
                    }
                    sonos.set_bass(ip, (*bass).clamp(-10, 10)).await?;
                    if let Some((lf, rf)) = balance {
                        sonos.set_channel_volume(ip, "LF", *lf).await?;
                        sonos.set_channel_volume(ip, "RF", *rf).await?;
                    }
                }
                anyhow::Ok(())
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(()) => {
                        if let Some(b) = app.room.layout.borrow_mut().baseline.as_mut() {
                            b.applied = steps;
                        }
                        app.room.layout.borrow().save();
                    }
                    Err(e) => app.toast(&format!("Couldn't apply tuning: {e:#}")),
                }
            },
        );
    }

    /// Undoes exactly what tuning changed.
    fn restore_regular(self: &Rc<Self>) {
        let Some(b) = self.room.layout.borrow_mut().baseline.take() else { return };
        let mut jobs = Vec::new();
        let mut rooms: Vec<&String> = b.applied.keys().chain(b.bass.keys()).chain(b.balance.keys()).collect();
        rooms.sort();
        rooms.dedup();
        for room in rooms {
            if let Some(ip) = self.room_ip(room) {
                jobs.push((ip, b.applied.get(room).copied().unwrap_or(0), b.bass.get(room).copied(), b.balance.get(room).copied()));
            }
        }
        let sonos = self.core.sonos.clone();
        self.act(async move {
            for (ip, applied, bass, balance) in jobs {
                if applied != 0 {
                    let v = sonos.volume(&ip).await? as i32;
                    sonos.set_volume(&ip, (v - applied).clamp(0, 100) as u8).await?;
                }
                if let Some(bass) = bass {
                    sonos.set_bass(&ip, bass).await?;
                }
                if let Some((lf, rf)) = balance {
                    sonos.set_channel_volume(&ip, "LF", lf).await?;
                    sonos.set_channel_volume(&ip, "RF", rf).await?;
                }
            }
            Ok(())
        });
    }
}
