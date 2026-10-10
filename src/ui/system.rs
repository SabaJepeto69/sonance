//! Desktop integration: the top-bar island's D-Bus controls, live
//! events from the speakers, running in the background, and start at login.

use adw::prelude::*;
use gtk::glib;
use std::path::PathBuf;
use std::cell::RefCell;
use std::rc::Rc;

use super::{spawn, widgets, App};
use crate::island;
use crate::mpris::{Cmd, Mpris, State};
use crate::sonos::events::{Event, Events, Kind};
use crate::sonos::{self, PlayState};

fn autostart_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("autostart").join("dev.sonance.Sonance.desktop")
}

fn set_autostart(on: bool) -> std::io::Result<()> {
    let path = autostart_file();
    if !on {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Sonance\nComment=Sonos controller (in the background)\nExec={} --background\nIcon=audio-speakers\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            exe.display()
        ),
    )
}

impl App {
    pub(super) fn connect_system(self: &Rc<Self>) {
        // Island controls: commands arrive on a channel and are handled on the GTK thread.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let w = Rc::downgrade(self);
        spawn(async move { Mpris::start(tx).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            match r {
                Ok(m) => {
                    *app.mpris.borrow_mut() = Some(m);
                    app.mpris_sync();
                }
                Err(e) => eprintln!("top-bar controls unavailable: {e:#}"),
            }
        });
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Some(cmd) = rx.recv().await {
                let Some(app) = w.upgrade() else { break };
                app.handle_mpris(cmd);
            }
        });

        // Speaker events.
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let http = self.core.sonos.soap.http().clone();
        let w = Rc::downgrade(self);
        spawn(async move { Events::start(http, etx).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            match r {
                Ok(ev) => {
                    *app.events.borrow_mut() = Some(ev);
                    app.resubscribe();
                }
                Err(e) => eprintln!("live updates unavailable, polling instead: {e:#}"),
            }
        });
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Some(ev) = erx.recv().await {
                let Some(app) = w.upgrade() else { break };
                app.handle_event(ev);
            }
        });
    }

    /// True once speakers are calling back, so the once-a-second poll can relax.
    pub(super) fn live(&self) -> bool {
        self.events.borrow().as_ref().is_some_and(|e| e.heard())
    }

    /// Subscribes to the selected group's coordinator, plus topology from any speaker.
    pub(super) fn resubscribe(self: &Rc<Self>) {
        let Some(ev) = self.events.borrow().clone() else { return };
        let mut wanted = Vec::new();
        if let Some(c) = self.coord() {
            for k in [Kind::Transport, Kind::GroupVolume, Kind::Queue] {
                wanted.push((c.ip.clone(), k));
            }
        }
        if let Some(ip) = self.any_ip() {
            wanted.push((ip, Kind::Topology));
        }
        crate::rt().spawn(async move { ev.set(&wanted).await });
    }

    fn handle_event(self: &Rc<Self>, ev: Event) {
        // Late callbacks from a group we've since switched away from.
        let from_coord = self.coord().is_some_and(|c| c.ip == ev.ip);
        if matches!(ev.kind, Kind::Transport | Kind::GroupVolume | Kind::Queue) && !from_coord {
            return;
        }
        match ev.kind {
            Kind::Transport | Kind::GroupVolume => self.poll_status(),
            Kind::Queue => {
                if self.stack.visible_child_name().as_deref() == Some("queue") {
                    self.refresh_queue(false);
                }
                self.refresh_cover();
            }
            Kind::Topology => self.refresh_topology(),
        }
    }

    /// Between polls, move the clock along locally while playing.
    pub(super) fn advance_position(self: &Rc<Self>) {
        {
            let mut s = self.status.borrow_mut();
            if s.state != PlayState::Playing || s.duration == 0 {
                return;
            }
            s.position = s.position.saturating_add(1).min(s.duration);
        }
        self.extras.status_at.set(Some(std::time::Instant::now()));
        self.player.update(self);
        self.mpris_sync();
    }

    /// Publishes the current state to the island.
    pub(super) fn mpris_sync(&self) {
        let Some(m) = self.mpris.borrow().clone() else { return };
        let s = self.status.borrow().clone();
        let state = State {
            playing: matches!(s.state, PlayState::Playing | PlayState::Transitioning),
            paused: s.state == PlayState::Paused,
            title: s.title.clone(),
            artist: s.artist.clone(),
            album: s.album.clone(),
            room: self.group().map(|g| g.name()).unwrap_or_default(),
            art: s.art.clone(),
            track_no: s.track_no,
            length_us: s.duration as i64 * 1_000_000,
            position_us: s.position as i64 * 1_000_000,
            volume: s.volume as f64 / 100.0,
            shuffle: s.shuffle(),
            loop_status: match s.repeat() {
                "one" => "Track",
                "all" => "Playlist",
                _ => "None",
            }
            .into(),
            can_skip: s.from_queue,
        };
        crate::rt().spawn(async move {
            let _ = m.update(state).await;
        });
    }

    fn handle_mpris(self: &Rc<Self>, cmd: Cmd) {
        if let Cmd::Raise = cmd {
            self.window.present();
            return;
        }
        if let Cmd::Action(a) = &cmd {
            match a.as_str() {
                "handoff" => self.handoff(),
                "sleep-extend" => self.extend_sleep(),
                _ => self.window.present(),
            }
            return;
        }
        if let Cmd::Quit = cmd {
            self.quit();
            return;
        }
        let Some(c) = self.coord() else { return };
        let s = self.status.borrow().clone();
        let sonos = self.core.sonos.clone();
        let playing = matches!(s.state, PlayState::Playing | PlayState::Transitioning);
        match cmd {
            Cmd::PlayPause => self.act(async move { if playing { sonos.pause(&c.ip).await } else { sonos.play(&c.ip).await } }),
            Cmd::Play => self.act(async move { sonos.play(&c.ip).await }),
            Cmd::Pause | Cmd::Stop => self.act(async move { sonos.pause(&c.ip).await }),
            Cmd::Next => self.act(async move { sonos.next(&c.ip).await }),
            Cmd::Previous => self.act(async move { sonos.previous(&c.ip).await }),
            Cmd::Seek(off) => {
                let to = (s.position as i64 + off / 1_000_000).clamp(0, s.duration as i64) as u32;
                self.act(async move { sonos.seek(&c.ip, to).await });
            }
            Cmd::SetPosition(p) => {
                let to = (p / 1_000_000).clamp(0, s.duration as i64) as u32;
                self.act(async move { sonos.seek(&c.ip, to).await });
            }
            Cmd::Volume(v) => {
                let vol = (v * 100.0).round() as u8;
                self.act(async move { sonos.set_group_volume(&c.ip, vol, true).await });
            }
            Cmd::Shuffle(on) => {
                let mode = sonos::play_mode(on, s.repeat());
                self.act(async move { sonos.set_play_mode(&c.ip, mode).await });
            }
            Cmd::Loop(l) => {
                let rep = match l.as_str() {
                    "Track" => "one",
                    "Playlist" => "all",
                    _ => "off",
                };
                let mode = sonos::play_mode(s.shuffle(), rep);
                self.act(async move { sonos.set_play_mode(&c.ip, mode).await });
            }
            Cmd::Raise | Cmd::Quit | Cmd::Action(_) => {}
        }
    }

    /// Really quits: hands the PC's sound back, cancels speaker callbacks, exits.
    pub fn quit(self: &Rc<Self>) {
        let (audio, events) = (self.core.audio.clone(), self.events.borrow().clone());
        crate::rt().block_on(async move {
            audio.shutdown().await;
            if let Some(e) = events {
                e.shutdown().await;
            }
        });
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    pub(super) fn show_settings(self: &Rc<Self>) {
        let page = adw::PreferencesPage::new();

        // Top bar: the island, once the desktop is known to support it.
        let top = adw::PreferencesGroup::builder()
            .title("Top bar")
            .description("A Dynamic Island in the middle of GNOME's top bar: click to play or pause, scroll for volume, rest the pointer on it for the full controls.")
            .build();
        let island_row = adw::ActionRow::builder().title("Top-bar island").subtitle("Checking your system…").build();
        let island_btn = gtk::Button::builder().label("Turn on").valign(gtk::Align::Center).css_classes(["pill"]).sensitive(false).build();
        let spinner = adw::Spinner::new();
        island_row.add_suffix(&spinner);
        island_row.add_suffix(&island_btn);
        top.add(&island_row);
        page.add(&top);

        let bg_group = adw::PreferencesGroup::builder().title("In the background").build();
        let bg = adw::SwitchRow::builder()
            .title("Keep running when closed")
            .subtitle("The top-bar island and the PC sound output keep working")
            .active(!self.core.cfg.lock().unwrap().quit_on_close)
            .build();
        let login = adw::SwitchRow::builder().title("Start when you log in").subtitle("Starts quietly in the background").active(autostart_file().exists()).build();
        bg_group.add(&bg);
        bg_group.add(&login);
        page.add(&bg_group);

        let now = adw::PreferencesGroup::builder().title("Now Playing").build();
        let lyrics = adw::SwitchRow::builder().title("Lyrics").subtitle("Synced lyrics from LRCLIB, when it has them").active(self.player.lyrics_btn.is_active()).build();
        now.add(&lyrics);
        page.add(&now);

        let net = adw::PreferencesGroup::builder()
            .title("Network")
            .description(format!(
                "Live updates need speakers to reach this PC on TCP port {}. Without it, Sonance checks the speakers every second instead.",
                sonos::events::EVENT_PORT
            ))
            .build();
        net.add(&widgets::row("Live updates", if self.live() { "On" } else { "Off: polling instead" }));
        page.add(&net);

        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        tv.set_content(Some(&page));
        let dialog = adw::Dialog::builder().title("Settings").content_width(520).content_height(620).child(&tv).build();
        super::glass::dialog(&dialog);

        let w = Rc::downgrade(self);
        bg.connect_active_notify(move |r| {
            if let Some(app) = w.upgrade() {
                let mut c = app.core.cfg.lock().unwrap();
                c.quit_on_close = !r.is_active();
                c.save();
            }
        });
        let w = Rc::downgrade(self);
        login.connect_active_notify(move |r| {
            if let Some(app) = w.upgrade() {
                if let Err(e) = set_autostart(r.is_active()) {
                    app.toast(&format!("Couldn't change start at login: {e}"));
                }
            }
        });
        let w = Rc::downgrade(self);
        lyrics.connect_active_notify(move |r| {
            if let Some(app) = w.upgrade() {
                // The player's button saves it and redraws.
                app.player.lyrics_btn.set_active(r.is_active());
            }
        });

        let ui = Rc::new(IslandUi { row: island_row, button: island_btn, spinner, state: RefCell::new(island::State::Off) });
        let u = ui.clone();
        let w = Rc::downgrade(self);
        ui.button.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let on = matches!(*u.state.borrow(), island::State::On { outdated: false } | island::State::NeedsLogin);
            u.busy(if on { "Turning off…" } else { "Turning on…" });
            let u = u.clone();
            let w = Rc::downgrade(&app);
            spawn(async move { if on { island::turn_off().await } else { island::turn_on().await } }, move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(state) => u.show(&island::Support::Supported { version: String::new() }, state),
                    Err(e) => {
                        app.toast(&format!("Couldn't change the top-bar island: {e:#}"));
                        u.recheck();
                    }
                }
            });
        });
        ui.recheck();
        dialog.present(Some(&self.window));
    }
}

