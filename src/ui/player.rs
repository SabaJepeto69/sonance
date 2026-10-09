use adw::prelude::*;
use gtk::{glib, pango};
use std::cell::RefCell;
use std::time::Duration;
use std::collections::HashMap;
use std::rc::Rc;

use super::coverflow::{CardInfo, CoverFlow};
use super::{spawn, widgets, App, Job};
use crate::sonos::{self, didl::fmt_time, PlayState};

pub struct Player {
    pub bar: gtk::CenterBox,
    pub now_page: gtk::Overlay,
    pub cover: Rc<CoverFlow>,
    bar_art: gtk::Image,
    bar_title: gtk::Label,
    bar_artist: gtk::Label,
    play: gtk::Button,
    prev: gtk::Button,
    next: gtk::Button,
    shuffle: gtk::ToggleButton,
    repeat: gtk::ToggleButton,
    seek: gtk::Scale,
    pos: gtk::Label,
    dur: gtk::Label,
    mute: gtk::Button,
    /// Per-room volumes, shown while hovering the volume icon.
    room_pop: gtk::Popover,
    room_list: gtk::ListBox,
    room_close: RefCell<Option<glib::SourceId>>,
    volume: gtk::Scale,
    sleep: gtk::MenuButton,
    sleep_pop: gtk::Popover,
    now_title: gtk::Label,
    now_artist: gtk::Label,
    now_album: gtk::Label,
    art_url: RefCell<Option<String>>,
}

fn label(classes: &[&str], xalign: f32) -> gtk::Label {
    gtk::Label::builder().xalign(xalign).ellipsize(pango::EllipsizeMode::End).css_classes(classes.to_vec()).build()
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tip).css_classes(["flat", "circular"]).valign(gtk::Align::Center).build()
}

fn volume_scale(width: i32) -> gtk::Scale {
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    s.set_width_request(width);
    s.set_hexpand(true);
    s.set_valign(gtk::Align::Center);
    s
}

