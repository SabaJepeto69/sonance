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
    clock: std::cell::RefCell<Option<Clock>>,
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

/// The song's position, kept to a fraction of a second. Sonos reports whole
/// seconds, so each reading only says the true position is somewhere in
/// [p, p+1); the clock runs on its own and is nudged back inside that window
/// whenever a reading disagrees, which pins it down within a few readings.
#[derive(Clone)]
struct Clock {
    pos: f64,
    at: Instant,
    playing: bool,
    /// Which song: a new one restarts the clock.
    key: String,
}

impl Clock {
    fn now(&self) -> f64 {
        if self.playing { self.pos + self.at.elapsed().as_secs_f64() } else { self.pos }
    }
}

/// Where the clock should be after reading `p` whole seconds.
fn settle(predicted: Option<f64>, p: f64, playing: bool) -> f64 {
    match predicted {
        // Close: keep the smooth estimate, pulled just inside [p, p+1).
        Some(pr) if pr >= p - 1.5 && pr <= p + 2.5 => pr.clamp(p, p + 0.98),
        // A seek, a new song or a first reading: the middle of the window.
        _ => p + if playing { 0.5 } else { 0.0 },
    }
}

impl App {
    pub(super) fn connect_extras(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.player.handoff.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.handoff();
            }
        });
        // Lyrics need finer steps than the once-a-second clock.
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(100), move || {
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

    /// While lyrics are on screen, read the position often enough to keep them in step.
    pub(super) fn wants_fast_poll(&self) -> bool {
        self.extras.lyrics.borrow().lyrics.is_some() && self.status.borrow().state == PlayState::Playing
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

    /// Feeds a fresh reading from the speaker into the song clock.
    pub(super) fn sync_clock(&self) {
        let s = self.status.borrow();
        let key = format!("{}\n{}\n{}", s.track_no, s.title, s.artist);
        let playing = s.state == PlayState::Playing;
        let mut c = self.extras.clock.borrow_mut();
        let predicted = c.as_ref().filter(|c| c.key == key).map(Clock::now);
        let pos = settle(predicted, s.position as f64, playing);
        *c = Some(Clock { pos, at: Instant::now(), playing, key });
    }

    fn song_position(&self) -> f32 {
        self.extras.clock.borrow().as_ref().map_or(0.0, |c| c.now() as f32)
    }

    /// Fetches lyrics when the song changed.
    pub(super) fn update_lyrics(self: &Rc<Self>) {
        let s = self.status.borrow().clone();
        let key = format!("{}\n{}", s.title, s.artist);
        if self.extras.lyrics.borrow().key == key {
            return;
        }
        *self.extras.lyrics.borrow_mut() = LyricsState { key: key.clone(), ..Default::default() };
        self.build_lyrics();
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
            app.build_lyrics();
        });
    }

    /// One label per line of the song's lyrics (or none).
    fn build_lyrics(&self) {
        let p = &self.player;
        while let Some(c) = p.lyrics_col.first_child() {
            p.lyrics_col.remove(&c);
        }
        let mut labels = Vec::new();
        if let Some(l) = &self.extras.lyrics.borrow().lyrics {
            for (_, text) in &l.lines {
                let label = gtk::Label::builder()
                    .label(if text.is_empty() { "♪" } else { text.as_str() })
                    .wrap(true)
                    // Right-to-left lyrics (Hebrew, Arabic) hang from the right.
                    .xalign(if gtk::pango::find_base_dir(text) == gtk::pango::Direction::Rtl { 1.0 } else { 0.0 })
                    .justify(if gtk::pango::find_base_dir(text) == gtk::pango::Direction::Rtl { gtk::Justification::Right } else { gtk::Justification::Left })
                    .css_classes(["lyric"])
                    .build();
                p.lyrics_col.append(&label);
                labels.push(label);
            }
        }
        p.lyrics_box.set_visible(!labels.is_empty());
        *p.lyric_lines.borrow_mut() = labels;
        if let Some(a) = p.lyrics_anim.borrow_mut().take() {
            a.skip();
        }
        p.lyrics_scroll.vadjustment().set_value(0.0);
        self.extras.lyrics.borrow_mut().line = None;
        self.show_lyric_line(true);
    }

    /// Moves the lyrics on to the line being sung; `force` redraws regardless.
    fn show_lyric_line(&self, force: bool) {
        let pos = self.song_position();
        let mut st = self.extras.lyrics.borrow_mut();
        let line = st.lyrics.as_ref().and_then(|l| l.index_at(pos));
        if !force && line == st.line {
            return;
        }
        let jumped = match (st.line, line) {
            (Some(a), Some(b)) => b < a || b > a + 1,
            _ => false,
        };
        st.line = line;
        let current = match (&st.lyrics, line) {
            (Some(l), Some(i)) => l.lines[i].1.clone(),
            _ => String::new(),
        };
        drop(st);

        let p = &self.player;
        let labels = p.lyric_lines.borrow();
        for (i, label) in labels.iter().enumerate() {
            let (past, now) = match line {
                Some(l) => (i < l, i == l),
                None => (false, false),
            };
            if past { label.add_css_class("past") } else { label.remove_css_class("past") }
            if now { label.add_css_class("current") } else { label.remove_css_class("current") }
        }
        // Glide the current line to a third of the way down; a seek jumps.
        let target = line.and_then(|i| labels.get(i)).and_then(|label| {
            let y = label.compute_point(&p.lyrics_col, &gtk::graphene::Point::zero())?.y() as f64;
            Some(y + label.height() as f64 / 2.0 - p.lyrics_scroll.height() as f64 * 0.38)
        });
        let adj = p.lyrics_scroll.vadjustment();
        let to = target.unwrap_or(0.0).clamp(0.0, (adj.upper() - adj.page_size()).max(0.0));
        if let Some(a) = p.lyrics_anim.borrow_mut().take() {
            a.pause();
        }
        if jumped || force || !self.window.is_visible() {
            adj.set_value(to);
        } else {
            let a2 = adj.clone();
            let anim = adw::TimedAnimation::builder()
                .widget(&p.lyrics_scroll)
                .value_from(adj.value())
                .value_to(to)
                .duration(650)
                .easing(adw::Easing::EaseOutCubic)
                .target(&adw::CallbackAnimationTarget::new(move |v| a2.set_value(v)))
                .build();
            anim.play();
            *p.lyrics_anim.borrow_mut() = Some(anim);
        }
        drop(labels);
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
    use super::{rings_on, settle};

    #[test]
    fn clock_settles_into_the_reading() {
        // First reading: middle of the second.
        assert_eq!(settle(None, 10.0, true), 10.5);
        // Running a little ahead or behind: pulled just inside [p, p+1).
        assert_eq!(settle(Some(12.3), 11.0, true), 11.98);
        assert_eq!(settle(Some(10.7), 11.0, true), 11.0);
        assert_eq!(settle(Some(11.4), 11.0, true), 11.4);
        // A seek.
        assert_eq!(settle(Some(40.0), 11.0, true), 11.5);
    }

    #[test]
    fn recurrence() {
        assert!(rings_on("DAILY", 3));
        assert!(rings_on("WEEKDAYS", 1) && !rings_on("WEEKDAYS", 0));
        assert!(rings_on("WEEKENDS", 6) && !rings_on("WEEKENDS", 2));
        assert!(rings_on("ON_135", 3) && !rings_on("ON_135", 2));
    }
}
