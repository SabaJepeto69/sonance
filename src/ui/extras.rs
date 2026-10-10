//! Lyrics, handing the PC's Spotify over to the speakers, and the island's
//! pop-up notices (alarms, the sleep timer, Spotify).

use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{spawn, App};
use crate::local_spotify::{self, Local};
use crate::lyrics::Lyrics;
use crate::sonos::{self, PlayState};

#[derive(Default)]
pub struct LyricsState {
    /// "title\nartist" of the song the lyrics are for (or being fetched for).
    key: String,
    lyrics: Option<Lyrics>,
    line: Option<usize>,
}

#[derive(Default)]
pub struct Extras {
    pub lyrics: std::cell::RefCell<LyricsState>,
    /// When `status.position` was last true, so lyrics can move between seconds.
    pub status_at: std::cell::Cell<Option<Instant>>,
    local: std::cell::RefCell<Option<Local>>,
    handoff_at: std::cell::Cell<Option<Instant>>,
    /// Notices already shown, so each fires once.
    shown: std::cell::RefCell<std::collections::HashSet<String>>,
    pub alarms: std::cell::RefCell<Vec<sonos::Alarm>>,
}

/// Local wall-clock time as (weekday 0 = Sunday, "HH:MM", "YYYY-MM-DD").
fn local_now() -> (u32, String, String) {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    (tm.tm_wday as u32, format!("{:02}:{:02}", tm.tm_hour, tm.tm_min), format!("{}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday))
}

/// Whether an alarm with this Sonos recurrence rings on `wday` (0 = Sunday).
pub fn rings_on(recurrence: &str, wday: u32) -> bool {
    match recurrence {
        "DAILY" | "ONCE" => true,
        "WEEKDAYS" => (1..=5).contains(&wday),
        "WEEKENDS" => wday == 0 || wday == 6,
        r => r.strip_prefix("ON_").is_some_and(|days| days.contains(char::from_digit(wday, 10).unwrap_or('x'))),
    }
}

impl App {
    pub(super) fn connect_extras(self: &Rc<Self>) {
        let hide = self.core.cfg.lock().unwrap().hide_lyrics;
        self.player.lyrics_btn.set_active(!hide);
        let w = Rc::downgrade(self);
        self.player.lyrics_btn.connect_toggled(move |b| {
            let Some(app) = w.upgrade() else { return };
            {
                let mut c = app.core.cfg.lock().unwrap();
                c.hide_lyrics = !b.is_active();
                c.save();
            }
            app.show_lyric_line(true);
        });
        let w = Rc::downgrade(self);
        self.player.handoff.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.handoff();
            }
        });
        // Lyrics need finer steps than the once-a-second clock.
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(app) = w.upgrade() else { return glib::ControlFlow::Break };
            app.show_lyric_line(false);
            glib::ControlFlow::Continue
        });
    }

    /// Called from the main tick, once a second.
    pub(super) fn extras_tick(self: &Rc<Self>, t: u64) {
        if t % 2 == 0 {
            self.watch_local_spotify();
        }
        if t % 20 == 0 {
            self.check_alarms();
        }
        if t % 600 == 0 {
            self.refresh_alarm_cache();
        }
    }

    pub(super) fn notice(&self, kind: &str, title: &str, body: &str, action: &str) {
        let Some(m) = self.mpris.borrow().clone() else { return };
        let (kind, title, body, action) = (kind.to_string(), title.to_string(), body.to_string(), action.to_string());
        crate::rt().spawn(async move {
            let _ = m.notice(&kind, &title, &body, &action).await;
        });
    }

    /// Notices that fire once per `key`.
    fn notice_once(&self, key: String, kind: &str, title: &str, body: &str, action: &str) {
        if self.extras.shown.borrow_mut().insert(key) {
            self.notice(kind, title, body, action);
        }
    }

    // ---- lyrics -------------------------------------------------------------

    /// Fetches lyrics when the song changed.
    pub(super) fn update_lyrics(self: &Rc<Self>) {
        let s = self.status.borrow().clone();
        let key = format!("{}\n{}", s.title, s.artist);
        if self.extras.lyrics.borrow().key == key {
            return;
        }
        *self.extras.lyrics.borrow_mut() = LyricsState { key: key.clone(), ..Default::default() };
        self.show_lyric_line(true);
        if s.title.is_empty() || s.artist.is_empty() || sonos::is_stream(&s.title) {
            return;
        }
        let http = self.core.sonos.soap.http().clone();
        let w = Rc::downgrade(self);
        spawn(async move { crate::lyrics::fetch(&http, &s.title, &s.artist, &s.album, s.duration).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            let mut st = app.extras.lyrics.borrow_mut();
            if st.key != key {
                return;
            }
            st.lyrics = r.ok().flatten();
            drop(st);
            app.show_lyric_line(true);
        });
    }

    fn song_position(&self) -> f32 {
        let s = self.status.borrow();
        let mut pos = s.position as f32;
        if s.state == PlayState::Playing {
            if let Some(at) = self.extras.status_at.get() {
                pos += at.elapsed().as_secs_f32().min(1.5);
            }
        }
        pos
    }

    /// Moves the lyrics on to the line being sung; `force` redraws regardless.
    fn show_lyric_line(&self, force: bool) {
        let pos = self.song_position();
        let mut st = self.extras.lyrics.borrow_mut();
        let line = st.lyrics.as_ref().and_then(|l| l.index_at(pos));
        if !force && line == st.line {
            return;
        }
        st.line = line;
        let p = &self.player;
        let show = st.lyrics.is_some() && p.lyrics_btn.is_active();
        p.lyrics_box.set_visible(show);
        p.lyrics_btn.set_sensitive(st.lyrics.is_some());
        p.lyrics_btn.set_tooltip_text(Some(if st.lyrics.is_some() { "Lyrics" } else { "No lyrics for this song" }));
        let current = match (&st.lyrics, line) {
            (Some(l), Some(i)) => l.lines[i].1.clone(),
            _ => String::new(),
        };
        if let Some(l) = &st.lyrics {
            for (k, label) in p.lyric_lines.iter().enumerate() {
                // Before the first line, show what's coming in the "next" slot.
                let idx = line.map_or(k as isize - 2, |i| i as isize + k as isize - 1);
                let text = usize::try_from(idx).ok().and_then(|i| l.lines.get(i)).map(|(_, t)| t.as_str()).unwrap_or("");
                label.set_label(if k == 1 && text.is_empty() && line.is_some() { "♪" } else { text });
            }
        }
        drop(st);
        if let Some(m) = self.mpris.borrow().clone() {
            crate::rt().spawn(async move {
                let _ = m.set_lyric(&current).await;
            });
        }
    }

    // ---- this PC's Spotify ----------------------------------------------------

    fn watch_local_spotify(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        spawn(local_spotify::now_playing(), move |now| {
            let Some(app) = w.upgrade() else { return };
            let was_playing = app.extras.local.borrow().as_ref().is_some_and(|l| l.playing);
            let playing = now.as_ref().is_some_and(|l| l.playing);
            app.player.handoff.set_visible(playing && app.coord().is_some());
            *app.extras.local.borrow_mut() = now;
            let speakers_idle = !matches!(app.status.borrow().state, PlayState::Playing | PlayState::Transitioning);
            let recent = app.extras.handoff_at.get().is_some_and(|t| t.elapsed() < Duration::from_secs(60));
            if playing && !was_playing && speakers_idle && !recent {
                if let Some(g) = app.group() {
                    app.notice("spotify", "Spotify is playing on this PC", &format!("Click to play it on {}", g.name()), "handoff");
                }
            }
        });
    }

    /// Moves what the Spotify app plays onto the selected speakers: the same
    /// song at the same point, inside its album or playlist when Sonance is
    /// signed in to Spotify, and pauses the app.
    pub(super) fn handoff(self: &Rc<Self>) {
        let Some(c) = self.coord() else { return };
        let acc = self.spotify_acc.borrow().clone();
        if acc.sn.is_empty() {
            self.toast("Sonance hasn't seen your Sonos Spotify account yet: save one Spotify favorite in the Sonos app");
            return;
        }
        self.extras.handoff_at.set(Some(Instant::now()));
        self.player.handoff.set_visible(false);
        let (sonos, spotify) = (self.core.sonos.clone(), self.core.spotify.clone());
        let room = self.group().map(|g| g.name()).unwrap_or_default();
        let w = Rc::downgrade(self);
        spawn(
            async move {
                let local = local_spotify::now_playing().await.ok_or_else(|| anyhow::anyhow!("Spotify isn't playing on this PC"))?;
                let started = Instant::now();
                let context = if spotify.logged_in() { spotify.playing_context().await.ok().flatten() } else { None };
                let index = match &context {
                    Some(ctx) => spotify.index_in(ctx, &local.uri).await.ok().flatten(),
                    None => None,
                };
                let _ = local_spotify::pause().await;
                let mut whole = false;
                if let (Some(ctx), Some(i)) = (&context, index) {
                    if let Some((uri, meta)) = sonos::spotify_target(&acc, ctx, &local.title) {
                        sonos.clear_queue(&c.ip).await?;
                        sonos.enqueue(&c.ip, &uri, &meta, 0, false).await?;
                        sonos.play_from_queue(&c, i + 1).await?;
                        whole = true;
                    }
                }
                if !whole {
                    let (uri, meta) = sonos::spotify_target(&acc, &local.uri, &local.title).ok_or_else(|| anyhow::anyhow!("not a Spotify track"))?;
                    sonos.play_now_keep_queue(&c, &uri, &meta).await?;
                }
                // Pick up where the app was: wait for the speakers to start, then seek,
                // counting the time all this took.
                for _ in 0..20 {
                    if sonos.status(&c.ip).await.is_ok_and(|s| s.state == PlayState::Playing) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                let at = (local.position_ms + started.elapsed().as_millis() as u64) / 1000;
                if at > 2 {
                    let _ = sonos.seek(&c.ip, at as u32).await;
                }
                anyhow::Ok((local.title, whole))
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok((title, whole)) => {
                        app.toast(&format!("“{title}” moved to {room}{}", if whole { ", with its playlist" } else { "" }));
                        app.poll_status();
                        app.refresh_cover();
                    }
                    Err(e) => app.toast(&format!("Couldn't move Spotify: {e:#}")),
                }
            },
        );
    }

    // ---- alarms and the sleep timer -------------------------------------------

    pub(super) fn refresh_alarm_cache(self: &Rc<Self>) {
        let Some(ip) = self.any_ip() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(async move { sonos.alarms(&ip).await }, move |r| {
            if let (Some(app), Ok(a)) = (w.upgrade(), r) {
                *app.extras.alarms.borrow_mut() = a;
            }
        });
    }

    fn check_alarms(&self) {
        let (wday, hhmm, date) = local_now();
        let alarms = self.extras.alarms.borrow().clone();
        for a in alarms.iter().filter(|a| a.enabled && a.start.starts_with(&hhmm) && rings_on(&a.recurrence, wday)) {
            let room = self
                .groups
                .borrow()
                .iter()
                .flat_map(|g| g.members.iter())
                .find(|m| m.uuid == a.room_uuid)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "a room".into());
            self.notice_once(format!("alarm {} {date}", a.id), "alarm", &format!("Alarm · {hhmm}"), &format!("Waking up {room}"), "raise");
        }
    }

    /// Warns a minute before the sleep timer stops the music.
    pub(super) fn sleep_notice(&self, secs: Option<u32>) {
        match secs {
            Some(s) if s <= 75 => {
                if let Some(g) = self.group() {
                    self.notice_once(format!("sleep {}", g.coordinator.uuid), "sleep", "Sleep timer", &format!("{} stops in a minute · click for 15 more", g.name()), "sleep-extend");
                }
            }
            // Re-arm once the timer is gone or set again.
            _ => self.extras.shown.borrow_mut().retain(|k| !k.starts_with("sleep ")),
        }
    }

    /// The sleep notice's click: fifteen more minutes.
    pub(super) fn extend_sleep(self: &Rc<Self>) {
        let Some(c) = self.coord() else { return };
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(async move { sonos.set_sleep_timer(&c.ip, Some(15 * 60)).await }, move |r| {
            if let Some(app) = w.upgrade() {
                if r.is_ok() {
                    app.notice("sleep", "Sleep timer", "15 more minutes", "");
                    app.refresh_sleep_timer();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::rings_on;

    #[test]
    fn recurrence() {
        assert!(rings_on("DAILY", 3));
        assert!(rings_on("WEEKDAYS", 1) && !rings_on("WEEKDAYS", 0));
        assert!(rings_on("WEEKENDS", 6) && !rings_on("WEEKENDS", 2));
        assert!(rings_on("ON_135", 3) && !rings_on("ON_135", 2));
    }
}