impl Player {
    pub fn new() -> Self {
        // --- bottom bar ---
        let bar_art = widgets::thumb(52);
        let bar_title = label(&["heading"], 0.0);
        let bar_artist = label(&["dim-label", "caption"], 0.0);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.append(&bar_title);
        text.append(&bar_artist);
        text.set_width_request(160);
        let start = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        start.append(&bar_art);
        start.append(&text);

        let shuffle = gtk::ToggleButton::builder().icon_name("media-playlist-shuffle-symbolic").tooltip_text("Shuffle").css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();
        let prev = icon_button("media-skip-backward-symbolic", "Previous");
        let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play").css_classes(["circular", "suggested-action", "play"]).valign(gtk::Align::Center).build();
        let next = icon_button("media-skip-forward-symbolic", "Next");
        let repeat = gtk::ToggleButton::builder().icon_name("media-playlist-repeat-symbolic").tooltip_text("Repeat").css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.set_halign(gtk::Align::Center);
        for w in [shuffle.upcast_ref::<gtk::Widget>(), prev.upcast_ref(), play.upcast_ref(), next.upcast_ref(), repeat.upcast_ref()] {
            controls.append(w);
        }
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        seek.set_hexpand(true);
        let pos = label(&["caption", "numeric", "dim-label"], 1.0);
        pos.set_width_chars(6);
        let dur = label(&["caption", "numeric", "dim-label"], 0.0);
        dur.set_width_chars(6);
        let seek_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        seek_row.append(&pos);
        seek_row.append(&seek);
        seek_row.append(&dur);
        let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
        center.set_width_request(380);
        center.append(&controls);
        center.append(&seek_row);

        let mute = icon_button("audio-volume-high-symbolic", "Mute");
        let room_list = widgets::boxed_list();
        let room_col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        room_col.set_width_request(320);
        room_col.append(&widgets::heading("Room volumes"));
        room_col.append(&room_list);
        // Not autohide: a hover pop-up must not grab the pointer, and it closes itself on leave.
        let room_pop = gtk::Popover::builder().child(&room_col).autohide(false).has_arrow(false).position(gtk::PositionType::Top).build();
        room_pop.set_parent(&mute);
        super::glass::popover(&room_pop);
        let volume = volume_scale(130);
        volume.set_tooltip_text(Some("Group volume"));
        let sleep_pop = gtk::Popover::new();
        let sleep = gtk::MenuButton::builder().icon_name("weather-clear-night-symbolic").tooltip_text("Sleep timer").css_classes(["flat"]).valign(gtk::Align::Center).popover(&sleep_pop).build();
        let end = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        end.append(&mute);
        end.append(&volume);
        end.append(&sleep);

        let bar = gtk::CenterBox::new();
        bar.add_css_class("capsule");
        bar.set_valign(gtk::Align::End);
        bar.set_start_widget(Some(&start));
        bar.set_center_widget(Some(&center));
        bar.set_end_widget(Some(&end));

        // --- now playing page ---
        let cover = CoverFlow::new();
        cover.root.set_margin_top(28);
        let now_title = label(&["now-title"], 0.5);
        now_title.set_wrap(true);
        now_title.set_lines(2);
        now_title.set_justify(gtk::Justification::Center);
        let now_artist = label(&["now-artist"], 0.5);
        let now_album = label(&["now-album"], 0.5);
        let info = gtk::Box::new(gtk::Orientation::Vertical, 4);
        info.append(&now_title);
        info.append(&now_artist);
        info.append(&now_album);
        let clamp = adw::Clamp::builder().maximum_size(640).child(&info).margin_top(18).margin_bottom(130).margin_start(16).margin_end(16).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        col.append(&cover.root);
        col.append(&clamp);
        let scroll = gtk::ScrolledWindow::builder().child(&col).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let now_page = gtk::Overlay::new();
        now_page.set_child(Some(&scroll));

        Self {
            bar,
            now_page,
            bar_art,
            bar_title,
            bar_artist,
            play,
            prev,
            next,
            shuffle,
            repeat,
            seek,
            pos,
            dur,
            mute,
            room_pop,
            room_list,
            room_close: RefCell::default(),
            volume,
            sleep,
            sleep_pop,
            cover,
            now_title,
            now_artist,
            now_album,
            art_url: RefCell::default(),
        }
    }

    pub fn set_message(&self, msg: &str) {
        self.now_title.set_label(msg);
        self.now_artist.set_label("");
        self.now_album.set_label("");
        self.bar_title.set_label("Sonance");
        self.bar_artist.set_label(msg);
    }

    /// Syncs every control to `app.status`.
    pub fn update(&self, app: &Rc<App>) {
        let s = app.status.borrow().clone();
        let has_track = !s.title.is_empty();
        let title = if has_track { s.title.clone() } else { "Nothing playing".into() };
        self.bar_title.set_label(&title);
        self.bar_artist.set_label(&s.artist);
        // While the carousel moves, the titles follow it instead.
        if !self.cover.busy() {
            self.now_title.set_label(&title);
            self.now_artist.set_label(&s.artist);
            self.now_album.set_label(&s.album);
        }

        if *self.art_url.borrow() != s.art {
            *self.art_url.borrow_mut() = s.art.clone();
            app.load_art(s.art.clone(), &self.bar_art);
        }

        let playing = matches!(s.state, PlayState::Playing | PlayState::Transitioning);
        self.play.set_icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
        self.play.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
        for b in [&self.prev, &self.next] {
            b.set_sensitive(s.from_queue);
        }
        self.shuffle.set_sensitive(s.from_queue);
        self.repeat.set_sensitive(s.from_queue);
        self.shuffle.set_active(s.shuffle());
        self.repeat.set_active(s.repeat() != "off");
        self.repeat.set_icon_name(if s.repeat() == "one" { "media-playlist-repeat-song-symbolic" } else { "media-playlist-repeat-symbolic" });

        let seekable = s.duration > 0;
        self.seek.set_sensitive(seekable);
        if !app.recently_touched("seek") {
            self.seek.set_range(0.0, s.duration.max(1) as f64);
            self.seek.set_value(s.position as f64);
            self.pos.set_label(if seekable { fmt_time(s.position) } else { String::new() }.as_str());
        }
        self.dur.set_label(if seekable { fmt_time(s.duration) } else { String::new() }.as_str());

        if !app.recently_touched("gvol") {
            self.volume.set_value(s.volume as f64);
        }
        self.mute.set_icon_name(match (s.muted, s.volume) {
            (true, _) => "audio-volume-muted-symbolic",
            (_, 0..=33) => "audio-volume-low-symbolic",
            (_, 34..=66) => "audio-volume-medium-symbolic",
            _ => "audio-volume-high-symbolic",
        });
    }