/// The island row in Settings.
struct IslandUi {
    row: adw::ActionRow,
    button: gtk::Button,
    spinner: adw::Spinner,
    state: RefCell<island::State>,
}

impl IslandUi {
    fn busy(&self, text: &str) {
        self.row.set_subtitle(text);
        self.button.set_sensitive(false);
        self.spinner.set_visible(true);
    }

    fn recheck(self: &Rc<Self>) {
        self.busy("Checking your system…");
        let u = self.clone();
        spawn(island::check(), move |(support, state)| u.show(&support, state));
    }

    fn show(&self, support: &island::Support, state: island::State) {
        use island::{State, Support};
        self.spinner.set_visible(false);
        let blocked = match support {
            Support::NotGnome(desktop) => Some(format!("Not available: the island is made for GNOME, and this desktop is {desktop}.")),
            Support::Unsupported { version, supported } => Some(format!(
                "Not available on GNOME {version}. It supports GNOME {}.",
                supported.join(", ")
            )),
            _ => None,
        };
        if let Some(why) = blocked {
            self.row.set_subtitle(&why);
            self.button.set_label("Turn on");
            self.button.set_sensitive(false);
            *self.state.borrow_mut() = state;
            return;
        }
        let extensions_off = matches!(support, Support::ExtensionsOff { .. });
        let (text, label) = match &state {
            State::On { outdated: false } => ("On".to_string(), "Turn off"),
            State::On { outdated: true } => ("On, from an older version of Sonance".to_string(), "Update"),
            State::NeedsLogin => ("Log out and back in to finish: GNOME loads top-bar extensions at login.".to_string(), "Turn off"),
            State::Error(e) => (format!("Didn't start: {e}"), "Try again"),
            State::Off if extensions_off => (
                "Your system supports it, but GNOME's extensions are switched off. Turning the island on switches them back on (other extensions you have will run again too).".to_string(),
                "Turn on",
            ),
            State::Off => {
                let v = match support {
                    Support::Supported { version } if !version.is_empty() => format!(" (GNOME {version})"),
                    _ => String::new(),
                };
                (format!("Your system supports it{v}."), "Turn on")
            }
        };
        self.row.set_subtitle(&text);
        self.button.set_label(label);
        self.button.set_sensitive(true);
        self.button.remove_css_class("suggested-action");
        self.button.remove_css_class("destructive-action");
        if label != "Turn off" {
            self.button.add_css_class("suggested-action");
        }
        *self.state.borrow_mut() = state;
    }
}
