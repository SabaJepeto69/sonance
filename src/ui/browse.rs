use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::widgets::{self, PlayHow};
use super::{spawn, App};
use crate::sonos::{self, Item, Member, Sonos};
use crate::spotify::{self, SpItem};

pub struct BrowseView {
    pub root: gtk::Box,
    fav_list: gtk::ListBox,
    fav_empty: adw::StatusPage,
    sp_stack: gtk::Stack,
    client_id: adw::EntryRow,
    connect: gtk::Button,
    search: gtk::SearchEntry,
    back: gtk::Button,
    results: gtk::Box,
    spinner: adw::Spinner,
    pub(super) library: RefCell<Option<(Vec<SpItem>, Vec<SpItem>)>>,
    generation: Cell<u64>,
}

impl BrowseView {
    pub fn new() -> Self {
        let inner = adw::ViewStack::new();

        // Favorites.
        let fav_list = widgets::boxed_list();
        let fav_empty = adw::StatusPage::builder().icon_name("starred-symbolic").title("No Sonos Favorites").description("Favorites you save in the Sonos app show up here.").build();
        let fav_col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        fav_col.append(&fav_list);
        fav_col.append(&fav_empty);
        inner.add_titled_with_icon(&widgets::page(&fav_col), Some("favorites"), "Sonos Favorites", "starred-symbolic");

        // Spotify: a login page and the search/library page.
        let client_id = adw::EntryRow::builder().title("Spotify Client ID").build();
        let id_list = widgets::boxed_list();
        id_list.append(&client_id);
        let steps = gtk::Label::builder()
            .label(format!(
                "1. Open the Spotify developer dashboard and create an app (tick “Web API”).\n2. Add this Redirect URI to it:  {}\n3. Paste the app’s Client ID above and press Connect.\n\nPlayback goes through the Spotify account already linked in your Sonos system; this login is only used for search and your library.",
                spotify::redirect_uri()
            ))
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["dim-label"])
            .build();
        let dash = gtk::LinkButton::with_label("https://developer.spotify.com/dashboard", "Open the Spotify developer dashboard");
        let connect = gtk::Button::builder().label("Connect Spotify").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
        let login_col = gtk::Box::new(gtk::Orientation::Vertical, 14);
        login_col.append(&id_list);
        login_col.append(&steps);
        login_col.append(&dash);
        login_col.append(&connect);
        let login = adw::StatusPage::builder().icon_name("audio-x-generic-symbolic").title("Connect Spotify").description("Search Spotify and play anything on your speakers.").child(&login_col).build();

        let search = gtk::SearchEntry::builder().placeholder_text("Search songs, artists, albums, playlists").hexpand(true).build();
        let back = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Back").visible(false).build();
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        search_row.append(&back);
        search_row.append(&search);
        let spinner = adw::Spinner::builder().height_request(32).visible(false).margin_top(24).build();
        let results = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let sp_col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        sp_col.append(&search_row);
        sp_col.append(&spinner);
        sp_col.append(&results);

        let sp_stack = gtk::Stack::new();
        sp_stack.add_named(&widgets::page(&login), Some("login"));
        sp_stack.add_named(&widgets::page(&sp_col), Some("main"));
        inner.add_titled_with_icon(&sp_stack, Some("spotify"), "Spotify", "audio-x-generic-symbolic");

        let switcher = adw::InlineViewSwitcher::builder().stack(&inner).halign(gtk::Align::Center).margin_top(12).build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&switcher);
        root.append(&inner);
        inner.set_vexpand(true);

        Self { root, fav_list, fav_empty, sp_stack, client_id, connect, search, back, results, spinner, library: RefCell::default(), generation: Cell::new(0) }
    }
}

/// How to play a Sonos or Spotify item given the user's choice.
async fn play_item(sonos: Sonos, coord: Member, uri: String, meta: String, container: bool, how: PlayHow) -> anyhow::Result<()> {
    if sonos::is_stream(&uri) {
        return sonos.play_uri(&coord, &uri, &meta, false).await;
    }
    match how {
        PlayHow::Now if container => sonos.play_uri(&coord, &uri, &meta, true).await,
        PlayHow::Now => sonos.play_now_keep_queue(&coord, &uri, &meta).await,
        PlayHow::Next => sonos.play_next(&coord, &uri, &meta).await,
        PlayHow::End => sonos.enqueue(&coord.ip, &uri, &meta, 0, false).await.map(|_| ()),
    }
}

