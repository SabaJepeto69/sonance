use adw::prelude::*;
use std::rc::Rc;

use super::{spawn, widgets, App};
use crate::sonos::{self, Alarm, BUZZER};

pub struct AlarmsView {
    pub root: gtk::ScrolledWindow,
    list: gtk::ListBox,
    add: gtk::Button,
    empty: adw::StatusPage,
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const REPEATS: [(&str, &str); 4] = [("ONCE", "Once"), ("DAILY", "Every day"), ("WEEKDAYS", "Weekdays"), ("WEEKENDS", "Weekends")];
const DURATIONS: [(&str, &str); 5] = [("00:10:00", "10 minutes"), ("00:30:00", "30 minutes"), ("01:00:00", "1 hour"), ("02:00:00", "2 hours"), ("04:00:00", "4 hours")];

fn describe_recurrence(r: &str) -> String {
    if let Some((_, label)) = REPEATS.iter().find(|(k, _)| *k == r) {
        return label.to_string();
    }
    match r.strip_prefix("ON_") {
        Some(days) => days.chars().filter_map(|c| c.to_digit(10)).filter_map(|d| DAYS.get(d as usize)).copied().collect::<Vec<_>>().join(", "),
        None => r.to_string(),
    }
}

impl AlarmsView {
    pub fn new() -> Self {
        let add = gtk::Button::builder().label("New alarm").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).margin_top(12).build();
        let list = widgets::boxed_list();
        let empty = adw::StatusPage::builder().icon_name("alarm-symbolic").title("No alarms").visible(false).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.append(&empty);
        col.append(&list);
        col.append(&add);
        Self { root: widgets::page(&col), list, add, empty }
    }
}

