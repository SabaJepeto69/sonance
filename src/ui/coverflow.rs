//! The Now Playing carousel: the current song large in the middle, its
//! neighbours smaller and dimmed on either side. Drag, swipe, scroll
//! sideways or click a side cover to change songs.
//!
//! Cards sit in a `gtk::Fixed` and are placed purely by transforms, so the
//! whole strip can glide continuously. Position is `slot - offset`: slot is a
//! card's index relative to `center` (a queue position), offset is how far the
//! strip has been pushed. `(center, offset)` and `(center + k, offset - k)`
//! draw identically, which is what lets a skip land without a jump.

use adw::prelude::*;
use gtk::{glib, graphene, gsk};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct CardInfo {
    pub title: String,
    pub artist: String,
    pub art: Option<String>,
}

pub type ArtLoader = Rc<dyn Fn(Option<String>, &gtk::Picture)>;
/// (queue position before, queue position after)
pub type CommitFn = Rc<dyn Fn(i64, i64)>;
/// The song nearest the middle while the strip is moving.
pub type FocusFn = Rc<dyn Fn(Option<CardInfo>)>;

const SLOTS: [i64; 5] = [-2, -1, 0, 1, 2];

struct Card {
    slot: i64,
    frame: gtk::Overlay,
    pic: gtk::Picture,
    placeholder: gtk::Image,
    has_content: Cell<bool>,
    shown_art: RefCell<Option<Option<String>>>,
}

pub struct CoverFlow {
    me: std::rc::Weak<CoverFlow>,
    pub root: gtk::Overlay,
    fixed: gtk::Fixed,
    cards: Vec<Card>,
    size: Cell<(f64, f64)>,
    center: Cell<i64>,
    offset: Cell<f64>,
    target: Cell<f64>,
    dragging: Cell<bool>,
    drag_base: Cell<f64>,
    animating: Cell<bool>,
    last_frame: Cell<i64>,
    /// After a skip, polls still report the old song for a moment; ignore them.
    hold_until: Cell<Option<Instant>>,
    items: RefCell<HashMap<i64, CardInfo>>,
    /// Skipping is possible at all (playing from the queue).
    enabled: Cell<bool>,
    /// Neighbours are unknown (shuffle) but skipping still works.
    blind: Cell<bool>,
    order: RefCell<Vec<usize>>,
    focused: Cell<i64>,
    loader: RefCell<Option<ArtLoader>>,
    on_commit: RefCell<Option<CommitFn>>,
    on_focus: RefCell<Option<FocusFn>>,
}

