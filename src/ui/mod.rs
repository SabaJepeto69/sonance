mod alarms;
mod calibrate;
mod browse;
mod coverflow;
mod glass;
mod dialogs;
mod player;
mod output;
mod queue;
mod room3d;
mod roomtab;
mod widgets;

use adw::prelude::*;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::sonos::{self, Group, Item, Member, SpotifyAccount, Status};
use crate::Core;

pub type Job = Rc<dyn Fn(f64) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>>;

/// Runs `fut` on the tokio runtime and hands the result back on the GTK thread.
pub fn spawn<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static, then: impl FnOnce(T) + 'static) {
    glib::spawn_future_local(async move {
        match crate::rt().spawn(fut).await {
            Ok(v) => then(v),
            Err(e) => eprintln!("background task failed: {e}"),
        }
    });
}

#[derive(Default)]
struct ThrottleSlot {
    busy: bool,
    pending: Option<f64>,
}

pub struct App {
    pub core: Core,
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    rooms: gtk::ListBox,
    room_label: gtk::Label,
    room_pop: gtk::Popover,
    pub stack: adw::ViewStack,
    pub player: player::Player,
    pub queue: queue::QueueView,
    pub browse: browse::BrowseView,
    pub alarms: alarms::AlarmsView,
    pub room: roomtab::RoomTab,
    pub output: output::OutputSection,

    pub groups: RefCell<Vec<Group>>,
    summaries: RefCell<HashMap<String, String>>,
    pub selected: RefCell<Option<String>>,
    pub status: RefCell<Status>,
    pub favorites: RefCell<Vec<Item>>,
    pub spotify_acc: RefCell<SpotifyAccount>,

    art_cache: RefCell<HashMap<String, gdk::Texture>>,
    art_waiters: RefCell<HashMap<String, Vec<gtk::Widget>>>,
    throttles: RefCell<HashMap<String, ThrottleSlot>>,
    user_touched: RefCell<HashMap<String, Instant>>,
    polling: Cell<bool>,
    repoll: Cell<bool>,
    tick: Cell<u64>,
}

pub fn run(core: Core) {
    let app = adw::Application::builder().application_id("dev.sonance.Sonance").build();
    let core2 = core.clone();
    app.connect_activate(move |gapp| {
        if let Some(w) = gapp.active_window() {
            w.present();
            return;
        }
        load_css();
        let ui = App::build(gapp, core2.clone());
        ui.window.present();
        if let Ok(connector) = std::env::var("SONANCE_MONITOR") {
            open_on_monitor(&ui.window, &connector);
        }
        ui.start();
        // Every callback holds a Weak<App>; this is the one strong reference.
        let keep = RefCell::new(Some(ui.clone()));
        ui.window.connect_close_request(move |_| {
            // Put the PC's sound back on its normal output and stop every helper process.
            if let Some(app) = keep.borrow_mut().take() {
                crate::rt().block_on(app.core.audio.shutdown());
            }
            glib::Propagation::Proceed
        });
    });
    app.run_with_args(&Vec::<String>::new());
}

/// Wayland gives apps no say in placement, but a window fullscreened on a
/// monitor stays on that monitor once it leaves fullscreen.
fn open_on_monitor(window: &adw::ApplicationWindow, connector: &str) {
    let Some(display) = gdk::Display::default() else { return };
    let monitors = display.monitors();
    let monitor = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .find(|m| m.connector().as_deref() == Some(connector));
    let Some(monitor) = monitor else {
        eprintln!("SONANCE_MONITOR: no monitor called {connector}");
        return;
    };
    window.fullscreen_on_monitor(&monitor);
    let w = window.clone();
    glib::timeout_add_local_once(Duration::from_millis(400), move || w.unfullscreen());
}

fn load_css() {
    // OLED black: the glass look only works dark, whatever the desktop prefers.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let css = gtk::CssProvider::new();
    css.connect_parsing_error(|_, section, err| eprintln!("style.css {}: {err}", section.to_str()));
    css.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        // One above USER: ~/.config/gtk-4.0/gtk.css here imports a whole desktop
        // theme at USER priority, which would otherwise repaint the glass.
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    }
}

