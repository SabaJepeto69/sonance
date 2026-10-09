use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use super::{spawn, widgets, App};

/// Rows past this are not drawn; Sonos queues can hold 40k tracks.
const MAX_ROWS: usize = 1000;

pub struct QueueView {
    pub root: gtk::ScrolledWindow,
    list: gtk::ListBox,
    count: gtk::Label,
    clear: gtk::Button,
    empty: adw::StatusPage,
    rows: RefCell<Vec<(adw::ActionRow, gtk::Image)>>,
    version: RefCell<Option<(String, usize)>>,
}

impl QueueView {
    pub fn new() -> Self {
        let count = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["heading"]).build();
        let clear = gtk::Button::builder().label("Clear queue").css_classes(["flat", "destructive-action"]).build();
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.set_margin_bottom(8);
        top.append(&count);
        top.append(&clear);
        let list = widgets::boxed_list();
        let empty = adw::StatusPage::builder().icon_name("view-list-symbolic").title("The queue is empty").description("Play something from Browse, or add songs with “Add to queue”.").build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        col.append(&top);
        col.append(&list);
        col.append(&empty);
        Self { root: widgets::page(&col), list, count, clear, empty, rows: RefCell::default(), version: RefCell::default() }
    }

    pub fn invalidate(&self) {
        *self.version.borrow_mut() = None;
    }

    pub fn highlight(&self, track_no: u32) {
        for (i, (row, icon)) in self.rows.borrow().iter().enumerate() {
            let current = i as u32 + 1 == track_no;
            icon.set_visible(current);
            if current {
                row.add_css_class("queue-current");
            } else {
                row.remove_css_class("queue-current");
            }
        }
    }
}

impl App {
    pub(super) fn connect_queue(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.queue.clear.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let Some(c) = app.coord() else { return };
            let sonos = app.core.sonos.clone();
            let w2 = Rc::downgrade(&app);
            spawn(async move { sonos.clear_queue(&c.ip).await }, move |r| {
                let Some(app) = w2.upgrade() else { return };
                if let Err(e) = r {
                    app.toast(&format!("{e:#}"));
                }
                app.refresh_queue(true);
                app.poll_status();
            });
        });
    }

    /// Reloads the queue; without `force` only when the speaker says it changed.
    pub fn refresh_queue(self: &Rc<Self>, force: bool) {
        let Some(c) = self.coord() else { return };
        let sonos = self.core.sonos.clone();
        let known = if force { None } else { self.queue.version.borrow().clone() };
        let w = Rc::downgrade(self);
        let uuid = c.uuid.clone();
        spawn(
            async move {
                let v = sonos.queue_version(&c.ip).await?;
                if known.as_ref() == Some(&v) {
                    return anyhow::Ok(None);
                }
                Ok(Some((v, sonos.queue(&c.ip).await?)))
            },
            move |r| {
                let Some(app) = w.upgrade() else { return };
                if app.selected.borrow().as_deref() != Some(uuid.as_str()) {
                    return;
                }
                if let Ok(Some((v, items))) = r {
                    *app.queue.version.borrow_mut() = Some(v);
                    app.render_queue(items);
                }
            },
        );
    }

    fn render_queue(self: &Rc<Self>, items: Vec<crate::sonos::Item>) {
        let q = &self.queue;
        q.list.remove_all();
        q.rows.borrow_mut().clear();
        q.count.set_label(&match items.len() {
            1 => "1 song".to_string(),
            n => format!("{n} songs"),
        });
        q.clear.set_sensitive(!items.is_empty());
        q.empty.set_visible(items.is_empty());
        q.list.set_visible(!items.is_empty());
        let total = items.len() as u32;
        let update_id = q.version.borrow().as_ref().map(|v| v.0.clone()).unwrap_or_else(|| "0".into());
        for (i, it) in items.into_iter().take(MAX_ROWS).enumerate() {
            let n = i as u32 + 1;
            let sub = [it.artist.as_str(), it.album.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
            let row = { let r = widgets::row(&it.title, &sub); r.set_activatable(true); r };
            row.set_title_lines(1);
            row.set_subtitle_lines(1);
            let img = widgets::thumb(40);
            row.add_prefix(&img);
            self.load_art(it.art.clone(), &img);
            let playing = gtk::Image::from_icon_name("media-playback-start-symbolic");
            playing.add_css_class("accent");
            playing.set_visible(false);
            playing.set_valign(gtk::Align::Center);
            row.add_suffix(&playing);
            if it.duration > 0 {
                row.add_suffix(&gtk::Label::builder().label(crate::sonos::didl::fmt_time(it.duration)).css_classes(["dim-label", "numeric", "caption"]).build());
            }

            let mk = |f: fn(crate::sonos::Sonos, String, u32, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>| -> Box<dyn Fn()> {
                let w = Rc::downgrade(self);
                let update_id = update_id.clone();
                Box::new(move || {
                    let Some(app) = w.upgrade() else { return };
                    let Some(c) = app.coord() else { return };
                    let fut = f(app.core.sonos.clone(), c.ip, n, update_id.clone());
                    let w2 = Rc::downgrade(&app);
                    spawn(fut, move |r| {
                        let Some(app) = w2.upgrade() else { return };
                        if let Err(e) = r {
                            app.toast(&format!("{e:#}"));
                        }
                        app.refresh_queue(true);
                        app.poll_status();
                    });
                })
            };
            let mut entries: Vec<(&'static str, Box<dyn Fn()>)> = Vec::new();
            if n > 1 {
                entries.push(("Move to top", mk(|s, ip, n, u| Box::pin(async move { s.move_in_queue(&ip, n, 1, &u).await }))));
                entries.push(("Move up", mk(|s, ip, n, u| Box::pin(async move { s.move_in_queue(&ip, n, n - 1, &u).await }))));
            }
            if n < total {
                // InsertBefore counts positions in the queue as it was before the move.
                entries.push(("Move down", mk(|s, ip, n, u| Box::pin(async move { s.move_in_queue(&ip, n, n + 2, &u).await }))));
            }
            entries.push(("Remove", mk(|s, ip, n, u| Box::pin(async move { s.remove_from_queue(&ip, n, &u).await }))));
            row.add_suffix(&widgets::flat_menu(entries));

            let w = Rc::downgrade(self);
            row.connect_activated(move |_| {
                let Some(app) = w.upgrade() else { return };
                let Some(c) = app.coord() else { return };
                let sonos = app.core.sonos.clone();
                app.act(async move { sonos.play_from_queue(&c, n).await });
            });
            q.list.append(&row);
            q.rows.borrow_mut().push((row, playing));
        }
        if total as usize > MAX_ROWS {
            q.list.append(&adw::ActionRow::builder().title(format!("…and {} more", total as usize - MAX_ROWS)).build());
        }
        let s = self.status.borrow();
        q.highlight(if s.from_queue { s.track_no } else { 0 });
    }
}