fn play_msg(how: PlayHow, title: &str) -> String {
    match how {
        PlayHow::Now => format!("Playing “{title}”"),
        PlayHow::Next => format!("“{title}” plays next"),
        PlayHow::End => format!("Added “{title}” to the queue"),
    }
}

impl App {
    pub(super) fn connect_browse(self: &Rc<Self>) {
        let b = &self.browse;
        b.client_id.set_text(&self.core.cfg.lock().unwrap().spotify_client_id);

        let w = Rc::downgrade(self);
        b.connect.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                let id = app.browse.client_id.text().trim().to_string();
                app.spotify_login(id);
            }
        });

        let w = Rc::downgrade(self);
        b.search.connect_search_changed(move |e| {
            if let Some(app) = w.upgrade() {
                app.spotify_search(e.text().trim().to_string());
            }
        });

        let w = Rc::downgrade(self);
        b.back.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.browse.search.set_text("");
                app.spotify_search(String::new());
            }
        });
        self.update_spotify_page();
    }

    pub(super) fn browse_shown(self: &Rc<Self>) {
        self.update_spotify_page();
        if self.core.spotify.logged_in() && self.browse.library.borrow().is_none() && self.browse.search.text().is_empty() {
            self.spotify_search(String::new());
        }
    }

    pub fn update_spotify_page(&self) {
        let page = if self.core.spotify.logged_in() { "main" } else { "login" };
        self.browse.sp_stack.set_visible_child_name(page);
    }

    pub(super) fn render_favorites(self: &Rc<Self>) {
        let b = &self.browse;
        b.fav_list.remove_all();
        let favs = self.favorites.borrow().clone();
        b.fav_empty.set_visible(favs.is_empty());
        b.fav_list.set_visible(!favs.is_empty());
        for f in favs {
            if f.uri.is_empty() {
                // Sonos Radio "shortcuts" only open inside the official app.
                let row = { let r = widgets::row(&f.title, &format!("{} · opens only in the Sonos app", f.description)); r.set_sensitive(false); r };
                let img = widgets::thumb(44);
                row.add_prefix(&img);
                self.load_art(f.art.clone(), &img);
                b.fav_list.append(&row);
                continue;
            }
            let queueable = !sonos::is_stream(&f.uri);
            let fav: Item = f.clone();
            let row = widgets::media_row(self, &f.title, &f.description, f.art.clone(), queueable, move |app, how| {
                let Some(c) = app.coord() else { return };
                app.toast(&play_msg(how, &fav.title));
                let (sonos, uri, meta, container) = (app.core.sonos.clone(), fav.uri.clone(), fav.meta.clone(), fav.is_container());
                app.act(play_item(sonos, c, uri, meta, container, how));
            });
            b.fav_list.append(&row);
        }
    }

    pub fn spotify_login(self: &Rc<Self>, client_id: String) {
        if client_id.is_empty() {
            self.toast("Paste your Spotify app’s Client ID first");
            return;
        }
        {
            let mut c = self.core.cfg.lock().unwrap();
            c.spotify_client_id = client_id.clone();
            c.save();
        }
        let pending = spotify::begin_login(&client_id);
        let url = pending.url.clone();
        let sp = self.core.spotify.clone();
        let w = Rc::downgrade(self);
        // Start listening before the browser can hit the redirect.
        spawn(
            async move { tokio::time::timeout(std::time::Duration::from_secs(300), sp.finish_login(client_id, pending)).await },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                match r {
                    Ok(Ok(())) => {
                        app.toast("Spotify connected");
                        app.update_spotify_page();
                        *app.browse.library.borrow_mut() = None;
                        app.spotify_search(String::new());
                    }
                    Ok(Err(e)) => app.toast(&format!("{e:#}")),
                    Err(_) => app.toast("Spotify login timed out"),
                }
            },
        );
        if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>) {
            self.toast(&format!("Couldn't open the browser: {e}"));
        }
        self.toast("Finish signing in in your browser");
    }

    /// Empty query shows your library; otherwise search results.
    fn spotify_search(self: &Rc<Self>, q: String) {
        let b = &self.browse;
        let generation_id = b.generation.get() + 1;
        b.generation.set(generation_id);
        b.back.set_visible(false);
        if q.is_empty() {
            if let Some((pl, al)) = b.library.borrow().clone() {
                b.spinner.set_visible(false);
                self.render_spotify(vec![("Your playlists".into(), pl), ("Saved albums".into(), al)]);
                return;
            }
        }
        b.spinner.set_visible(true);
        let sp = self.core.spotify.clone();
        let w = Rc::downgrade(self);
        let is_library = q.is_empty();
        spawn(
            async move {
                if is_library {
                    let (pl, al) = tokio::join!(sp.my_playlists(), sp.my_albums());
                    // Show whichever half worked; endpoints come and go in dev mode.
                    match (pl, al) {
                        (Err(e), Err(_)) => Err(e),
                        (pl, al) => Ok(vec![("Your playlists".to_string(), pl.unwrap_or_default()), ("Saved albums".to_string(), al.unwrap_or_default())]),
                    }
                } else {
                    let items = sp.search(&q).await?;
                    let pick = |k: &str| items.iter().filter(|i| i.kind() == k).cloned().collect::<Vec<_>>();
                    Ok(vec![("Songs".to_string(), pick("track")), ("Artists".into(), pick("artist")), ("Albums".into(), pick("album")), ("Playlists".into(), pick("playlist"))])
                }
            },
            move |r: anyhow::Result<Vec<(String, Vec<SpItem>)>>| {
                let Some(app) = w.upgrade() else { return };
                if app.browse.generation.get() != generation_id {
                    return;
                }
                app.browse.spinner.set_visible(false);
                match r {
                    Ok(sections) => {
                        if is_library {
                            *app.browse.library.borrow_mut() = Some((sections[0].1.clone(), sections[1].1.clone()));
                        }
                        app.render_spotify(sections);
                    }
                    Err(e) => {
                        app.update_spotify_page();
                        app.toast(&format!("{e:#}"));
                    }
                }
            },
        );
    }

    fn show_artist(self: &Rc<Self>, artist: SpItem) {
        let b = &self.browse;
        let generation_id = b.generation.get() + 1;
        b.generation.set(generation_id);
        b.spinner.set_visible(true);
        let sp = self.core.spotify.clone();
        let w = Rc::downgrade(self);
        let name = artist.title.clone();
        spawn(async move { sp.artist_albums(&name).await }, move |r| {
            let Some(app) = w.upgrade() else { return };
            if app.browse.generation.get() != generation_id {
                return;
            }
            app.browse.spinner.set_visible(false);
            match r {
                Ok(albums) => {
                    app.browse.back.set_visible(true);
                    app.render_spotify(vec![(format!("Albums by {}", artist.title), albums)]);
                }
                Err(e) => app.toast(&format!("{e:#}")),
            }
        });
    }

    fn render_spotify(self: &Rc<Self>, sections: Vec<(String, Vec<SpItem>)>) {
        let results = &self.browse.results;
        while let Some(c) = results.first_child() {
            results.remove(&c);
        }
        let mut any = false;
        for (title, items) in sections {
            if items.is_empty() {
                continue;
            }
            any = true;
            results.append(&widgets::heading(&title));
            let list = widgets::boxed_list();
            for it in items {
                let row = if it.kind() == "artist" {
                    let row = { let r = widgets::row(&it.title, &it.subtitle); r.set_activatable(true); r };
                    let img = widgets::thumb(44);
                    img.add_css_class("circular");
                    row.add_prefix(&img);
                    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                    self.load_art(it.art.clone(), &img);
                    let w = Rc::downgrade(self);
                    row.connect_activated(move |_| {
                        if let Some(app) = w.upgrade() {
                            app.show_artist(it.clone());
                        }
                    });
                    row
                } else {
                    let item = it.clone();
                    widgets::media_row(self, &it.title, &it.subtitle, it.art.clone(), true, move |app, how| app.play_spotify(&item, how))
                };
                list.append(&row);
            }
            results.append(&list);
        }
        if !any {
            results.append(&adw::StatusPage::builder().icon_name("system-search-symbolic").title("Nothing here").build());
        }
    }

    fn play_spotify(self: &Rc<Self>, item: &SpItem, how: PlayHow) {
        let Some(c) = self.coord() else { return };
        let acc = self.spotify_acc.borrow().clone();
        let Some((uri, meta)) = sonos::spotify_target(&acc, &item.uri, &item.title) else { return };
        self.toast(&play_msg(how, &item.title));
        let container = item.kind() != "track";
        self.act(play_item(self.core.sonos.clone(), c, uri, meta, container, how));
    }
}