    pub fn set_sleep_remaining(&self, secs: Option<u32>) {
        match secs {
            Some(s) => {
                self.sleep.set_label(&format!("{}m", s.div_ceil(60)));
                self.sleep.set_tooltip_text(Some(&format!("Sleep timer: {} left", fmt_time(s))));
            }
            None => {
                self.sleep.set_icon_name("weather-clear-night-symbolic");
                self.sleep.set_tooltip_text(Some("Sleep timer"));
            }
        }
    }
}

impl App {
    pub(super) fn connect_player(self: &Rc<Self>) {
        let p = &self.player;

        let w = Rc::downgrade(self);
        p.cover.set_loader(Rc::new(move |url, pic| {
            if let Some(app) = w.upgrade() {
                app.load_art(url, pic);
            }
        }));
        let w = Rc::downgrade(self);
        p.cover.set_on_focus(Rc::new(move |info| {
            let (Some(app), Some(info)) = (w.upgrade(), info) else { return };
            if app.player.cover.busy() {
                app.player.now_title.set_label(&info.title);
                app.player.now_artist.set_label(&info.artist);
                app.player.now_album.set_label("");
            }
        }));
        // Landing on another cover plays it.
        let w = Rc::downgrade(self);
        p.cover.set_on_commit(Rc::new(move |from, to| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let s = app.status.borrow().clone();
            let sonos = app.core.sonos.clone();
            if s.from_queue && !s.shuffle() {
                app.act(async move { sonos.play_from_queue(&c, to as u32).await });
            } else {
                app.act(async move {
                    for _ in 0..(to - from).abs() {
                        if to > from { sonos.next(&c.ip).await? } else { sonos.previous(&c.ip).await? }
                    }
                    Ok(())
                });
            }
        }));

