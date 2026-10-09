//! Apple-style "liquid glass" for menus and dialogs.
//!
//! GTK has no backdrop-filter, so each surface carries its own backdrop: when
//! it opens, the app behind it is rendered to a texture once, and on the CPU
//! that crop is blurred, bent inward near the rim like a thick lens, and split
//! into slightly different bends per colour channel (blue most, red least)
//! for the prism fringe. A Fresnel brightening at the rim and a CSS specular
//! edge on top finish it. The image is static while the surface is open, which
//! keeps it cheap: nothing is re-rendered per frame.

use adw::prelude::*;
use gtk::{gdk, glib, graphene};
use std::cell::Cell;
use std::time::Duration;

/// Retries `f` every frame-ish until it succeeds: right as a surface opens,
/// GTK is mid-redraw and capturing the window comes back empty.
fn until_painted(delay_ms: u64, f: impl Fn() -> bool + 'static) {
    let tries = Cell::new(0);
    glib::timeout_add_local(Duration::from_millis(delay_ms), move || {
        tries.set(tries.get() + 1);
        if f() || tries.get() > 30 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
    });
}

/// Corner radius of every glass surface; keep in sync with style.css.
pub const RADIUS: f32 = 22.0;
/// How far the refraction reaches in from the rim, in pixels.
const BAND: f32 = 26.0;
/// Peak displacement at the rim, in pixels.
const BEND: f32 = 16.0;
const BLUR: usize = 7;
const MARGIN: i32 = 40;

/// Wraps the popover's child in a glass surface. Call after `set_child`.
pub fn popover(pop: &gtk::Popover) {
    pop.add_css_class("liquid");
    pop.set_has_arrow(false);
    let Some(content) = pop.child() else { return };
    pop.set_child(None::<&gtk::Widget>);
    let (surface, pic) = wrap(&content);
    pop.set_child(Some(&surface));
    pop.connect_map(move |pop| {
        // The popup's position is only final once the compositor has placed it.
        let (pop, surface, pic) = (pop.clone(), surface.clone(), pic.clone());
        until_painted(16, move || !pop.is_visible() || render_for_popover(&pop, &surface, &pic));
    });
}

/// Same for an AdwDialog. Call after the dialog has its child.
pub fn dialog(dialog: &adw::Dialog) {
    dialog.add_css_class("liquid");
    let Some(content) = dialog.child() else { return };
    dialog.set_child(None::<&gtk::Widget>);
    let (surface, pic) = wrap(&content);
    dialog.set_child(Some(&surface));
    surface.connect_map(move |surface| {
        // Wait out the open animation so the bounds are where the sheet lands.
        let (surface, pic) = (surface.clone(), pic.clone());
        until_painted(260, move || {
            let Some(win) = surface.root().and_downcast::<gtk::Window>() else { return true };
            let Some(b) = surface.compute_bounds(&win) else { return true };
            !surface.is_mapped() || paint(&win, &pic, b.x(), b.y(), surface.width(), surface.height())
        });
    });
}

/// backdrop picture underneath, specular rim above it, the real content on top.
fn wrap(content: &gtk::Widget) -> (gtk::Overlay, gtk::Picture) {
    let pic = gtk::Picture::new();
    pic.set_content_fit(gtk::ContentFit::Fill);
    pic.set_can_shrink(true);
    pic.set_can_target(false);
    let rim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rim.add_css_class("liquid-rim");
    rim.set_can_target(false);
    let surface = gtk::Overlay::new();
    surface.add_css_class("liquid-surface");
    surface.set_overflow(gtk::Overflow::Hidden);
    surface.set_child(Some(&pic));
    surface.add_overlay(&rim);
    surface.add_overlay(content);
    surface.set_measure_overlay(content, true);
    content.add_css_class("liquid-content");
    (surface, pic)
}

fn render_for_popover(pop: &gtk::Popover, surface: &gtk::Overlay, pic: &gtk::Picture) -> bool {
    let Some(win) = pop.root().and_downcast::<gtk::Window>() else { return false };
    let Some(popup) = pop.surface().and_downcast::<gdk::Popup>() else { return false };
    let Some(b) = surface.compute_bounds(pop) else { return false };
    // Popup position is relative to the window's surface; both natives have
    // their own offset (CSD shadow) between surface and widget origin.
    let (ptx, pty) = pop.surface_transform();
    let (wtx, wty) = win.surface_transform();
    let x = popup.position_x() as f32 + ptx as f32 + b.x() - wtx as f32;
    let y = popup.position_y() as f32 + pty as f32 + b.y() - wty as f32;
    paint(&win, pic, x, y, surface.width(), surface.height())
}

/// Renders what the window shows under (x, y, w, h) and turns it into glass.
/// Returns false when the capture came back empty and is worth retrying.
fn paint(win: &gtk::Window, pic: &gtk::Picture, x: f32, y: f32, w: i32, h: i32) -> bool {
    if w <= 0 || h <= 0 {
        return true;
    }
    // The window's content excludes popovers and dialogs, so the glass never sees itself.
    let content = match win.downcast_ref::<adw::ApplicationWindow>().and_then(|a| a.content()) {
        Some(c) => c,
        None => match win.child() {
            Some(c) => c,
            None => return true,
        },
    };
    let Some(origin) = content.compute_point(win, &graphene::Point::new(0.0, 0.0)) else { return true };
    let (cw, ch) = (content.width(), content.height());
    let Some(renderer) = win.renderer() else { return true };
    let Some(parent) = content.parent() else { return true };
    // Draw the content the way its parent does for a frame. (A WidgetPaintable
    // only refreshes when its widget is invalidated, so it often comes back empty.)
    let snap = gtk::Snapshot::new();
    parent.snapshot_child(&content, &snap);
    let Some(node) = snap.to_node() else { return false };
    let tex = renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, cw as f32, ch as f32)));
    let (tw, th) = (tex.width() as usize, tex.height() as usize);
    let mut src = vec![0u8; tw * th * 4];
    tex.download(&mut src, tw * 4);

    let (cx, cy) = ((x - origin.x()).round() as i32, (y - origin.y()).round() as i32);
    let out = liquid(&src, tw, th, cx, cy, w as usize, h as usize);
    let tex = gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::B8g8r8a8Premultiplied, &glib::Bytes::from_owned(out), w as usize * 4);
    pic.set_paintable(Some(&tex));
    true
}