impl CoverFlow {
    pub fn new() -> Rc<Self> {
        let sizer = gtk::DrawingArea::new();
        sizer.set_hexpand(true);
        sizer.set_size_request(-1, 380);
        let fixed = gtk::Fixed::new();
        let root = gtk::Overlay::new();
        root.set_child(Some(&sizer));
        root.add_overlay(&fixed);
        root.set_overflow(gtk::Overflow::Visible);

        let cards = SLOTS
            .iter()
            .map(|&slot| {
                let pic = gtk::Picture::new();
                pic.set_content_fit(gtk::ContentFit::Cover);
                pic.set_can_shrink(true);
                let placeholder = gtk::Image::from_icon_name("folder-music-symbolic");
                placeholder.set_pixel_size(72);
                let frame = gtk::Overlay::new();
                frame.set_child(Some(&pic));
                frame.add_overlay(&placeholder);
                frame.add_css_class("cover-card");
                frame.set_overflow(gtk::Overflow::Hidden);
                frame.set_cursor_from_name(Some(if slot == 0 { "grab" } else { "pointer" }));
                fixed.put(&frame, 0.0, 0.0);
                Card { slot, frame, pic, placeholder, has_content: Cell::new(false), shown_art: RefCell::new(None) }
            })
            .collect();

        let cf = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            root,
            fixed,
            cards,
            size: Cell::new((0.0, 0.0)),
            center: Cell::new(0),
            offset: Cell::new(0.0),
            target: Cell::new(0.0),
            dragging: Cell::new(false),
            drag_base: Cell::new(0.0),
            animating: Cell::new(false),
            last_frame: Cell::new(0),
            hold_until: Cell::new(None),
            items: RefCell::default(),
            enabled: Cell::new(false),
            blind: Cell::new(false),
            order: RefCell::default(),
            focused: Cell::new(0),
            loader: RefCell::default(),
            on_commit: RefCell::default(),
            on_focus: RefCell::default(),
        });

        let w = Rc::downgrade(&cf);
        sizer.connect_resize(move |_, width, height| {
            if let Some(cf) = w.upgrade() {
                cf.size.set((width as f64, height as f64));
                let s = cf.card_size() as i32;
                for c in &cf.cards {
                    c.frame.set_size_request(s, s);
                }
                cf.layout();
            }
        });

        // Clicking a side cover jumps to it.
        for (i, card) in cf.cards.iter().enumerate() {
            let click = gtk::GestureClick::new();
            let w = Rc::downgrade(&cf);
            click.connect_released(move |g, _, _, _| {
                let Some(cf) = w.upgrade() else { return };
                let slot = cf.cards[i].slot;
                if slot != 0 && !cf.dragging.get() {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    cf.go(slot);
                }
            });
            card.frame.add_controller(click);
        }

        // Dragging pushes the strip; letting go settles on the nearest song.
        let drag = gtk::GestureDrag::new();
        let w = Rc::downgrade(&cf);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(cf) = w.upgrade() {
                cf.dragging.set(true);
                cf.drag_base.set(cf.offset.get());
            }
        });
        let w = Rc::downgrade(&cf);
        drag.connect_drag_update(move |_, dx, _| {
            if let Some(cf) = w.upgrade() {
                cf.offset.set(cf.rubber_band(cf.drag_base.get() - dx / cf.spacing()));
                cf.layout();
            }
        });
        let w = Rc::downgrade(&cf);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(cf) = w.upgrade() {
                cf.dragging.set(false);
                cf.settle();
            }
        });
        cf.root.add_controller(drag);

        // Touchpad swipes and horizontal wheels.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::HORIZONTAL);
        let w = Rc::downgrade(&cf);
        scroll.connect_scroll(move |c, dx, _| {
            let Some(cf) = w.upgrade() else { return glib::Propagation::Proceed };
            if dx == 0.0 {
                return glib::Propagation::Proceed;
            }
            if c.unit() == gtk::gdk::ScrollUnit::Wheel {
                cf.go(if dx > 0.0 { 1 } else { -1 });
            } else {
                cf.dragging.set(true);
                cf.offset.set(cf.rubber_band(cf.offset.get() + dx / cf.spacing()));
                cf.layout();
            }
            glib::Propagation::Stop
        });
        let w = Rc::downgrade(&cf);
        scroll.connect_scroll_end(move |_| {
            if let Some(cf) = w.upgrade() {
                if cf.dragging.get() {
                    cf.dragging.set(false);
                    cf.settle();
                }
            }
        });
        cf.root.add_controller(scroll);
        cf
    }

    pub fn set_loader(&self, f: ArtLoader) {
        *self.loader.borrow_mut() = Some(f);
    }
    pub fn set_on_commit(&self, f: CommitFn) {
        *self.on_commit.borrow_mut() = Some(f);
    }
    pub fn set_on_focus(&self, f: FocusFn) {
        *self.on_focus.borrow_mut() = Some(f);
    }

    /// True while the user is moving the strip or it is still gliding.
    pub fn busy(&self) -> bool {
        self.dragging.get() || self.animating.get()
    }

    /// New data from the speaker. `center` is the queue position playing now.
    pub fn set_items(&self, center: i64, items: HashMap<i64, CardInfo>, enabled: bool, blind: bool) {
        *self.items.borrow_mut() = items;
        self.enabled.set(enabled);
        self.blind.set(blind);
        self.sync(center);
        self.refresh_cards();
        self.layout();
    }

    /// Follows the speaker to `center`, gliding if it's a near neighbour.
    pub fn sync(&self, center: i64) {
        let held = self.hold_until.get().is_some_and(|t| Instant::now() < t);
        if self.busy() || held {
            return;
        }
        let old = self.center.get();
        if center == old {
            return;
        }
        self.center.set(center);
        if (center - old).abs() <= 2 && self.size.get().0 > 0.0 {
            self.offset.set(self.offset.get() + (old - center) as f64);
            self.refresh_cards();
            self.animate_to(0.0);
        } else {
            self.offset.set(0.0);
            self.refresh_cards();
            self.layout();
        }
    }

    fn can_go(&self, delta: i64) -> bool {
        self.enabled.get() && (self.blind.get() && delta.abs() == 1 || self.items.borrow().contains_key(&(self.center.get() + delta)))
    }

    fn go(&self, delta: i64) {
        if self.busy() || !self.can_go(delta) {
            return;
        }
        self.animate_to(delta as f64);
    }

    /// Where a released drag should come to rest.
    fn settle(&self) {
        let o = self.offset.get();
        let mut t = o.round().clamp(-2.0, 2.0);
        if t == 0.0 && o.abs() > 0.22 {
            t = o.signum();
        }
        if t != 0.0 && !self.can_go(t as i64) {
            t = 0.0;
        }
        self.animate_to(t);
    }

    /// Resists dragging past the songs that exist.
    fn rubber_band(&self, o: f64) -> f64 {
        let max = [2, 1].into_iter().find(|&d| self.can_go(d)).unwrap_or(0) as f64;
        let min = -([2, 1].into_iter().find(|&d| self.can_go(-d)).unwrap_or(0) as f64);
        if o > max {
            max + (o - max) * 0.25
        } else if o < min {
            min + (o - min) * 0.25
        } else {
            o
        }
    }

    fn animate_to(&self, target: f64) {
        self.target.set(target);
        if self.animating.replace(true) {
            return;
        }
        self.last_frame.set(0);
        let me = self.me.clone();
        self.fixed.add_tick_callback(move |_, clock| match me.upgrade() {
            Some(cf) => cf.tick(clock.frame_time()),
            None => glib::ControlFlow::Break,
        });
    }

    fn tick(&self, now: i64) -> glib::ControlFlow {
        if self.dragging.get() {
            self.animating.set(false);
            return glib::ControlFlow::Break;
        }
        let last = self.last_frame.replace(now);
        let dt = if last == 0 { 1.0 / 60.0 } else { ((now - last) as f64 / 1e6).clamp(0.0, 0.05) };
        let (o, t) = (self.offset.get(), self.target.get());
        let next = o + (t - o) * (1.0 - (-dt * 13.0).exp());
        if (t - next).abs() < 0.004 {
            self.offset.set(t);
            self.animating.set(false);
            self.finish();
            return glib::ControlFlow::Break;
        }
        self.offset.set(next);
        self.layout();
        glib::ControlFlow::Continue
    }

    /// The strip came to rest: if it rests on another song, that song is now the center.
    fn finish(&self) {
        let t = self.target.get().round() as i64;
        if t != 0 {
            let from = self.center.get();
            self.center.set(from + t);
            self.offset.set(0.0);
            self.target.set(0.0);
            self.hold_until.set(Some(Instant::now() + Duration::from_millis(2500)));
            self.refresh_cards();
            if let Some(f) = self.on_commit.borrow().clone() {
                f(from, from + t);
            }
        }
        self.layout();
    }

    fn card_size(&self) -> f64 {
        let (w, h) = self.size.get();
        // Leave room under the cards: anything drawn past this area gets clipped,
        // and the shadows reach ~60px below the cover.
        (h * 0.72).min(w * 0.36).max(80.0)
    }

    fn spacing(&self) -> f64 {
        self.card_size() * 0.78
    }

    fn refresh_cards(&self) {
        let items = self.items.borrow();
        let loader = self.loader.borrow().clone();
        for c in &self.cards {
            let info = items.get(&(self.center.get() + c.slot));
            let unknown_neighbour = info.is_none() && self.blind.get() && self.enabled.get() && c.slot.abs() == 1;
            c.has_content.set(info.is_some() || unknown_neighbour);
            match info {
                Some(info) => {
                    c.placeholder.set_icon_name(Some("folder-music-symbolic"));
                    c.placeholder.set_visible(info.art.is_none());
                    if c.shown_art.borrow().as_ref() != Some(&info.art) {
                        *c.shown_art.borrow_mut() = Some(info.art.clone());
                        if let Some(load) = &loader {
                            load(info.art.clone(), &c.pic);
                        }
                    }
                    c.pic.set_visible(true);
                }
                None => {
                    // Shuffle: Sonos doesn't say what's next, so show a hint instead of a cover.
                    c.placeholder.set_icon_name(Some("media-playlist-shuffle-symbolic"));
                    c.placeholder.set_visible(true);
                    c.pic.set_visible(false);
                    *c.shown_art.borrow_mut() = None;
                }
            }
        }
    }

    fn layout(&self) {
        let (w, h) = self.size.get();
        if w <= 0.0 {
            return;
        }
        let s = self.card_size();
        let (cx, cy) = (w / 2.0, h * 0.44);
        let mut order: Vec<(usize, f64)> = Vec::new();
        for (i, c) in self.cards.iter().enumerate() {
            let p = c.slot as f64 - self.offset.get();
            let a = p.abs();
            // Two songs out only shows while the strip is moving.
            let visible = c.has_content.get() && a < 1.9;
            c.frame.set_visible(visible);
            if !visible {
                continue;
            }
            // Flat, like Apple Music: neighbours shrink, dim and tuck behind the middle.
            // (A 3D tilt looked great until GSK smeared the shadows and rounded clips
            // of perspective-transformed cards.)
            let near = a.min(1.0);
            let far = (a - 1.0).max(0.0);
            let x = cx + p.signum() * (near * s * 0.68 + far * s * 0.42);
            let scale = 1.0 - 0.24 * near - 0.12 * far;
            let t = gsk::Transform::new()
                .translate(&graphene::Point::new(x as f32, cy as f32))
                .scale(scale as f32, scale as f32)
                .translate(&graphene::Point::new(-(s / 2.0) as f32, -(s / 2.0) as f32));
            self.fixed.set_child_transform(&c.frame, Some(&t));
            // Fade the cover image, never the card: opacity on the card makes GSK
            // render it offscreen, which crops its soft shadow into a hard box.
            c.pic.set_opacity((1.0 - 0.5 * near - 0.3 * far).clamp(0.1, 1.0));
            order.push((i, a));
        }
        // Nearest the middle draws last, on top.
        order.sort_by(|x, y| y.1.total_cmp(&x.1));
        let ids: Vec<usize> = order.iter().map(|o| o.0).collect();
        if *self.order.borrow() != ids {
            for &i in &ids {
                self.cards[i].frame.insert_before(&self.fixed, None::<&gtk::Widget>);
            }
            *self.order.borrow_mut() = ids;
        }
        // Tell the page which song the strip is showing, so titles follow the drag.
        let near = self.center.get() + self.offset.get().round() as i64;
        if self.focused.replace(near) != near {
            if let Some(f) = self.on_focus.borrow().clone() {
                f(self.items.borrow().get(&near).cloned());
            }
        }
    }
}