impl App {
    pub(super) fn connect_alarms(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.alarms.add.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let room = app.coord().map(|c| c.uuid).unwrap_or_default();
            let a = Alarm {
                start: "07:00:00".into(),
                duration: "01:00:00".into(),
                recurrence: "WEEKDAYS".into(),
                enabled: true,
                room_uuid: room,
                program_uri: BUZZER.into(),
                play_mode: "NORMAL".into(),
                volume: 20,
                ..Default::default()
            };
            app.edit_alarm(a);
        });
    }

    pub fn refresh_alarms(self: &Rc<Self>) {
        let Some(ip) = self.any_ip() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(async move { sonos.alarms(&ip).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            match r {
                Ok(alarms) => app.render_alarms(alarms),
                Err(e) => app.toast(&format!("{e:#}")),
            }
        });
    }

    fn room_name(&self, uuid: &str) -> String {
        self.all_members().into_iter().find(|m| m.uuid == uuid).map(|m| m.name).unwrap_or_else(|| "Unknown room".into())
    }

    fn program_name(&self, a: &Alarm) -> String {
        if a.program_uri.is_empty() {
            return "Default sound".into();
        }
        if a.program_uri == BUZZER {
            return "Chime".into();
        }
        sonos::xml::tag(&a.program_meta, "dc:title")
            .or_else(|| self.favorites.borrow().iter().find(|f| f.uri == a.program_uri).map(|f| f.title.clone()))
            .unwrap_or_else(|| "Music".into())
    }

    fn render_alarms(self: &Rc<Self>, alarms: Vec<Alarm>) {
        let v = &self.alarms;
        v.list.remove_all();
        v.empty.set_visible(alarms.is_empty());
        v.list.set_visible(!alarms.is_empty());
        for a in alarms {
            let sub = format!("{} · {} · {} · volume {}", self.room_name(&a.room_uuid), describe_recurrence(&a.recurrence), self.program_name(&a), a.volume);
            let row = { let r = widgets::row(a.start.get(..5).unwrap_or(&a.start), &sub); r.set_activatable(true); r };
            let sw = gtk::Switch::builder().active(a.enabled).valign(gtk::Align::Center).build();
            let alarm = a.clone();
            let w = Rc::downgrade(self);
            sw.connect_state_set(move |_, on| {
                if let Some(app) = w.upgrade() {
                    let mut a = alarm.clone();
                    a.enabled = on;
                    app.save_alarm(a, None);
                }
                gtk::glib::Propagation::Proceed
            });
            row.add_suffix(&sw);
            let w = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(app) = w.upgrade() {
                    app.edit_alarm(a.clone());
                }
            });
            v.list.append(&row);
        }
    }

    fn save_alarm(self: &Rc<Self>, a: Alarm, dialog: Option<adw::Dialog>) {
        let Some(ip) = self.any_ip() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(async move { sonos.save_alarm(&ip, &a).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            match r {
                Ok(()) => {
                    if let Some(d) = dialog {
                        d.close();
                    }
                }
                Err(e) => app.toast(&format!("Couldn't save the alarm: {e:#}")),
            }
            app.refresh_alarms();
        });
    }

    fn edit_alarm(self: &Rc<Self>, a: Alarm) {
        let is_new = a.id.is_empty();
        let page = adw::PreferencesPage::new();

        let when = adw::PreferencesGroup::builder().title("When").build();
        let (h, m) = {
            let mut it = a.start.split(':').map(|x| x.parse::<f64>().unwrap_or(0.0));
            (it.next().unwrap_or(7.0), it.next().unwrap_or(0.0))
        };
        let hour = adw::SpinRow::with_range(0.0, 23.0, 1.0);
        hour.set_title("Hour");
        hour.set_value(h);
        let minute = adw::SpinRow::with_range(0.0, 59.0, 1.0);
        minute.set_title("Minute");
        minute.set_value(m);
        let repeat = adw::ComboRow::builder().title("Repeat").build();
        let mut repeat_labels: Vec<&str> = REPEATS.iter().map(|(_, l)| *l).collect();
        repeat_labels.push("Certain days");
        repeat.set_model(Some(&gtk::StringList::new(&repeat_labels)));
        let custom = a.recurrence.strip_prefix("ON_").unwrap_or("").to_string();
        repeat.set_selected(REPEATS.iter().position(|(k, _)| *k == a.recurrence).unwrap_or(4) as u32);
        let days_row = adw::ActionRow::builder().title("Days").build();
        let days_box = gtk::Box::builder().spacing(4).valign(gtk::Align::Center).css_classes(["linked"]).build();
        let day_buttons: Vec<gtk::ToggleButton> = DAYS
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let b = gtk::ToggleButton::builder().label(&d[..2]).active(custom.contains(char::from(b'0' + i as u8))).build();
                days_box.append(&b);
                b
            })
            .collect();
        days_row.add_suffix(&days_box);
        days_row.set_visible(repeat.selected() == 4);
        let dr = days_row.clone();
        repeat.connect_selected_notify(move |r| dr.set_visible(r.selected() == 4));
        for r in [hour.upcast_ref::<gtk::Widget>(), minute.upcast_ref(), repeat.upcast_ref(), days_row.upcast_ref()] {
            when.add(r);
        }

        let what = adw::PreferencesGroup::builder().title("Where and what").build();
        let mut rooms: Vec<(String, String)> = self.all_members().into_iter().map(|m| (m.name, m.uuid)).collect();
        if !a.room_uuid.is_empty() && !rooms.iter().any(|r| r.1 == a.room_uuid) {
            rooms.push(("Current room (not found now)".into(), a.room_uuid.clone()));
        }
        let room = adw::ComboRow::builder().title("Room").build();
        room.set_model(Some(&gtk::StringList::new(&rooms.iter().map(|r| r.0.as_str()).collect::<Vec<_>>())));
        room.set_selected(rooms.iter().position(|r| r.1 == a.room_uuid).unwrap_or(0) as u32);
        // Sound: the chime, the alarm's current music, or any playable favorite.
        let mut sounds: Vec<(String, String, String)> = vec![("Sonos chime".into(), BUZZER.into(), String::new())];
        if a.program_uri.is_empty() && !is_new {
            sounds.insert(0, ("Keep current sound".into(), String::new(), String::new()));
        } else if !a.program_uri.is_empty() && a.program_uri != BUZZER {
            sounds.push((format!("{} (current)", self.program_name(&a)), a.program_uri.clone(), a.program_meta.clone()));
        }
        for f in self.favorites.borrow().iter().filter(|f| !f.uri.is_empty() && f.uri != a.program_uri) {
            sounds.push((f.title.clone(), f.uri.clone(), f.meta.clone()));
        }
        let sound = adw::ComboRow::builder().title("Sound").build();
        sound.set_model(Some(&gtk::StringList::new(&sounds.iter().map(|s| s.0.as_str()).collect::<Vec<_>>())));
        sound.set_selected(sounds.iter().position(|s| s.1 == a.program_uri).unwrap_or(0) as u32);
        let volume = adw::SpinRow::with_range(0.0, 100.0, 1.0);
        volume.set_title("Volume");
        volume.set_value(a.volume as f64);
        let mut durations: Vec<(String, String)> = DURATIONS.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect();
        if !a.duration.is_empty() && !durations.iter().any(|d| d.0 == a.duration) {
            durations.push((a.duration.clone(), format!("{} (current)", a.duration)));
        }
        let duration = adw::ComboRow::builder().title("Play for").build();
        duration.set_model(Some(&gtk::StringList::new(&durations.iter().map(|d| d.1.as_str()).collect::<Vec<_>>())));
        duration.set_selected(durations.iter().position(|d| d.0 == a.duration).unwrap_or(2) as u32);
        let shuffle = adw::SwitchRow::builder().title("Shuffle").active(a.play_mode.starts_with("SHUFFLE")).build();
        let linked = adw::SwitchRow::builder().title("Include grouped rooms").active(a.include_linked).build();
        let enabled = adw::SwitchRow::builder().title("Enabled").active(a.enabled).build();
        for r in [room.upcast_ref::<gtk::Widget>(), sound.upcast_ref(), volume.upcast_ref(), duration.upcast_ref(), shuffle.upcast_ref(), linked.upcast_ref(), enabled.upcast_ref()] {
            what.add(r);
        }
        page.add(&when);
        page.add(&what);

        let dialog = adw::Dialog::builder().title(if is_new { "New alarm" } else { "Edit alarm" }).content_width(460).content_height(640).build();
        if !is_new {
            let del_group = adw::PreferencesGroup::new();
            let del = gtk::Button::builder().label("Delete alarm").css_classes(["destructive-action", "pill"]).halign(gtk::Align::Center).build();
            del_group.add(&del);
            page.add(&del_group);
            let w = Rc::downgrade(self);
            let (id, d) = (a.id.clone(), dialog.downgrade());
            del.connect_clicked(move |_| {
                let Some(app) = w.upgrade() else { return };
                let Some(ip) = app.any_ip() else { return };
                let sonos = app.core.sonos.clone();
                let (id, d, w2) = (id.clone(), d.clone(), Rc::downgrade(&app));
                spawn(async move { sonos.delete_alarm(&ip, &id).await }, move |r| {
                    let Some(app) = w2.upgrade() else { return };
                    match r {
                        Ok(()) => {
                            if let Some(d) = d.upgrade() {
                                d.close();
                            }
                        }
                        Err(e) => app.toast(&format!("{e:#}")),
                    }
                    app.refresh_alarms();
                });
            });
        }

        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        header.set_show_start_title_buttons(false);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
        header.pack_start(&cancel);
        header.pack_end(&save);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&page));
        dialog.set_child(Some(&tv));
        super::glass::dialog(&dialog);

        let d = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(d) = d.upgrade() {
                d.close();
            }
        });
        let w = Rc::downgrade(self);
        let d = dialog.downgrade();
        save.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let mut out = a.clone();
            out.start = format!("{:02}:{:02}:00", hour.value() as u32, minute.value() as u32);
            out.recurrence = match repeat.selected() as usize {
                i if i < REPEATS.len() => REPEATS[i].0.to_string(),
                _ => {
                    let days: String = day_buttons.iter().enumerate().filter(|(_, b)| b.is_active()).map(|(i, _)| char::from(b'0' + i as u8)).collect();
                    if days.is_empty() {
                        app.toast("Pick at least one day");
                        return;
                    }
                    format!("ON_{days}")
                }
            };
            if let Some((_, uuid)) = rooms.get(room.selected() as usize) {
                out.room_uuid = uuid.clone();
            }
            if let Some((_, uri, meta)) = sounds.get(sound.selected() as usize) {
                out.program_uri = uri.clone();
                out.program_meta = meta.clone();
            }
            out.volume = volume.value() as u8;
            out.duration = durations.get(duration.selected() as usize).map(|d| d.0.clone()).unwrap_or(out.duration);
            // Keep modes this dialog doesn't show (e.g. REPEAT_ALL) unless shuffle was flipped.
            if shuffle.is_active() != a.play_mode.starts_with("SHUFFLE") {
                out.play_mode = if shuffle.is_active() { "SHUFFLE_NOREPEAT" } else { "NORMAL" }.into();
            }
            out.include_linked = linked.is_active();
            out.enabled = enabled.is_active();
            app.save_alarm(out, d.upgrade());
        });
        dialog.present(Some(&self.window));
    }
}
