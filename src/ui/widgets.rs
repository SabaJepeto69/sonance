use adw::prelude::*;
use std::rc::Rc;

use super::App;

#[derive(Clone, Copy, PartialEq)]
pub enum PlayHow {
    Now,
    Next,
    End,
}

pub fn thumb(size: i32) -> gtk::Image {
    let img = gtk::Image::from_icon_name("folder-music-symbolic");
    img.set_pixel_size(size);
    img.add_css_class("thumb");
    img.set_overflow(gtk::Overflow::Hidden);
    img
}

/// An ActionRow showing plain text: song titles are full of `&`, which the
/// default markup mode would choke on.
pub fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let r = adw::ActionRow::new();
    r.set_use_markup(false);
    r.set_title(title);
    r.set_subtitle(subtitle);
    r
}

pub fn boxed_list() -> gtk::ListBox {
    let l = gtk::ListBox::new();
    l.set_selection_mode(gtk::SelectionMode::None);
    l.add_css_class("boxed-list");
    l
}

/// A centred, scrollable column the width of a typical settings page.
pub fn page(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder().maximum_size(860).child(child).margin_top(18).margin_bottom(130).margin_start(12).margin_end(12).build();
    gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build()
}

pub fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(["heading"]).margin_top(18).margin_bottom(6).build()
}

pub fn flat_menu(entries: Vec<(&'static str, Box<dyn Fn()>)>) -> gtk::MenuButton {
    let pop = gtk::Popover::new();
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, f) in entries {
        let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let p = pop.downgrade();
        b.connect_clicked(move |_| {
            if let Some(p) = p.upgrade() {
                p.popdown();
            }
            f();
        });
        bx.append(&b);
    }
    pop.set_child(Some(&bx));
    super::glass::popover(&pop);
    gtk::MenuButton::builder().icon_name("view-more-symbolic").css_classes(["flat"]).valign(gtk::Align::Center).popover(&pop).build()
}

/// A track/album/playlist row: click plays now, the menu offers queue options.
pub fn media_row(
    app: &Rc<App>,
    title: &str,
    subtitle: &str,
    art: Option<String>,
    queueable: bool,
    on_play: impl Fn(&Rc<App>, PlayHow) + 'static,
) -> adw::ActionRow {
    let row = { let r = row(title, subtitle); r.set_activatable(true); r };
    row.set_subtitle_lines(1);
    row.set_title_lines(1);
    let img = thumb(44);
    row.add_prefix(&img);
    app.load_art(art, &img);
    let on_play = Rc::new(on_play);
    if queueable {
        let mk = |how: PlayHow| -> Box<dyn Fn()> {
            let w = Rc::downgrade(app);
            let f = on_play.clone();
            Box::new(move || {
                if let Some(a) = w.upgrade() {
                    f(&a, how)
                }
            })
        };
        row.add_suffix(&flat_menu(vec![("Play now", mk(PlayHow::Now)), ("Play next", mk(PlayHow::Next)), ("Add to end of queue", mk(PlayHow::End))]));
    }
    let w = Rc::downgrade(app);
    row.connect_activated(move |_| {
        if let Some(a) = w.upgrade() {
            on_play(&a, PlayHow::Now)
        }
    });
    row
}