impl App {
    fn build(gapp: &adw::Application, core: Core) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder().application(gapp).title("Sonance").default_width(1180).default_height(760).build();
        window.set_size_request(380, 400);

        // Rooms live in a popover off the header's room button; no sidebar.
        let output = output::OutputSection::new();
        let rooms = gtk::ListBox::new();
        rooms.add_css_class("navigation-sidebar");
        rooms.set_selection_mode(gtk::SelectionMode::Single);
        let rooms_scroll = gtk::ScrolledWindow::builder().child(&rooms).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(420).build();
        let group_btn = gtk::Button::builder().label("Group rooms…").css_classes(["flat"]).hexpand(true).build();
        let refresh_btn = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Find speakers again").css_classes(["flat"]).build();
        let room_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        room_actions.append(&group_btn);
        room_actions.append(&refresh_btn);
        let room_col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        room_col.set_width_request(280);
        room_col.append(&rooms_scroll);
        room_col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        room_col.append(&room_actions);
        room_col.append(&output.root);
        let room_pop = gtk::Popover::new();
        room_pop.set_child(Some(&room_col));
        glass::popover(&room_pop);
        let room_label = gtk::Label::builder().label("Rooms").ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(18).build();
        let room_btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        room_btn_box.append(&gtk::Image::from_icon_name("audio-speakers-symbolic"));
        room_btn_box.append(&room_label);
        room_btn_box.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let room_picker = gtk::MenuButton::builder().child(&room_btn_box).popover(&room_pop).tooltip_text("Rooms").css_classes(["flat", "room-picker"]).build();

        // Content: tabs plus the player bar.
        let stack = adw::ViewStack::new();
        let player = player::Player::new();
        let queue = queue::QueueView::new();
        let browse = browse::BrowseView::new();
        let alarms = alarms::AlarmsView::new();
        let room = roomtab::RoomTab::new();
        stack.add_titled_with_icon(&player.now_page, Some("now"), "Now Playing", "audio-x-generic-symbolic");
        stack.add_titled_with_icon(&queue.root, Some("queue"), "Queue", "view-list-symbolic");
        stack.add_titled_with_icon(&browse.root, Some("browse"), "Browse", "system-search-symbolic");
        stack.add_titled_with_icon(&room.root, Some("room"), "Room", "find-location-symbolic");
        stack.add_titled_with_icon(&alarms.root, Some("alarms"), "Alarms", "alarm-symbolic");

