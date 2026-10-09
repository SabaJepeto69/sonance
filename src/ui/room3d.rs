//! The 3D room view: a Cairo-drawn perspective scene with an orbit camera.
//! Drag empty space to orbit, drag a speaker or spot across the floor to move
//! it, scroll over a speaker to turn it, scroll elsewhere to zoom.

use gtk::{cairo, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::f32::consts::PI;
use std::rc::{Rc, Weak};

use crate::room::{facing, plan::Plan, Layout};

type V3 = [f32; 3];

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn mul(a: V3, k: f32) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn norm(a: V3) -> V3 {
    let l = dot(a, a).sqrt().max(1e-6);
    mul(a, 1.0 / l)
}

#[derive(Clone, Copy)]
struct Camera {
    yaw: f32,
    pitch: f32,
    dist: f32,
    target: V3,
}

struct View {
    eye: V3,
    right: V3,
    up: V3,
    fwd: V3,
    focal: f32,
    cx: f32,
    cy: f32,
}

impl Camera {
    fn view(&self, w: f32, h: f32) -> View {
        let (y, p) = (self.yaw.to_radians(), self.pitch.to_radians());
        let eye = add(self.target, mul([p.cos() * y.sin(), p.sin(), p.cos() * y.cos()], self.dist));
        let fwd = norm(sub(self.target, eye));
        let right = norm(cross(fwd, [0.0, 1.0, 0.0]));
        let up = cross(right, fwd);
        View { eye, right, up, fwd, focal: h.min(w) * 1.05, cx: w / 2.0, cy: h / 2.0 }
    }
}

impl View {
    fn project(&self, p: V3) -> Option<(f64, f64, f32)> {
        let v = sub(p, self.eye);
        let z = dot(v, self.fwd);
        if z < 0.05 {
            return None;
        }
        let x = self.cx + self.focal * dot(v, self.right) / z;
        let y = self.cy - self.focal * dot(v, self.up) / z;
        Some((x as f64, y as f64, z))
    }

    fn ray(&self, sx: f32, sy: f32) -> (V3, V3) {
        let d = add(self.fwd, add(mul(self.right, (sx - self.cx) / self.focal), mul(self.up, -(sy - self.cy) / self.focal)));
        (self.eye, norm(d))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    Speaker(String),
    Spot(String),
}

pub struct Room3d {
    pub area: gtk::DrawingArea,
    pub layout: Rc<RefCell<Layout>>,
    pub plan: RefCell<Option<Plan>>,
    /// Speaker ids that are playing right now (drawn lit, with rays).
    pub live: RefCell<Vec<String>>,
    pub selected: RefCell<Option<Pick>>,
    cam: Cell<Camera>,
    cam_base: Cell<Camera>,
    dragging: RefCell<Option<Pick>>,
    hits: RefCell<Vec<(Pick, f64, f64, f64)>>,
    pointer: Cell<(f64, f64)>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    on_select: RefCell<Option<Box<dyn Fn()>>>,
    me: Weak<Room3d>,
}

const ACCENT: (f64, f64, f64) = (1.0, 0.216, 0.373);
const BT_BLUE: (f64, f64, f64) = (0.39, 0.82, 1.0);

impl Room3d {
    pub fn new(layout: Rc<RefCell<Layout>>) -> Rc<Self> {
        let area = gtk::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_focusable(true);
        let target = {
            let r = &layout.borrow().room;
            [r.width / 2.0, 0.6, r.depth / 2.0]
        };
        let cam = Camera { yaw: 25.0, pitch: 38.0, dist: 7.5, target };
        let r = Rc::new_cyclic(|me| Self {
            area,
            layout,
            plan: RefCell::new(None),
            live: RefCell::default(),
            selected: RefCell::new(None),
            cam: Cell::new(cam),
            cam_base: Cell::new(cam),
            dragging: RefCell::new(None),
            hits: RefCell::default(),
            pointer: Cell::new((0.0, 0.0)),
            on_change: RefCell::default(),
            on_select: RefCell::default(),
            me: me.clone(),
        });

        let w = r.me.clone();
        r.area.set_draw_func(move |_, cr, width, height| {
            if let Some(r) = w.upgrade() {
                r.draw(cr, width as f32, height as f32);
            }
        });

        let drag = gtk::GestureDrag::new();
        let w = r.me.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let Some(r) = w.upgrade() else { return };
            let hit = r.hit(x, y);
            if hit.is_some() && hit != *r.selected.borrow() {
                *r.selected.borrow_mut() = hit.clone();
                r.notify_select();
            }
            *r.dragging.borrow_mut() = hit;
            r.cam_base.set(r.cam.get());
        });
        let w = r.me.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(r) = w.upgrade() else { return };
            let pick = r.dragging.borrow().clone();
            match pick {
                Some(p) => {
                    let Some((sx, sy)) = g.start_point() else { return };
                    r.move_on_floor(&p, (sx + dx) as f32, (sy + dy) as f32);
                }
                None => {
                    let mut c = r.cam_base.get();
                    c.yaw -= dx as f32 * 0.35;
                    c.pitch = (c.pitch + dy as f32 * 0.25).clamp(8.0, 86.0);
                    r.cam.set(c);
                }
            }
            r.area.queue_draw();
        });
        let w = r.me.clone();
        drag.connect_drag_end(move |_, _, _| {
            let Some(r) = w.upgrade() else { return };
            if r.dragging.borrow_mut().take().is_some() {
                r.notify_change();
            }
        });
        r.area.add_controller(drag);

        // A click on nothing clears the selection.
        let click = gtk::GestureClick::new();
        let w = r.me.clone();
        click.connect_released(move |_, n, x, y| {
            let Some(r) = w.upgrade() else { return };
            if n == 1 && r.hit(x, y).is_none() && r.selected.borrow().is_some() {
                *r.selected.borrow_mut() = None;
                r.notify_select();
                r.area.queue_draw();
            }
        });
        r.area.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        let w = r.me.clone();
        motion.connect_motion(move |_, x, y| {
            if let Some(r) = w.upgrade() {
                r.pointer.set((x, y));
                r.area.set_cursor_from_name(Some(if r.hit(x, y).is_some() { "grab" } else { "default" }));
            }
        });
        r.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let w = r.me.clone();
        scroll.connect_scroll(move |_, _, dy| {
            let Some(r) = w.upgrade() else { return glib::Propagation::Proceed };
            let (px, py) = r.pointer.get();
            match r.hit(px, py) {
                Some(Pick::Speaker(id)) => {
                    // Turn the speaker under the pointer, in 5° steps.
                    if let Some(s) = r.layout.borrow_mut().speakers.iter_mut().find(|s| s.id == id) {
                        s.yaw = ((s.yaw + dy as f32 * 5.0) / 5.0).round() * 5.0;
                        s.yaw = (s.yaw + 360.0).rem_euclid(360.0);
                    }
                    *r.selected.borrow_mut() = Some(Pick::Speaker(id));
                    r.notify_select();
                    r.notify_change();
                }
                _ => {
                    let mut c = r.cam.get();
                    c.dist = (c.dist * (1.0 + dy as f32 * 0.08)).clamp(2.0, 25.0);
                    r.cam.set(c);
                }
            }
            r.area.queue_draw();
            glib::Propagation::Stop
        });
        r.area.add_controller(scroll);
        r
    }

    pub fn set_on_change(&self, f: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }
    pub fn set_on_select(&self, f: impl Fn() + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(f));
    }
    fn notify_change(&self) {
        if let Some(f) = self.on_change.borrow().as_ref() {
            f();
        }
    }
    fn notify_select(&self) {
        if let Some(f) = self.on_select.borrow().as_ref() {
            f();
        }
    }

    /// Re-aim the camera after the room is resized.
    pub fn recenter(&self) {
        let r = self.layout.borrow().room.clone();
        let mut c = self.cam.get();
        c.target = [r.width / 2.0, 0.6, r.depth / 2.0];
        c.dist = (r.width.max(r.depth) * 1.6).clamp(4.0, 20.0);
        self.cam.set(c);
        self.area.queue_draw();
    }

    fn view(&self) -> View {
        self.cam.get().view(self.area.width() as f32, self.area.height() as f32)
    }

    fn hit(&self, x: f64, y: f64) -> Option<Pick> {
        // Nearest first: the list is in draw order, so search it backwards.
        self.hits.borrow().iter().rev().find(|(_, hx, hy, r)| (x - hx).powi(2) + (y - hy).powi(2) <= r * r).map(|h| h.0.clone())
    }

    /// Slides an object across the horizontal plane at its own height.
    fn move_on_floor(&self, pick: &Pick, sx: f32, sy: f32) {
        let v = self.view();
        let (o, d) = v.ray(sx, sy);
        let mut l = self.layout.borrow_mut();
        let height = match pick {
            Pick::Speaker(id) => l.speakers.iter().find(|s| &s.id == id).map(|s| s.pos[1]),
            Pick::Spot(id) => l.spots.iter().find(|s| &s.id == id).map(|s| s.pos[1]),
        };
        let Some(hgt) = height else { return };
        if d[1].abs() < 1e-4 {
            return;
        }
        let t = (hgt - o[1]) / d[1];
        if t <= 0.0 {
            return;
        }
        let p = add(o, mul(d, t));
        let room = l.room.clone();
        let (x, z) = (p[0].clamp(0.1, room.width - 0.1), p[2].clamp(0.1, room.depth - 0.1));
        match pick {
            Pick::Speaker(id) => {
                if let Some(s) = l.speakers.iter_mut().find(|s| &s.id == id) {
                    s.pos[0] = x;
                    s.pos[2] = z;
                }
            }
            Pick::Spot(id) => {
                if let Some(s) = l.spots.iter_mut().find(|s| &s.id == id) {
                    s.pos[0] = x;
                    s.pos[2] = z;
                }
            }
        }
    }

    fn draw(&self, cr: &cairo::Context, w: f32, h: f32) {
        let v = self.cam.get().view(w, h);
        let l = self.layout.borrow();
        let room = &l.room;
        let mut hits = Vec::new();

        let line = |cr: &cairo::Context, a: V3, b: V3| {
            if let (Some(p), Some(q)) = (v.project(a), v.project(b)) {
                cr.move_to(p.0, p.1);
                cr.line_to(q.0, q.1);
            }
        };
        let poly = |cr: &cairo::Context, pts: &[V3]| -> bool {
            let proj: Vec<_> = pts.iter().filter_map(|p| v.project(*p)).collect();
            if proj.len() != pts.len() {
                return false;
            }
            cr.move_to(proj[0].0, proj[0].1);
            for p in &proj[1..] {
                cr.line_to(p.0, p.1);
            }
            cr.close_path();
            true
        };

        // Floor, with a soft grid every 50 cm.
        let (rw, rd, rh) = (room.width, room.depth, room.height);
        if poly(cr, &[[0.0, 0.0, 0.0], [rw, 0.0, 0.0], [rw, 0.0, rd], [0.0, 0.0, rd]]) {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.035);
            let _ = cr.fill();
        }
        cr.set_line_width(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
        let mut x = 0.5;
        while x < rw {
            line(cr, [x, 0.0, 0.0], [x, 0.0, rd]);
            x += 0.5;
        }
        let mut z = 0.5;
        while z < rd {
            line(cr, [0.0, 0.0, z], [rw, 0.0, z]);
            z += 0.5;
        }
        let _ = cr.stroke();

        // Walls the camera looks at from inside get a faint wash; all edges get a line.
        let walls: [([V3; 4], V3); 4] = [
            ([[0.0, 0.0, 0.0], [rw, 0.0, 0.0], [rw, rh, 0.0], [0.0, rh, 0.0]], [0.0, 0.0, 1.0]),
            ([[0.0, 0.0, rd], [rw, 0.0, rd], [rw, rh, rd], [0.0, rh, rd]], [0.0, 0.0, -1.0]),
            ([[0.0, 0.0, 0.0], [0.0, 0.0, rd], [0.0, rh, rd], [0.0, rh, 0.0]], [1.0, 0.0, 0.0]),
            ([[rw, 0.0, 0.0], [rw, 0.0, rd], [rw, rh, rd], [rw, rh, 0.0]], [-1.0, 0.0, 0.0]),
        ];
        for (quad, inward) in &walls {
            let c = mul(add(quad[0], quad[2]), 0.5);
            if dot(*inward, sub(v.eye, c)) > 0.0 {
                // Facing the camera from inside: it's a back wall, draw it.
                if poly(cr, quad) {
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.025);
                    let _ = cr.fill_preserve();
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.14);
                    let _ = cr.stroke();
                }
            }
        }

        let live = self.live.borrow();
        let selected = self.selected.borrow().clone();
        let active = l.active();

        // Shadows under everything, with a thin stand up to speakers off the floor.
        for s in &l.speakers {
            self.disc(cr, &v, [s.pos[0], 0.0, s.pos[2]], 0.16, (0.0, 0.0, 0.0), 0.55);
            if s.pos[1] > 0.15 {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
                cr.set_line_width(1.5);
                line(cr, [s.pos[0], 0.0, s.pos[2]], [s.pos[0], s.pos[1] - 0.09, s.pos[2]]);
                let _ = cr.stroke();
            }
        }

        // Sound rays from live speakers to the active spot.
        if let Some(spot) = active {
            for s in l.speakers.iter().filter(|s| live.contains(&s.id)) {
                if let (Some(a), Some(b)) = (v.project(s.pos), v.project(spot.pos)) {
                    let grad = cairo::LinearGradient::new(a.0, a.1, b.0, b.1);
                    grad.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.35);
                    grad.add_color_stop_rgba(1.0, ACCENT.0, ACCENT.1, ACCENT.2, 0.7);
                    cr.set_source(&grad).ok();
                    cr.set_line_width(1.6);
                    cr.set_dash(&[6.0, 5.0], 0.0);
                    cr.move_to(a.0, a.1);
                    cr.line_to(b.0, b.1);
                    let _ = cr.stroke();
                    cr.set_dash(&[], 0.0);
                    let d = (0..3).map(|i| (s.pos[i] - spot.pos[i]).powi(2)).sum::<f32>().sqrt();
                    self.label(cr, (a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0, &format!("{d:.1} m"), 0.55, 11.0);
                }
            }
        }

        // Objects back to front.
        enum Obj<'a> {
            Sp(&'a crate::room::Speaker),
            Sp2(&'a crate::room::Spot),
        }
        let mut objs: Vec<(f32, Obj)> = Vec::new();
        for s in &l.speakers {
            objs.push((v.project(s.pos).map(|p| p.2).unwrap_or(0.0), Obj::Sp(s)));
        }
        for s in &l.spots {
            objs.push((v.project(s.pos).map(|p| p.2).unwrap_or(0.0), Obj::Sp2(s)));
        }
        objs.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, o) in objs {
            match o {
                Obj::Sp(s) => {
                    let sel = selected == Some(Pick::Speaker(s.id.clone()));
                    let lit = live.contains(&s.id);
                    let (wid, hgt, dep) = s.size();
                    self.speaker_box(cr, &v, s.pos, s.yaw, (wid, hgt, dep), s.is_bluetooth(), lit, sel);
                    // Facing arrow on the floor.
                    let f = facing(s.yaw);
                    let base = [s.pos[0], 0.01, s.pos[2]];
                    let tip = add(base, [f[0] * 0.45, 0.0, f[1] * 0.45]);
                    let c = if s.is_bluetooth() { BT_BLUE } else { (1.0, 1.0, 1.0) };
                    cr.set_source_rgba(c.0, c.1, c.2, if lit { 0.7 } else { 0.3 });
                    cr.set_line_width(2.0);
                    line(cr, base, tip);
                    let _ = cr.stroke();
                    let side = [-f[1], f[0]];
                    let back = add(base, [f[0] * 0.33, 0.0, f[1] * 0.33]);
                    if poly(cr, &[tip, add(back, [side[0] * 0.07, 0.0, side[1] * 0.07]), add(back, [-side[0] * 0.07, 0.0, -side[1] * 0.07])]) {
                        let _ = cr.fill();
                    }
                    if let Some(p) = v.project(add(s.pos, [0.0, hgt / 2.0 + 0.12, 0.0])) {
                        self.label(cr, p.0, p.1, &s.name, if lit { 0.95 } else { 0.5 }, 12.0);
                    }
                    if let Some(p) = v.project(s.pos) {
                        hits.push((Pick::Speaker(s.id.clone()), p.0, p.1, (v.focal * 0.16 / p.2).max(14.0) as f64));
                    }
                }
                Obj::Sp2(s) => {
                    let is_active = active.map(|a| a.id == s.id).unwrap_or(false);
                    let sel = selected == Some(Pick::Spot(s.id.clone()));
                    let (c, a) = if is_active { (ACCENT, 1.0) } else { ((1.0, 1.0, 1.0), 0.45) };
                    // Ring on the floor, a stem, and a head at ear height.
                    self.ring(cr, &v, [s.pos[0], 0.005, s.pos[2]], 0.32, c, a * 0.8);
                    self.disc(cr, &v, [s.pos[0], 0.004, s.pos[2]], 0.32, c, a * 0.10);
                    cr.set_source_rgba(c.0, c.1, c.2, a * 0.5);
                    cr.set_dash(&[3.0, 3.0], 0.0);
                    cr.set_line_width(1.2);
                    line(cr, [s.pos[0], 0.0, s.pos[2]], s.pos);
                    let _ = cr.stroke();
                    cr.set_dash(&[], 0.0);
                    if let Some(p) = v.project(s.pos) {
                        let r = (v.focal * 0.09 / p.2) as f64;
                        if is_active || sel {
                            let glow = cairo::RadialGradient::new(p.0, p.1, 0.0, p.0, p.1, r * 3.0);
                            glow.add_color_stop_rgba(0.0, c.0, c.1, c.2, 0.45);
                            glow.add_color_stop_rgba(1.0, c.0, c.1, c.2, 0.0);
                            cr.set_source(&glow).ok();
                            cr.arc(p.0, p.1, r * 3.0, 0.0, 2.0 * std::f64::consts::PI);
                            let _ = cr.fill();
                        }
                        cr.set_source_rgba(c.0, c.1, c.2, a);
                        cr.arc(p.0, p.1, r, 0.0, 2.0 * std::f64::consts::PI);
                        let _ = cr.fill();
                        if sel {
                            cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                            cr.set_line_width(2.0);
                            cr.arc(p.0, p.1, r + 4.0, 0.0, 2.0 * std::f64::consts::PI);
                            let _ = cr.stroke();
                        }
                        self.label(cr, p.0, p.1 - r - 10.0, &s.name, a.max(0.6), 12.0);
                        hits.push((Pick::Spot(s.id.clone()), p.0, p.1, (r * 1.8).max(14.0)));
                    }
                }
            }
        }
        *self.hits.borrow_mut() = hits;
    }

    fn label(&self, cr: &cairo::Context, x: f64, y: f64, text: &str, alpha: f64, size: f64) {
        cr.select_font_face("SF Pro Text", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(size);
        let Ok(ext) = cr.text_extents(text) else { return };
        let (tx, ty) = (x - ext.width() / 2.0 - ext.x_bearing(), y);
        // Dark halo keeps labels readable over the grid and rays.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.75 * alpha);
        for (ox, oy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
            cr.move_to(tx + ox, ty + oy);
            let _ = cr.show_text(text);
        }
        cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
        cr.move_to(tx, ty);
        let _ = cr.show_text(text);
    }

    fn circle_pts(c: V3, r: f32) -> Vec<V3> {
        (0..32).map(|i| {
            let a = i as f32 / 32.0 * 2.0 * PI;
            [c[0] + r * a.cos(), c[1], c[2] + r * a.sin()]
        })
        .collect()
    }

    fn disc(&self, cr: &cairo::Context, v: &View, c: V3, r: f32, col: (f64, f64, f64), a: f64) {
        let pts: Vec<_> = Self::circle_pts(c, r).into_iter().filter_map(|p| v.project(p)).collect();
        if pts.len() < 32 {
            return;
        }
        cr.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            cr.line_to(p.0, p.1);
        }
        cr.close_path();
        cr.set_source_rgba(col.0, col.1, col.2, a);
        let _ = cr.fill();
    }

    fn ring(&self, cr: &cairo::Context, v: &View, c: V3, r: f32, col: (f64, f64, f64), a: f64) {
        let pts: Vec<_> = Self::circle_pts(c, r).into_iter().filter_map(|p| v.project(p)).collect();
        if pts.len() < 32 {
            return;
        }
        cr.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            cr.line_to(p.0, p.1);
        }
        cr.close_path();
        cr.set_source_rgba(col.0, col.1, col.2, a);
        cr.set_line_width(1.6);
        let _ = cr.stroke();
    }

    /// A shaded box for a speaker; the front face is darker, like a grille.
    #[allow(clippy::too_many_arguments)]
    fn speaker_box(&self, cr: &cairo::Context, v: &View, c: V3, yaw: f32, (w, h, d): (f32, f32, f32), bt: bool, lit: bool, sel: bool) {
        let f = facing(yaw);
        let fwd = [f[0], 0.0, f[1]];
        let side = [-f[1], 0.0, f[0]];
        let up = [0.0, 1.0, 0.0];
        let corner = |sx: f32, sy: f32, sz: f32| add(c, add(mul(side, sx * w / 2.0), add(mul(up, sy * h / 2.0), mul(fwd, sz * d / 2.0))));
        let faces: [([V3; 4], V3, bool); 6] = [
            ([corner(-1., -1., 1.), corner(1., -1., 1.), corner(1., 1., 1.), corner(-1., 1., 1.)], fwd, true),
            ([corner(-1., -1., -1.), corner(1., -1., -1.), corner(1., 1., -1.), corner(-1., 1., -1.)], mul(fwd, -1.0), false),
            ([corner(1., -1., -1.), corner(1., -1., 1.), corner(1., 1., 1.), corner(1., 1., -1.)], side, false),
            ([corner(-1., -1., -1.), corner(-1., -1., 1.), corner(-1., 1., 1.), corner(-1., 1., -1.)], mul(side, -1.0), false),
            ([corner(-1., 1., -1.), corner(1., 1., -1.), corner(1., 1., 1.), corner(-1., 1., 1.)], up, false),
            ([corner(-1., -1., -1.), corner(1., -1., -1.), corner(1., -1., 1.), corner(-1., -1., 1.)], mul(up, -1.0), false),
        ];
        let base = if bt { BT_BLUE } else { (0.93, 0.93, 0.95) };
        let light = norm([0.4, 0.9, 0.3]);
        let mut visible: Vec<_> = faces
            .iter()
            .filter(|(q, n, _)| dot(*n, sub(v.eye, mul(add(q[0], q[2]), 0.5))) > 0.0)
            .collect();
        visible.sort_by(|a, b| {
            let da = dot(sub(mul(add(a.0[0], a.0[2]), 0.5), v.eye), v.fwd);
            let db = dot(sub(mul(add(b.0[0], b.0[2]), 0.5), v.eye), v.fwd);
            db.total_cmp(&da)
        });
        for (q, n, front) in visible {
            let pts: Vec<_> = q.iter().filter_map(|p| v.project(*p)).collect();
            if pts.len() != 4 {
                continue;
            }
            cr.move_to(pts[0].0, pts[0].1);
            for p in &pts[1..] {
                cr.line_to(p.0, p.1);
            }
            cr.close_path();
            let shade = (0.45 + 0.55 * dot(*n, light).max(0.0)) as f64 * if lit { 1.0 } else { 0.55 };
            let k = if *front { 0.35 } else { 1.0 };
            cr.set_source_rgba(base.0 * shade * k, base.1 * shade * k, base.2 * shade * k, 1.0);
            let _ = cr.fill_preserve();
            if sel {
                cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 1.0);
                cr.set_line_width(2.0);
            } else {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
                cr.set_line_width(1.0);
            }
            let _ = cr.stroke();
        }
    }
}