        let w = Rc::downgrade(self);
        p.play.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let playing = matches!(app.status.borrow().state, PlayState::Playing | PlayState::Transitioning);
            let sonos = app.core.sonos.clone();
            app.act(async move { if playing { sonos.pause(&c.ip).await } else { sonos.play(&c.ip).await } });
        });

        for (btn, forward) in [(&p.prev, false), (&p.next, true)] {
            let w = Rc::downgrade(self);
            btn.connect_clicked(move |_| {
                let Some(app) = w.upgrade() else { return };
                let Some(c) = app.coord() else { return };
                let sonos = app.core.sonos.clone();
                app.act(async move { if forward { sonos.next(&c.ip).await } else { sonos.previous(&c.ip).await } });
            });
        }

        // Toggle buttons flip themselves on click; derive the new mode from the
        // speaker's state instead, and let the next poll set the buttons.
        let w = Rc::downgrade(self);
        p.shuffle.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let s = app.status.borrow().clone();
            let mode = sonos::play_mode(!s.shuffle(), s.repeat());
            let sonos = app.core.sonos.clone();
            app.act(async move { sonos.set_play_mode(&c.ip, mode).await });
        });
        let w = Rc::downgrade(self);
        p.repeat.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let s = app.status.borrow().clone();
            let next = match s.repeat() {
                "off" => "all",
                "all" => "one",
                _ => "off",
            };
            let mode = sonos::play_mode(s.shuffle(), next);
            let sonos = app.core.sonos.clone();
            app.act(async move { sonos.set_play_mode(&c.ip, mode).await });
        });

        let w = Rc::downgrade(self);
        p.seek.connect_change_value(move |_, _, v| {
            let Some(app) = w.upgrade() else { return glib::Propagation::Proceed };
            let Some(c) = app.coord() else { return glib::Propagation::Proceed };
            app.player.pos.set_label(&fmt_time(v.max(0.0) as u32));
            let sonos = app.core.sonos.clone();
            let job: Job = Rc::new(move |v| {
                let (sonos, ip) = (sonos.clone(), c.ip.clone());
                Box::pin(async move { sonos.seek(&ip, v.max(0.0) as u32).await })
            });
            app.throttled("seek", v, job);
            glib::Propagation::Proceed
        });

        let w = Rc::downgrade(self);
        p.volume.connect_change_value(move |_, _, v| {
            let Some(app) = w.upgrade() else { return glib::Propagation::Proceed };
            let Some(c) = app.coord() else { return glib::Propagation::Proceed };
            let sonos = app.core.sonos.clone();
            let drag_start = !app.recently_touched("gvol");
            let job: Job = Rc::new(move |v| {
                let (sonos, ip) = (sonos.clone(), c.ip.clone());
                Box::pin(async move { sonos.set_group_volume(&ip, v as u8, drag_start).await })
            });
            app.throttled("gvol", v.clamp(0.0, 100.0), job);
            glib::Propagation::Proceed
        });

        // Hovering the volume icon (or the pop-up itself) keeps the room volumes open.
        for widget in [p.mute.upcast_ref::<gtk::Widget>(), p.room_pop.upcast_ref()] {
            let motion = gtk::EventControllerMotion::new();
            let w = Rc::downgrade(self);
            motion.connect_enter(move |_, _, _| {
                if let Some(app) = w.upgrade() {
                    app.open_room_volumes();
                }
            });
            let w = Rc::downgrade(self);
            motion.connect_leave(move |_| {
                if let Some(app) = w.upgrade() {
                    app.close_room_volumes_soon();
                }
            });
            widget.add_controller(motion);
        }

        let w = Rc::downgrade(self);
        p.mute.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let muted = app.status.borrow().muted;
            let sonos = app.core.sonos.clone();
            app.act(async move { sonos.set_group_mute(&c.ip, !muted).await });
        });

        // Sleep timer choices.
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bx.append(&widgets::heading("Sleep timer"));
        for (label, mins) in [("Off", 0), ("15 minutes", 15), ("30 minutes", 30), ("45 minutes", 45), ("1 hour", 60), ("1½ hours", 90), ("2 hours", 120)] {
            let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let w = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                let Some(app) = w.upgrade() else { return };
                app.player.sleep_pop.popdown();
                let Some(c) = app.coord() else { return };
                let sonos = app.core.sonos.clone();
                let secs = (mins > 0).then_some(mins * 60);
                let w2 = Rc::downgrade(&app);
                spawn(async move { sonos.set_sleep_timer(&c.ip, secs).await }, move |r| {
                    let Some(app) = w2.upgrade() else { return };
                    match r {
                        Ok(()) => {
                            app.toast(if mins == 0 { "Sleep timer off" } else { "Sleep timer set" });
                            app.refresh_sleep_timer();
                        }
                        Err(e) => app.toast(&format!("{e:#}")),
                    }
                });
            });
            bx.append(&b);
        }
        p.sleep_pop.set_child(Some(&bx));
        super::glass::popover(&p.sleep_pop);
    }

    fn open_room_volumes(self: &Rc<Self>) {
        let p = &self.player;
        if let Some(id) = p.room_close.borrow_mut().take() {
            id.remove();
        }
        if p.room_pop.is_visible() {
            return;
        }
        let Some(g) = self.group() else { return };
        p.room_list.remove_all();
        let mut scales = Vec::new();
        for m in &g.members {
            let row = widgets::row(&m.name, "");
            let scale = volume_scale(170);
            scale.set_sensitive(false);
            row.add_suffix(&scale);
            let key = format!("vol:{}", m.ip);
            let (sonos, ip) = (self.core.sonos.clone(), m.ip.clone());
            let job: Job = Rc::new(move |v| {
                let (sonos, ip) = (sonos.clone(), ip.clone());
                Box::pin(async move { sonos.set_volume(&ip, v as u8).await })
            });
            let w = Rc::downgrade(self);
            scale.connect_change_value(move |_, _, v| {
                if let Some(a) = w.upgrade() {
                    a.throttled(&key, v.clamp(0.0, 100.0), job.clone());
                }
                glib::Propagation::Proceed
            });
            p.room_list.append(&row);
            scales.push((m.ip.clone(), scale));
        }
        p.room_pop.popup();
        // Fill in the real levels; sliders stay disabled until they arrive.
        let sonos = self.core.sonos.clone();
        let ips: Vec<String> = scales.iter().map(|(ip, _)| ip.clone()).collect();
        spawn(
            async move {
                let mut out = Vec::new();
                for ip in ips {
                    out.push(sonos.volume(&ip).await.ok());
                }
                out
            },
            move |vols| {
                for ((_, scale), v) in scales.iter().zip(vols) {
                    if let Some(v) = v {
                        scale.set_value(v as f64);
                        scale.set_sensitive(true);
                    }
                }
            },
        );
    }

    /// A short grace period lets the pointer travel from the icon to the pop-up.
    fn close_room_volumes_soon(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(350), move || {
            if let Some(app) = w.upgrade() {
                app.player.room_close.borrow_mut().take();
                app.player.room_pop.popdown();
            }
        });
        if let Some(old) = self.player.room_close.borrow_mut().replace(id) {
            old.remove();
        }
    }

    /// Fetches the songs around the current one for the carousel.
    pub fn refresh_cover(self: &Rc<Self>) {
        let s = self.status.borrow().clone();
        let current = CardInfo { title: s.title.clone(), artist: s.artist.clone(), art: s.art.clone() };
        let cover = self.player.cover.clone();
        if !s.from_queue || s.track_no == 0 {
            // Radio, TV, line-in: one card, nothing to swipe to.
            let pos = s.track_no as i64;
            cover.set_items(pos, HashMap::from([(pos, current)]), false, false);
            return;
        }
        let center = s.track_no as i64;
        if s.shuffle() {
            // Sonos keeps the shuffled order to itself; skipping still works.
            cover.set_items(center, HashMap::from([(center, current)]), true, true);
            return;
        }
        let Some(c) = self.coord() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        let start = (s.track_no.saturating_sub(3)) as u32;
        let uuid = c.uuid.clone();
        spawn(async move { sonos.queue_slice(&c.ip, start, 5).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            if app.selected.borrow().as_deref() != Some(uuid.as_str()) || app.status.borrow().track_no as i64 != center {
                return;
            }
            let Ok(slice) = r else { return };
            let mut items: HashMap<i64, CardInfo> = slice
                .into_iter()
                .enumerate()
                .map(|(i, it)| (start as i64 + 1 + i as i64, CardInfo { title: it.title, artist: it.artist, art: it.art }))
                .collect();
            items.insert(center, current);
            app.player.cover.set_items(center, items, true, false);
        });
    }

    pub fn refresh_sleep_timer(self: &Rc<Self>) {
        let Some(c) = self.coord() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        let uuid = c.uuid.clone();
        spawn(async move { sonos.sleep_timer(&c.ip).await }, move |r| {
            if let (Some(app), Ok(secs)) = (w.upgrade(), r) {
                if app.selected.borrow().as_deref() == Some(uuid.as_str()) {
                    app.player.set_sleep_remaining(secs);
                }
            }
        });
    }

}