/// Crop (with margin) → blur → per-channel rim refraction → tone.
fn liquid(src: &[u8], sw: usize, sh: usize, x: i32, y: i32, w: usize, h: usize) -> Vec<u8> {
    let (bw, bh) = (w + 2 * MARGIN as usize, h + 2 * MARGIN as usize);
    // Outside the window counts as the app's own black.
    let mut img = vec![[0f32; 4]; bw * bh];
    for j in 0..bh {
        for i in 0..bw {
            let (sx, sy) = (x - MARGIN + i as i32, y - MARGIN + j as i32);
            if sx >= 0 && sy >= 0 && (sx as usize) < sw && (sy as usize) < sh {
                let o = (sy as usize * sw + sx as usize) * 4;
                img[j * bw + i] = [src[o] as f32, src[o + 1] as f32, src[o + 2] as f32, 255.0];
            } else {
                img[j * bw + i] = [0.0, 0.0, 0.0, 255.0];
            }
        }
    }
    for _ in 0..3 {
        box_blur(&mut img, bw, bh, BLUR);
    }

    let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
    let r = RADIUS.min(hw).min(hh);
    let sdf = |px: f32, py: f32| -> f32 {
        // Signed distance to the rounded rect's edge, positive inside.
        let qx = (px - hw).abs() - (hw - r);
        let qy = (py - hh).abs() - (hh - r);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
        -outside
    };
    // Prism: blue bends most, red least. Index order is B, G, R.
    let disperse = [1.35f32, 1.0, 0.68];
    let mut out = vec![0u8; w * h * 4];
    for oy in 0..h {
        for ox in 0..w {
            let (px, py) = (ox as f32 + 0.5, oy as f32 + 0.5);
            let d = sdf(px, py);
            if d < -0.5 {
                continue;
            }
            // Inward normal from the SDF gradient.
            let (gx, gy) = (sdf(px + 1.0, py) - sdf(px - 1.0, py), sdf(px, py + 1.0) - sdf(px, py - 1.0));
            let len = (gx * gx + gy * gy).sqrt().max(1e-4);
            let (nx, ny) = (gx / len, gy / len);
            let t = (1.0 - d / BAND).clamp(0.0, 1.0);
            let bend = t * t * BEND;
            let o = (oy * w + ox) * 4;
            for c in 0..3 {
                // Sample from further out: the rim pulls the surroundings inward.
                let s = bend * disperse[c];
                let sx = px - nx * s + MARGIN as f32;
                let sy = py - ny * s + MARGIN as f32;
                let v = bilinear(&img, bw, bh, sx, sy, c);
                // Darken for legible text, plus a Fresnel lift toward the rim.
                let lit = v * 0.62 + 255.0 * (0.05 + 0.16 * t.powi(3));
                out[o + c] = lit.clamp(0.0, 255.0) as u8;
            }
            out[o + 3] = 255;
        }
    }
    out
}

fn box_blur(img: &mut [[f32; 4]], w: usize, h: usize, r: usize) {
    let mut tmp = vec![[0f32; 4]; w * h];
    let n = (2 * r + 1) as f32;
    for y in 0..h {
        let row = &img[y * w..(y + 1) * w];
        let mut acc = [0f32; 4];
        for k in 0..=2 * r {
            let p = row[k.saturating_sub(r).min(w - 1)];
            for c in 0..4 {
                acc[c] += p[c];
            }
        }
        for x in 0..w {
            for c in 0..4 {
                tmp[y * w + x][c] = acc[c] / n;
            }
            let add = row[(x + r + 1).min(w - 1)];
            let sub = row[x.saturating_sub(r)];
            for c in 0..4 {
                acc[c] += add[c] - sub[c];
            }
        }
    }
    for x in 0..w {
        let mut acc = [0f32; 4];
        for k in 0..=2 * r {
            let p = tmp[k.saturating_sub(r).min(h - 1) * w + x];
            for c in 0..4 {
                acc[c] += p[c];
            }
        }
        for y in 0..h {
            for c in 0..4 {
                img[y * w + x][c] = acc[c] / n;
            }
            let add = tmp[(y + r + 1).min(h - 1) * w + x];
            let sub = tmp[y.saturating_sub(r) * w + x];
            for c in 0..4 {
                acc[c] += add[c] - sub[c];
            }
        }
    }
}

fn bilinear(img: &[[f32; 4]], w: usize, h: usize, x: f32, y: f32, c: usize) -> f32 {
    let x = (x - 0.5).clamp(0.0, (w - 1) as f32);
    let y = (y - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let top = img[y0 * w + x0][c] * (1.0 - fx) + img[y0 * w + x1][c] * fx;
    let bot = img[y1 * w + x0][c] * (1.0 - fx) + img[y1 * w + x1][c] * fx;
    top * (1.0 - fy) + bot * fy
}
