use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

use super::{widgets, spawn, App, Job};

fn dialog_shell(title: &str, content: &impl IsA<gtk::Widget>, width: i32, height: i32) -> (adw::Dialog, adw::HeaderBar) {
    let header = adw::HeaderBar::new();
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(content));
    let d = adw::Dialog::builder().title(title).content_width(width).content_height(height).child(&tv).build();
    super::glass::dialog(&d);
    (d, header)
}

impl App {
    /// EQ, loudness and hardware toggles for each room in the selected group.
    pub fn show_room_settings(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        let page = adw::PreferencesPage::new();
        let spinner = adw::Spinner::builder().height_request(32).margin_top(48).build();
        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.append(&spinner);
        holder.append(&page);
        let (dialog, _) = dialog_shell("Room settings", &holder, 480, 680);
        page.set_vexpand(true);
        dialog.present(Some(&self.window));

        let sonos = self.core.sonos.clone();
        let members = g.members.clone();
        let w = Rc::downgrade(self);
        spawn(
            async move {
                let mut out = Vec::new();
                for m in members {
                    let eq = sonos.eq(&m.ip).await;
                    let vol = sonos.volume(&m.ip).await.unwrap_or(0);
                    out.push((m, eq, vol));
                }
                out
            },
            move |rooms| {
                let Some(app) = w.upgrade() else { return };
                spinner.set_visible(false);
                for (m, eq, vol) in rooms {
                    let group = adw::PreferencesGroup::builder().title(glib::markup_escape_text(&m.name)).build();
                    let eq = match eq {
                        Ok(eq) => eq,
                        Err(e) => {
                            group.set_description(Some(&glib::markup_escape_text(&format!("{e:#}"))));
                            page.add(&group);
                            continue;
                        }
                    };
                    let ip = m.ip.clone();
                    let sonos = app.core.sonos.clone();

                    let vol_row = adw::ActionRow::builder().title("Volume").build();
                    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
                    scale.set_hexpand(true);
                    scale.set_width_request(220);
                    scale.set_value(vol as f64);
                    vol_row.add_suffix(&scale);
                    group.add(&vol_row);
                    let job: Job = {
                        let (s, ip) = (sonos.clone(), ip.clone());
                        Rc::new(move |v| {
                            let (s, ip) = (s.clone(), ip.clone());
                            Box::pin(async move { s.set_volume(&ip, v as u8).await })
                        })
                    };
                    let (wk, key) = (Rc::downgrade(&app), format!("vol:{ip}"));
                    scale.connect_change_value(move |_, _, v| {
                        if let Some(a) = wk.upgrade() {
                            a.throttled(&key, v.clamp(0.0, 100.0), job.clone());
                        }
                        glib::Propagation::Proceed
                    });

                    for (title, value, is_bass) in [("Bass", eq.bass, true), ("Treble", eq.treble, false)] {
                        let row = adw::SpinRow::with_range(-10.0, 10.0, 1.0);
                        row.set_title(title);
                        row.set_value(value as f64);
                        let (s, ip2) = (sonos.clone(), ip.clone());
                        let job: Job = Rc::new(move |v| {
                            let (s, ip) = (s.clone(), ip2.clone());
                            Box::pin(async move { if is_bass { s.set_bass(&ip, v as i32).await } else { s.set_treble(&ip, v as i32).await } })
                        });
                        let (wk, key) = (Rc::downgrade(&app), format!("{title}:{ip}"));
                        row.connect_notify_local(Some("value"), move |r, _| {
                            if let Some(a) = wk.upgrade() {
                                a.throttled(&key, r.value(), job.clone());
                            }
                        });
                        group.add(&row);
                    }

                    type Setter = fn(crate::sonos::Sonos, String, bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;
                    let mut toggles: Vec<(&str, &str, bool, Setter)> = vec![(
                        "Loudness",
                        "Boosts bass and treble at low volume",
                        eq.loudness,
                        |s, ip, on| Box::pin(async move { s.set_loudness(&ip, on).await }),
                    )];
                    if let Some(on) = eq.night {
                        toggles.push(("Night sound", "Quieter loud parts, clearer quiet ones", on, |s, ip, on| Box::pin(async move { s.set_eq(&ip, "NightMode", on).await })));
                    }
                    if let Some(on) = eq.speech {
                        toggles.push(("Speech enhancement", "Clearer dialogue", on, |s, ip, on| Box::pin(async move { s.set_eq(&ip, "DialogLevel", on).await })));
                    }
                    toggles.push(("Status light", "The light on the speaker", eq.led, |s, ip, on| Box::pin(async move { s.set_led(&ip, on).await })));
                    toggles.push(("Lock buttons", "Ignore the speaker's touch controls", eq.buttons_locked, |s, ip, on| Box::pin(async move { s.set_buttons_locked(&ip, on).await })));
                    for (title, sub, on, set) in toggles {
                        let row = adw::SwitchRow::builder().title(title).subtitle(sub).active(on).build();
                        let (wk, s, ip2) = (Rc::downgrade(&app), sonos.clone(), ip.clone());
                        row.connect_active_notify(move |r| {
                            if let Some(a) = wk.upgrade() {
                                a.act(set(s.clone(), ip2.clone(), r.is_active()));
                            }
                        });
                        group.add(&row);
                    }
                    page.add(&group);
                }
            },
        );
    }

    /// Tick the rooms that should play together with the selected one.
    pub fn show_group_dialog(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        let all = self.all_members();
        let list = super::widgets::boxed_list();
        let mut checks = Vec::new();
        for m in &all {
            let is_coord = m.uuid == g.coordinator.uuid;
            let check = gtk::CheckButton::builder().active(g.members.iter().any(|x| x.uuid == m.uuid)).sensitive(!is_coord).valign(gtk::Align::Center).build();
            let row = { let r = widgets::row(&m.name, ""); r.set_activatable_widget(Some(&check)); r };
            if is_coord {
                row.set_subtitle("Plays the music for this group");
            } else if let Some(other) = self.groups.borrow().iter().find(|x| x.members.iter().any(|y| y.uuid == m.uuid) && x.coordinator.uuid != g.coordinator.uuid) {
                if other.members.len() > 1 || other.coordinator.uuid != m.uuid {
                    row.set_subtitle(&format!("Now in {}", other.name()));
                }
            }
            row.add_prefix(&check);
            list.append(&row);
            checks.push((m.clone(), check));
        }
        let all_btn = gtk::Button::builder().label("Select all").css_classes(["flat"]).halign(gtk::Align::Start).build();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.append(&gtk::Label::builder().label(format!("Rooms playing with {}", g.coordinator.name)).css_classes(["heading"]).xalign(0.0).build());
        col.append(&list);
        col.append(&all_btn);
        let (dialog, header) = dialog_shell("Group rooms", &super::widgets::page(&col), 420, 480);
        let apply = gtk::Button::builder().label("Done").css_classes(["suggested-action"]).build();
        header.pack_end(&apply);
        header.set_show_end_title_buttons(false);

        let cs: Vec<gtk::CheckButton> = checks.iter().map(|(_, c)| c.clone()).collect();
        all_btn.connect_clicked(move |_| cs.iter().for_each(|c| c.set_active(true)));

        let w = Rc::downgrade(self);
        let d = dialog.downgrade();
        apply.connect_clicked(move |_| {
            let Some(app) = w.upgrade() else { return };
            let coord = g.coordinator.clone();
            let mut joins = Vec::new();
            let mut leaves = Vec::new();
            for (m, c) in &checks {
                let was = g.members.iter().any(|x| x.uuid == m.uuid);
                match (was, c.is_active()) {
                    (false, true) => joins.push(m.ip.clone()),
                    (true, false) if m.uuid != coord.uuid => leaves.push(m.ip.clone()),
                    _ => {}
                }
            }
            if let Some(d) = d.upgrade() {
                d.close();
            }
            if joins.is_empty() && leaves.is_empty() {
                return;
            }
            let sonos = app.core.sonos.clone();
            let w2 = Rc::downgrade(&app);
            spawn(
                async move {
                    for ip in leaves {
                        sonos.leave(&ip).await?;
                    }
                    for ip in joins {
                        sonos.join(&ip, &coord.uuid).await?;
                    }
                    anyhow::Ok(())
                },
                move |r| {
                    let Some(app) = w2.upgrade() else { return };
                    if let Err(e) = r {
                        app.toast(&format!("{e:#}"));
                    }
                    // Sonos needs a moment before the new topology shows up.
                    let w3 = Rc::downgrade(&app);
                    glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
                        if let Some(app) = w3.upgrade() {
                            app.refresh_topology();
                        }
                    });
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    pub fn show_spotify_dialog(self: &Rc<Self>) {
        let group = adw::PreferencesGroup::builder().title("Spotify").build();
        let acc = self.spotify_acc.borrow().clone();
        let page = adw::PreferencesPage::new();
        if self.core.spotify.logged_in() {
            group.set_description(Some("Connected. Search and your library are available under Browse → Spotify."));
            let out = gtk::Button::builder().label("Disconnect").css_classes(["destructive-action"]).valign(gtk::Align::Center).build();
            let row = adw::ActionRow::builder().title("Spotify Web API").subtitle("Signed in").build();
            row.add_suffix(&out);
            group.add(&row);
            let w = Rc::downgrade(self);
            let g2 = group.clone();
            out.connect_clicked(move |b| {
                if let Some(app) = w.upgrade() {
                    app.core.spotify.logout();
                    *app.browse.library.borrow_mut() = None;
                    app.update_spotify_page();
                    b.set_sensitive(false);
                    g2.set_description(Some("Disconnected."));
                }
            });
        } else {
            group.set_description(Some("Not connected. Open Browse → Spotify to connect."));
        }
        let sonos_group = adw::PreferencesGroup::builder().title("Sonos playback account").description("Spotify plays through the account linked in your Sonos system. Sonance finds it from your favorites, alarms and queue.").build();
        sonos_group.add(&adw::ActionRow::builder().title("Account serial (sn)").subtitle(&acc.sn).css_classes(["property"]).build());
        page.add(&group);
        page.add(&sonos_group);
        let (dialog, _) = dialog_shell("Spotify account", &page, 460, 420);
        dialog.present(Some(&self.window));
    }

    pub fn show_about(self: &Rc<Self>) {
        let about = adw::AboutDialog::builder()
            .application_name("Sonance")
            .application_icon("audio-speakers")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A native Sonos controller for Linux.")
            .developer_name("Guy")
            .build();
        about.present(Some(&self.window));
    }
}