        let header = adw::HeaderBar::new();
        header.pack_start(&room_picker);
        header.set_title_widget(Some(&adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build()));
        let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").tooltip_text("Menu").build();
        header.pack_end(&menu_btn);
        let room_btn = gtk::Button::builder().icon_name("emblem-system-symbolic").tooltip_text("Room settings").build();
        header.pack_end(&room_btn);

        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&header);
        // The player floats as a glass capsule over the pages.
        let stage = gtk::Overlay::new();
        stage.set_child(Some(&stack));
        stage.add_overlay(&player.bar);
        content_view.set_content(Some(&stage));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content_view));
        window.set_content(Some(&toasts));

        let app = Rc::new(App {
            core,
            window,
            toasts,
            rooms,
            room_label,
            room_pop,
            stack,
            player,
            queue,
            browse,
            alarms,
            room,
            output,
            groups: RefCell::default(),
            summaries: RefCell::default(),
            selected: RefCell::default(),
            status: RefCell::default(),
            favorites: RefCell::default(),
            spotify_acc: RefCell::default(),
            art_cache: RefCell::default(),
            art_waiters: RefCell::default(),
            throttles: RefCell::default(),
            user_touched: RefCell::default(),
            polling: Cell::new(false),
            repoll: Cell::new(false),
            tick: Cell::new(0),
        });

        let w = Rc::downgrade(&app);
        app.rooms.connect_row_selected(move |_, row| {
            let (Some(app), Some(row)) = (w.upgrade(), row) else { return };
            let uuid = row.widget_name().to_string();
            if app.selected.borrow().as_deref() != Some(uuid.as_str()) {
                app.select(uuid);
            }
        });
        let w = Rc::downgrade(&app);
        app.rooms.connect_row_activated(move |_, _| {
            if let Some(app) = w.upgrade() {
                app.room_pop.popdown();
            }
        });
        let w = Rc::downgrade(&app);
        refresh_btn.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.discover();
            }
        });
        let w = Rc::downgrade(&app);
        group_btn.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.room_pop.popdown();
                app.show_group_dialog();
            }
        });
        let w = Rc::downgrade(&app);
        room_btn.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.show_room_settings();
            }
        });
        menu_btn.set_popover(Some(&app.main_menu()));
        let w = Rc::downgrade(&app);
        app.stack.connect_visible_child_name_notify(move |_| {
            if let Some(app) = w.upgrade() {
                app.on_tab_changed();
            }
        });

        app.connect_player();
        app.connect_queue();
        app.connect_browse();
        app.connect_alarms();
        app.connect_room();
        app.connect_output();
        app
    }

    fn main_menu(self: &Rc<Self>) -> gtk::Popover {
        let items: [(&str, fn(&Rc<App>)); 4] = [
            ("Group rooms…", |a| a.show_group_dialog()),
            ("Room settings…", |a| a.show_room_settings()),
            ("Spotify account…", |a| a.show_spotify_dialog()),
            ("About Sonance", |a| a.show_about()),
        ];
        let pop = gtk::Popover::new();
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for (label, f) in items {
            let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
            b.child().and_downcast::<gtk::Label>().map(|l| l.set_xalign(0.0));
            let w = Rc::downgrade(self);
            let p = pop.downgrade();
            b.connect_clicked(move |_| {
                if let Some(p) = p.upgrade() {
                    p.popdown();
                }
                if let Some(a) = w.upgrade() {
                    f(&a);
                }
            });
            bx.append(&b);
        }
        pop.set_child(Some(&bx));
        glass::popover(&pop);
        pop
    }

    // ---- helpers ------------------------------------------------------------

    pub fn toast(&self, msg: &str) {
        let t = adw::Toast::new(msg);
        t.set_timeout(4);
        self.toasts.add_toast(t);
    }

    pub fn group(&self) -> Option<Group> {
        let sel = self.selected.borrow().clone()?;
        self.groups.borrow().iter().find(|g| g.coordinator.uuid == sel).cloned()
    }

    pub fn coord(&self) -> Option<Member> {
        self.group().map(|g| g.coordinator)
    }

    pub fn all_members(&self) -> Vec<Member> {
        let mut v: Vec<Member> = self.groups.borrow().iter().flat_map(|g| g.members.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Any speaker IP, for household-wide calls (favorites, alarms).
    pub fn any_ip(&self) -> Option<String> {
        self.groups.borrow().first().map(|g| g.coordinator.ip.clone())
    }

    /// Runs a speaker command; errors become toasts and the UI refreshes after.
    pub fn act(self: &Rc<Self>, fut: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let w = Rc::downgrade(self);
        spawn(fut, move |r| {
            let Some(app) = w.upgrade() else { return };
            if let Err(e) = r {
                app.toast(&format!("{e:#}"));
            }
            app.poll_status();
        });
    }

    /// Like `act`, but coalesces rapid calls (slider drags) so at most one is in flight.
    pub fn throttled(self: &Rc<Self>, key: &str, v: f64, job: Job) {
        self.user_touched.borrow_mut().insert(key.to_string(), Instant::now());
        {
            let mut m = self.throttles.borrow_mut();
            let slot = m.entry(key.to_string()).or_default();
            if slot.busy {
                slot.pending = Some(v);
                return;
            }
            slot.busy = true;
        }
        let w = Rc::downgrade(self);
        let key = key.to_string();
        let fut = job(v);
        spawn(fut, move |r| {
            let Some(app) = w.upgrade() else { return };
            if let Err(e) = r {
                app.toast(&format!("{e:#}"));
            }
            let next = {
                let mut m = app.throttles.borrow_mut();
                let slot = m.entry(key.clone()).or_default();
                slot.busy = false;
                slot.pending.take()
            };
            if let Some(n) = next {
                app.throttled(&key, n, job);
            }
        });
    }

    /// True while the user is dragging a control, so polling doesn't fight them.
    pub fn recently_touched(&self, key: &str) -> bool {
        self.user_touched.borrow().get(key).is_some_and(|t| t.elapsed() < Duration::from_millis(2500))
    }

    /// Loads (cached) album art into an Image or Picture.
    pub fn load_art(self: &Rc<Self>, url: Option<String>, target: &impl IsA<gtk::Widget>) {
        let target: gtk::Widget = target.clone().upcast();
        let Some(url) = url else {
            target.set_widget_name("");
            set_paintable(&target, None);
            return;
        };
        // The name marks which image the widget wants now, so late answers can be dropped.
        target.set_widget_name(&url);
        if let Some(tex) = self.art_cache.borrow().get(&url) {
            set_paintable(&target, Some(tex));
            return;
        }
        set_paintable(&target, None);
        {
            let mut waiters = self.art_waiters.borrow_mut();
            if let Some(v) = waiters.get_mut(&url) {
                v.push(target);
                return;
            }
            waiters.insert(url.clone(), vec![target]);
        }
        let http = self.core.sonos.soap.http().clone();
        let w = Rc::downgrade(self);
        let u = url.clone();
        let fut = async move {
            // Speakers serve art slowly and choke on dozens of parallel requests.
            static LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);
            let _permit = LIMIT.acquire().await.ok()?;
            let r = http.get(&u).timeout(Duration::from_secs(10)).send().await.ok()?;
            if !r.status().is_success() {
                return None;
            }
            r.bytes().await.ok().map(|b| b.to_vec())
        };
        spawn(fut, move |bytes| {
            let Some(app) = w.upgrade() else { return };
            let waiters = app.art_waiters.borrow_mut().remove(&url).unwrap_or_default();
            let Some(tex) = bytes.and_then(|b| gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).ok()) else { return };
            for t in waiters.iter().filter(|t| t.widget_name() == url) {
                set_paintable(t, Some(&tex));
            }
            let mut cache = app.art_cache.borrow_mut();
            if cache.len() > 800 {
                cache.clear();
            }
            cache.insert(url, tex);
        });
    }

    // ---- lifecycle ----------------------------------------------------------

    fn start(self: &Rc<Self>) {
        self.discover();
        let w: Weak<App> = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(app) = w.upgrade() else { return glib::ControlFlow::Break };
            let t = app.tick.get() + 1;
            app.tick.set(t);
            app.poll_status();
            if t % 5 == 0 {
                app.refresh_topology();
                app.refresh_sleep_timer();
                if app.stack.visible_child_name().as_deref() == Some("now") {
                    app.refresh_cover();
                }
            }
            if t % 2 == 0 && app.stack.visible_child_name().as_deref() == Some("queue") {
                app.refresh_queue(false);
            }
            glib::ControlFlow::Continue
        });
    }

    fn discover(self: &Rc<Self>) {
        self.player.set_message("Looking for speakers…");
        let core = self.core.clone();
        let known = core.cfg.lock().unwrap().known_ips.clone();
        let w = Rc::downgrade(self);
        spawn(
            async move {
                let ip = sonos::discovery::find_speaker(&core.sonos.soap, &known).await?;
                core.sonos.groups(&ip).await
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(groups) => {
                        app.set_groups(groups);
                        app.load_household();
                        // The visible tab may have tried to load before any speaker was known.
                        app.on_tab_changed();
                    }
                    Err(e) => {
                        app.player.set_message(&format!("{e:#}"));
                        app.toast(&format!("{e:#}"));
                    }
                }
            },
        );
    }

    fn refresh_topology(self: &Rc<Self>) {
        let Some(ip) = self.any_ip() else { return };
        let ips: Vec<String> = self.all_members().into_iter().map(|m| m.ip).filter(|i| *i != ip).collect();
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(
            async move {
                // If the speaker we asked went away, any other one will do.
                let mut r = sonos.groups(&ip).await;
                for other in ips {
                    if r.is_ok() {
                        break;
                    }
                    r = sonos.groups(&other).await;
                }
                let groups = r?;
                let mut summaries = HashMap::new();
                for g in &groups {
                    if let Ok(s) = sonos.status(&g.coordinator.ip).await {
                        let text = match (s.state, s.title.is_empty()) {
                            (_, true) => String::new(),
                            (sonos::PlayState::Playing, _) => format!("▶ {}", join_artist(&s.title, &s.artist)),
                            _ => join_artist(&s.title, &s.artist),
                        };
                        summaries.insert(g.coordinator.uuid.clone(), text);
                    }
                }
                anyhow::Ok((groups, summaries))
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                if let Ok((groups, summaries)) = r {
                    *app.summaries.borrow_mut() = summaries;
                    app.set_groups(groups);
                }
            },
        );
    }

    fn set_groups(self: &Rc<Self>, groups: Vec<Group>) {
        {
            let mut c = self.core.cfg.lock().unwrap();
            let ips: Vec<String> = groups.iter().flat_map(|g| g.members.iter().map(|m| m.ip.clone())).collect();
            if c.known_ips != ips {
                c.known_ips = ips;
                c.save();
            }
        }
        let changed = *self.groups.borrow() != groups;
        *self.groups.borrow_mut() = groups;

        // Keep the selection on the same room even if its group changed shape.
        let sel = self.selected.borrow().clone();
        let still_there = sel.as_ref().is_some_and(|s| self.groups.borrow().iter().any(|g| &g.coordinator.uuid == s));
        if !still_there {
            let last = self.core.cfg.lock().unwrap().last_room.clone();
            let groups = self.groups.borrow();
            // The selected room may have joined another group: follow it there.
            let pick = sel
                .as_ref()
                .and_then(|s| groups.iter().find(|g| g.members.iter().any(|m| &m.uuid == s)))
                .or_else(|| last.as_ref().and_then(|l| groups.iter().find(|g| g.members.iter().any(|m| &m.uuid == l))))
                .or(groups.first())
                .map(|g| g.coordinator.uuid.clone());
            drop(groups);
            *self.selected.borrow_mut() = pick;
        }
        if changed || !still_there {
            self.rebuild_rooms();
            self.on_group_changed();
        } else {
            self.update_room_subtitles();
        }
    }

    fn rebuild_rooms(self: &Rc<Self>) {
        self.rooms.remove_all();
        let sel = self.selected.borrow().clone();
        for g in self.groups.borrow().iter() {
            let row = widgets::row(&g.name(), "");
            row.set_widget_name(&g.coordinator.uuid);
            let icon = gtk::Image::from_icon_name("audio-speakers-symbolic");
            row.add_prefix(&icon);
            if g.members.len() > 1 {
                row.set_tooltip_text(Some(&g.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")));
            }
            self.rooms.append(&row);
            if sel.as_deref() == Some(g.coordinator.uuid.as_str()) {
                self.rooms.select_row(Some(&row));
            }
        }
        self.update_room_subtitles();
    }

    fn update_room_subtitles(&self) {
        let summaries = self.summaries.borrow();
        let mut child = self.rooms.first_child();
        while let Some(w) = child {
            if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
                let text = summaries.get(row.widget_name().as_str()).cloned().unwrap_or_default();
                row.set_subtitle(&text);
                if text.starts_with('▶') {
                    row.add_css_class("room-playing");
                } else {
                    row.remove_css_class("room-playing");
                }
            }
            child = w.next_sibling();
        }
    }

    fn select(self: &Rc<Self>, uuid: String) {
        *self.selected.borrow_mut() = Some(uuid.clone());
        let mut c = self.core.cfg.lock().unwrap();
        c.last_room = Some(uuid);
        c.save();
        drop(c);
        self.on_group_changed();
    }

    fn on_group_changed(self: &Rc<Self>) {
        let title = self.group().map(|g| g.name()).unwrap_or_else(|| "Sonance".into());
        self.room_label.set_label(&title);
        *self.status.borrow_mut() = Status::default();
        self.poll_status();
        // The queue belongs to the old group; reload it now only if it's on screen.
        self.queue.invalidate();
        if self.stack.visible_child_name().as_deref() == Some("queue") {
            self.refresh_queue(true);
        }
        // Which speakers count as "playing" in the room view follows the selected group.
        self.recompute_plan();
        self.refresh_sleep_timer();
    }

    fn on_tab_changed(self: &Rc<Self>) {
        match self.stack.visible_child_name().as_deref() {
            Some("queue") => self.refresh_queue(true),
            Some("alarms") => self.refresh_alarms(),
            Some("browse") => self.browse_shown(),
            Some("room") => self.room_shown(),
            _ => {}
        }
    }

    /// Favorites, alarms and the Spotify link are household-wide.
    fn load_household(self: &Rc<Self>) {
        let Some(ip) = self.any_ip() else { return };
        let coord_ip = self.coord().map(|c| c.ip).unwrap_or_else(|| ip.clone());
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        spawn(
            async move {
                let favs = sonos.favorites(&ip).await.unwrap_or_default();
                let alarms = sonos.alarms(&ip).await.unwrap_or_default();
                let queue = sonos.queue(&coord_ip).await.unwrap_or_default();
                let acc = sonos::Sonos::spotify_account(&favs, &alarms, &queue);
                (favs, acc)
            },
            move |(favs, acc)| {
                let Some(app) = w.upgrade() else { return };
                *app.favorites.borrow_mut() = favs;
                if let Some(acc) = acc {
                    *app.spotify_acc.borrow_mut() = acc;
                }
                app.render_favorites();
            },
        );
    }

    fn poll_status(self: &Rc<Self>) {
        if self.polling.get() {
            // The answer in flight may predate a command just sent; ask again after it.
            self.repoll.set(true);
            return;
        }
        let Some(coord) = self.coord() else { return };
        self.polling.set(true);
        let sonos = self.core.sonos.clone();
        let w = Rc::downgrade(self);
        let uuid = coord.uuid.clone();
        spawn(async move { sonos.status(&coord.ip).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            app.polling.set(false);
            if app.repoll.take() {
                app.poll_status();
                return;
            }
            // Ignore answers for a room that is no longer selected.
            if app.selected.borrow().as_deref() != Some(uuid.as_str()) {
                return;
            }
            match r {
                Ok(s) => {
                    let old = app.status.borrow().clone();
                    let track_changed = s.track_no != old.track_no || s.title != old.title;
                    let cover_stale = track_changed || s.from_queue != old.from_queue || s.shuffle() != old.shuffle() || s.art != old.art;
                    *app.status.borrow_mut() = s;
                    app.player.update(&app);
                    if track_changed {
                        app.queue.highlight(app.status.borrow().track_no);
                    }
                    if cover_stale {
                        app.refresh_cover();
                    }
                }
                Err(e) => app.player.set_message(&format!("Can't reach the speaker: {e:#}")),
            }
        });
    }
}

pub fn join_artist(title: &str, artist: &str) -> String {
    if artist.is_empty() { title.to_string() } else { format!("{title} — {artist}") }
}

fn set_paintable(w: &gtk::Widget, tex: Option<&gdk::Texture>) {
    if let Some(img) = w.downcast_ref::<gtk::Image>() {
        match tex {
            Some(t) => img.set_paintable(Some(t)),
            None => img.set_icon_name(Some("folder-music-symbolic")),
        }
    } else if let Some(pic) = w.downcast_ref::<gtk::Picture>() {
        pic.set_paintable(tex);
    }
}
