//! The Lucide icons, the service badges and the demo's covers, drawn with cairo.
//!
//! The icons are stroked from their embedded SVG in the colour CSS gives the widget, so they
//! need no SVG loader at run time (GTK 4.8 has none of its own; gdk-pixbuf's is optional) and
//! follow hover and state colours like text does. Only the SVG the Lucide set uses is read:
//! `path` (M L H V C A Z, absolute and relative), `circle`, `line`, `rect`, `polyline` and
//! `polygon`, stroked 2 units wide with round caps and joins on a 24-unit square.

use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::PI;

use gtk::prelude::*;
use gtk::{cairo, gdk, glib};

use super::theme;
use crate::vm::Service;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    SkipBack,
    SkipForward,
    Search,
    ListMusic,
    Volume,
    Plus,
    X,
    Music,
    Check,
    Home,
    Library,
    ChevronLeft,
    ChevronRight,
    Settings,
    LogIn,
    LogOut,
    Copy,
    ExternalLink,
    BookOpen,
    ClipboardPaste,
}

impl Icon {
    #[cfg(test)]
    pub const ALL: [Icon; 22] = [
        Icon::Play,
        Icon::Pause,
        Icon::SkipBack,
        Icon::SkipForward,
        Icon::Search,
        Icon::ListMusic,
        Icon::Volume,
        Icon::Plus,
        Icon::X,
        Icon::Music,
        Icon::Check,
        Icon::Home,
        Icon::Library,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::Settings,
        Icon::LogIn,
        Icon::LogOut,
        Icon::Copy,
        Icon::ExternalLink,
        Icon::BookOpen,
        Icon::ClipboardPaste,
    ];

    fn svg(self) -> &'static str {
        match self {
            Icon::Play => include_str!("../../assets/icons/play.svg"),
            Icon::Pause => include_str!("../../assets/icons/pause.svg"),
            Icon::SkipBack => include_str!("../../assets/icons/skip-back.svg"),
            Icon::SkipForward => include_str!("../../assets/icons/skip-forward.svg"),
            Icon::Search => include_str!("../../assets/icons/search.svg"),
            Icon::ListMusic => include_str!("../../assets/icons/list-music.svg"),
            Icon::Volume => include_str!("../../assets/icons/volume-2.svg"),
            Icon::Plus => include_str!("../../assets/icons/plus.svg"),
            Icon::X => include_str!("../../assets/icons/x.svg"),
            Icon::Music => include_str!("../../assets/icons/music.svg"),
            Icon::Check => include_str!("../../assets/icons/check.svg"),
            Icon::Home => include_str!("../../assets/icons/house.svg"),
            Icon::Library => include_str!("../../assets/icons/library.svg"),
            Icon::ChevronLeft => include_str!("../../assets/icons/chevron-left.svg"),
            Icon::ChevronRight => include_str!("../../assets/icons/chevron-right.svg"),
            Icon::Settings => include_str!("../../assets/icons/settings.svg"),
            Icon::LogIn => include_str!("../../assets/icons/log-in.svg"),
            Icon::LogOut => include_str!("../../assets/icons/log-out.svg"),
            Icon::Copy => include_str!("../../assets/icons/copy.svg"),
            Icon::ExternalLink => include_str!("../../assets/icons/external-link.svg"),
            Icon::BookOpen => include_str!("../../assets/icons/book-open.svg"),
            Icon::ClipboardPaste => include_str!("../../assets/icons/clipboard-paste.svg"),
        }
    }

    /// The icon's drawing, parsed once per thread.
    fn ops(self) -> Vec<Op> {
        thread_local! {
            static OPS: RefCell<HashMap<Icon, Vec<Op>>> = RefCell::new(HashMap::new());
        }
        OPS.with_borrow_mut(|ops| ops.entry(self).or_insert_with(|| parse(self.svg())).clone())
    }

    /// Strokes the icon into a `size`-unit square at the context's origin, in its source colour.
    pub fn draw(self, cr: &cairo::Context, size: f64) {
        cr.save().ok();
        cr.scale(size / 24.0, size / 24.0);
        cr.set_line_width(2.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        for op in self.ops() {
            op.trace(cr);
            cr.stroke().ok();
        }
        cr.restore().ok();
    }
}

/// An icon `size` pixels square in the CSS colour of its widget (class `icon`).
pub fn icon(icon: Icon, size: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(size);
    area.set_content_height(size);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    area.set_can_target(false);
    area.add_css_class("icon");
    let shown = std::rc::Rc::new(std::cell::Cell::new(icon));
    set_icon_draw(&area, &shown, size);
    // SAFETY: the key holds only this type; `swap` reads it back as the same type.
    unsafe { area.set_data("requiem-icon", shown) };
    area
}

fn set_icon_draw(area: &gtk::DrawingArea, shown: &std::rc::Rc<std::cell::Cell<Icon>>, size: i32) {
    let shown = shown.clone();
    area.set_draw_func(move |area, cr, w, h| {
        #[allow(deprecated)]
        let c = area.style_context().color();
        cr.set_source_rgba(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
            f64::from(c.alpha()),
        );
        let s = f64::from(size);
        cr.translate((f64::from(w) - s) / 2.0, (f64::from(h) - s) / 2.0);
        shown.get().draw(cr, s);
    });
}

/// Shows `icon` in an area `icon()` made.
pub fn swap(area: &gtk::DrawingArea, icon: Icon) {
    // SAFETY: `icon()` stored this type under this key.
    let shown = unsafe { area.data::<std::rc::Rc<std::cell::Cell<Icon>>>("requiem-icon") };
    if let Some(shown) = shown {
        // SAFETY: the pointer is to the value `icon()` stored, which lives as long as `area`.
        let shown = unsafe { shown.as_ref() };
        if shown.get() != icon {
            shown.set(icon);
            area.queue_draw();
        }
    }
}

/// An icon and a label side by side, for a button's child.
pub fn labelled(icon_: Option<Icon>, text: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.set_halign(gtk::Align::Center);
    if let Some(i) = icon_ {
        b.append(&icon(i, 16));
    }
    b.append(&gtk::Label::new(Some(text)));
    b
}

/// A button with an icon and a label, and `classes`.
pub fn button(icon_: Option<Icon>, text: &str, classes: &[&str]) -> gtk::Button {
    let b = gtk::Button::new();
    b.set_child(Some(&labelled(icon_, text)));
    theme::classes(&b, classes);
    b
}

/// A button showing only `icon_`, named `name` for screen readers.
pub fn icon_button(icon_: Icon, size: i32, name: &str, classes: &[&str]) -> gtk::Button {
    let b = gtk::Button::new();
    b.set_child(Some(&icon(icon_, size)));
    b.set_tooltip_text(Some(name));
    b.update_property(&[gtk::accessible::Property::Label(name)]);
    theme::classes(&b, classes);
    b
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Path(Vec<Seg>),
    Circle {
        cx: f64,
        cy: f64,
        r: f64,
    },
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        r: f64,
    },
    Poly {
        points: Vec<(f64, f64)>,
        closed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Arc {
        from: (f64, f64),
        rx: f64,
        ry: f64,
        rot: f64,
        large: bool,
        sweep: bool,
        to: (f64, f64),
    },
    Close,
}

impl Op {
    fn trace(&self, cr: &cairo::Context) {
        cr.new_path();
        match self {
            Op::Path(segs) => {
                for s in segs {
                    match *s {
                        Seg::Move(x, y) => cr.move_to(x, y),
                        Seg::Line(x, y) => cr.line_to(x, y),
                        Seg::Cubic(a, b, c, d, e, f) => cr.curve_to(a, b, c, d, e, f),
                        Seg::Arc {
                            from,
                            rx,
                            ry,
                            rot,
                            large,
                            sweep,
                            to,
                        } => arc_to(cr, from, rx, ry, rot, large, sweep, to),
                        Seg::Close => cr.close_path(),
                    }
                }
            }
            Op::Circle { cx, cy, r } => {
                cr.new_sub_path();
                cr.arc(*cx, *cy, *r, 0.0, 2.0 * PI);
            }
            Op::Rect { x, y, w, h, r } => rounded(cr, *x, *y, *w, *h, *r),
            Op::Poly { points, closed } => {
                for (i, &(x, y)) in points.iter().enumerate() {
                    if i == 0 {
                        cr.move_to(x, y);
                    } else {
                        cr.line_to(x, y);
                    }
                }
                if *closed {
                    cr.close_path();
                }
            }
        }
    }
}

/// A rounded rectangle path.
pub fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    if r <= 0.0 {
        cr.rectangle(x, y, w, h);
        return;
    }
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// SVG's elliptical arc from `from` to `to`, by the spec's endpoint-to-centre conversion.
#[allow(clippy::too_many_arguments)]
fn arc_to(
    cr: &cairo::Context,
    from: (f64, f64),
    rx: f64,
    ry: f64,
    rot: f64,
    large: bool,
    sweep: bool,
    to: (f64, f64),
) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 || from == to {
        cr.line_to(to.0, to.1);
        return;
    }
    let phi = rot.to_radians();
    let (sin, cos) = phi.sin_cos();
    let dx = (from.0 - to.0) / 2.0;
    let dy = (from.1 - to.1) / 2.0;
    let x1 = cos * dx + sin * dy;
    let y1 = -sin * dx + cos * dy;
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut coef = (num / den).max(0.0).sqrt();
    if large == sweep {
        coef = -coef;
    }
    let cx1 = coef * rx * y1 / ry;
    let cy1 = -coef * ry * x1 / rx;
    let cx = cos * cx1 - sin * cy1 + (from.0 + to.0) / 2.0;
    let cy = sin * cx1 + cos * cy1 + (from.1 + to.1) / 2.0;
    let angle = |ux: f64, uy: f64| uy.atan2(ux);
    let t1 = angle((x1 - cx1) / rx, (y1 - cy1) / ry);
    let t2 = angle((-x1 - cx1) / rx, (-y1 - cy1) / ry);
    let mut dt = t2 - t1;
    if sweep && dt < 0.0 {
        dt += 2.0 * PI;
    } else if !sweep && dt > 0.0 {
        dt -= 2.0 * PI;
    }
    cr.save().ok();
    cr.translate(cx, cy);
    cr.rotate(phi);
    cr.scale(rx, ry);
    if dt > 0.0 {
        cr.arc(0.0, 0.0, 1.0, t1, t1 + dt);
    } else {
        cr.arc_negative(0.0, 0.0, 1.0, t1, t1 + dt);
    }
    cr.restore().ok();
}

/// The drawing of an SVG's shapes.
fn parse(svg: &str) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut rest = svg;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[end..];
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let num = |key: &str| attr(tag, key).and_then(|v| v.trim().parse::<f64>().ok());
        let op = match name.as_str() {
            "path" => attr(tag, "d").map(|d| Op::Path(path(d))),
            "circle" => Some(Op::Circle {
                cx: num("cx").unwrap_or(0.0),
                cy: num("cy").unwrap_or(0.0),
                r: num("r").unwrap_or(0.0),
            }),
            "rect" => Some(Op::Rect {
                x: num("x").unwrap_or(0.0),
                y: num("y").unwrap_or(0.0),
                w: num("width").unwrap_or(0.0),
                h: num("height").unwrap_or(0.0),
                r: num("rx").or_else(|| num("ry")).unwrap_or(0.0),
            }),
            "line" => Some(Op::Poly {
                points: vec![
                    (num("x1").unwrap_or(0.0), num("y1").unwrap_or(0.0)),
                    (num("x2").unwrap_or(0.0), num("y2").unwrap_or(0.0)),
                ],
                closed: false,
            }),
            "polyline" | "polygon" => attr(tag, "points").map(|p| {
                let n = numbers(p);
                Op::Poly {
                    points: n.chunks_exact(2).map(|c| (c[0], c[1])).collect(),
                    closed: name == "polygon",
                }
            }),
            _ => None,
        };
        ops.extend(op);
    }
    ops
}

/// The value of attribute `key` in a tag's text.
fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(key) {
        let at = from + i;
        let before = tag[..at].chars().next_back();
        let after = &tag[at + key.len()..];
        if before.is_some_and(char::is_whitespace)
            && let Some(v) = after.trim_start().strip_prefix('=')
        {
            let v = v.trim_start().strip_prefix('"')?;
            return v.find('"').map(|e| &v[..e]);
        }
        from = at + key.len();
    }
    None
}

/// The numbers in an SVG list, however they are separated: "1-2.5.5,3" is 1, -2.5, .5, 3.
fn numbers(s: &str) -> Vec<f64> {
    let mut lexer = Lexer {
        s: s.as_bytes(),
        i: 0,
    };
    let mut out = Vec::new();
    while let Some(n) = lexer.number() {
        out.push(n);
    }
    out
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',')
        {
            self.i += 1;
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip();
        let c = *self.s.get(self.i)?;
        if c.is_ascii_alphabetic() {
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<f64> {
        self.skip();
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
            i += 1;
        }
        let mut dot = false;
        let mut digits = false;
        while i < s.len() {
            match s[i] {
                b'0'..=b'9' => digits = true,
                b'.' if !dot => dot = true,
                b'e' | b'E' if digits => {
                    i += 1;
                    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
                        i += 1;
                    }
                    continue;
                }
                _ => break,
            }
            i += 1;
        }
        if !digits {
            return None;
        }
        self.i = i;
        std::str::from_utf8(&s[start..i]).ok()?.parse().ok()
    }

    /// An arc's flag: a lone 0 or 1, which may run into the next number.
    fn flag(&mut self) -> Option<bool> {
        self.skip();
        let c = *self.s.get(self.i)?;
        self.i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    }

    fn at_number(&mut self) -> bool {
        self.skip();
        self.s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }
}

fn path(d: &str) -> Vec<Seg> {
    let mut lx = Lexer {
        s: d.as_bytes(),
        i: 0,
    };
    let mut segs = Vec::new();
    let (mut x, mut y) = (0.0, 0.0);
    let (mut sx, mut sy) = (0.0, 0.0);
    let mut cmd = b'M';
    loop {
        if let Some(c) = lx.command() {
            cmd = c;
        } else if !lx.at_number() {
            break;
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (Some(a), Some(b)) = (lx.number(), lx.number()) else {
                    break;
                };
                (x, y) = (ox + a, oy + b);
                (sx, sy) = (x, y);
                segs.push(Seg::Move(x, y));
                // Further pairs after a move are lines.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' => {
                let (Some(a), Some(b)) = (lx.number(), lx.number()) else {
                    break;
                };
                (x, y) = (ox + a, oy + b);
                segs.push(Seg::Line(x, y));
            }
            b'H' => {
                let Some(a) = lx.number() else { break };
                x = ox + a;
                segs.push(Seg::Line(x, y));
            }
            b'V' => {
                let Some(b) = lx.number() else { break };
                y = oy + b;
                segs.push(Seg::Line(x, y));
            }
            b'C' => {
                let n: Vec<f64> = (0..6).filter_map(|_| lx.number()).collect();
                if n.len() < 6 {
                    break;
                }
                segs.push(Seg::Cubic(
                    ox + n[0],
                    oy + n[1],
                    ox + n[2],
                    oy + n[3],
                    ox + n[4],
                    oy + n[5],
                ));
                (x, y) = (ox + n[4], oy + n[5]);
            }
            b'A' => {
                let (Some(rx), Some(ry), Some(rot)) = (lx.number(), lx.number(), lx.number())
                else {
                    break;
                };
                let (Some(large), Some(sweep)) = (lx.flag(), lx.flag()) else {
                    break;
                };
                let (Some(a), Some(b)) = (lx.number(), lx.number()) else {
                    break;
                };
                let to = (ox + a, oy + b);
                segs.push(Seg::Arc {
                    from: (x, y),
                    rx,
                    ry,
                    rot,
                    large,
                    sweep,
                    to,
                });
                (x, y) = to;
            }
            b'Z' => {
                segs.push(Seg::Close);
                (x, y) = (sx, sy);
                // Numbers straight after Z have no command; read them as lines.
                cmd = if rel { b'l' } else { b'L' };
            }
            _ => break,
        }
    }
    segs
}

/// The badge's ring, in the window's background colour, that sets the disc off busy artwork.
const RING: f64 = 1.5;

/// A badge's side on a cover `art` pixels square: 14 on a table row's thumbnail, growing to
/// 20 on a 128 card and no larger.
pub fn badge_side(art: f64) -> i32 {
    (12.0 + art / 16.0).round().clamp(14.0, 20.0) as i32
}

/// A service badge `side` pixels square, hidden while it has no service.
pub struct Badge {
    pub area: gtk::DrawingArea,
    service: std::rc::Rc<std::cell::Cell<Option<Service>>>,
}

impl Badge {
    pub fn new(side: i32, ring: bool) -> Badge {
        let area = gtk::DrawingArea::new();
        area.set_content_width(side);
        area.set_content_height(side);
        area.set_can_target(false);
        area.set_halign(gtk::Align::End);
        area.set_valign(gtk::Align::End);
        area.set_visible(false);
        let service = std::rc::Rc::new(std::cell::Cell::new(None));
        let shown = service.clone();
        area.set_draw_func(move |_, cr, w, _| {
            if let Some(s) = shown.get() {
                draw_badge(cr, s, 0.0, 0.0, f64::from(w), ring);
            }
        });
        Badge { area, service }
    }

    pub fn set(&self, service: Option<Service>) {
        if self.service.replace(service) != service {
            self.area.queue_draw();
        }
        self.area.set_visible(service.is_some());
    }
}

/// `service`'s disc `d` across at (`x`, `y`), with or without its ring.
pub fn draw_badge(cr: &cairo::Context, service: Service, x: f64, y: f64, d: f64, ring: bool) {
    let inset = if ring {
        let (r, g, b) = theme::rgb(theme::BG);
        cr.set_source_rgb(r, g, b);
        cr.new_sub_path();
        cr.arc(x + d / 2.0, y + d / 2.0, d / 2.0, 0.0, 2.0 * PI);
        cr.fill().ok();
        RING
    } else {
        0.0
    };
    disc(cr, service, x + inset, y + inset, d - 2.0 * inset);
}

/// Both services' discs, each `side` across: YouTube Music's in front and set off from
/// Spotify's by a ring. Needs `pair_width(side)` by `side + 2 * RING`.
pub fn draw_pair(cr: &cairo::Context, side: f64) {
    let step = side - side / 3.0;
    disc(cr, Service::Spotify, 0.0, RING, side);
    let (r, g, b) = theme::rgb(theme::BG);
    cr.set_source_rgb(r, g, b);
    cr.new_sub_path();
    cr.arc(
        step + side / 2.0,
        RING + side / 2.0,
        side / 2.0 + RING,
        0.0,
        2.0 * PI,
    );
    cr.fill().ok();
    disc(cr, Service::YouTubeMusic, step, RING, side);
}

pub fn pair_width(side: f64) -> f64 {
    side - side / 3.0 + side + RING
}

fn disc(cr: &cairo::Context, service: Service, x: f64, y: f64, d: f64) {
    let fill = match service {
        Service::Spotify => theme::SPOTIFY_GREEN,
        Service::YouTubeMusic => theme::YOUTUBE_RED,
    };
    let (r, g, b) = theme::rgb(fill);
    cr.set_source_rgb(r, g, b);
    cr.new_sub_path();
    cr.arc(x + d / 2.0, y + d / 2.0, d / 2.0, 0.0, 2.0 * PI);
    cr.fill().ok();
    let (cx, cy) = (x + d / 2.0, y + d / 2.0);
    match service {
        Service::Spotify => {
            // Three black arcs stacked, the widest on top: each one's crown above the centre,
            // half its width, how far its ends fall below the crown, and its stroke.
            cr.set_source_rgb(0.0, 0.0, 0.0);
            cr.set_line_cap(cairo::LineCap::Round);
            for (crown, half, fall, stroke) in [
                (0.17, 0.30, 0.13, 0.12),
                (-0.02, 0.25, 0.1, 0.1),
                (-0.19, 0.19, 0.07, 0.085),
            ] {
                let (top, half, fall) = (cy - crown * d, half * d, fall * d);
                cr.new_path();
                cr.move_to(cx - half, top + fall);
                cr.curve_to(
                    cx - half / 3.0,
                    top - fall / 3.0,
                    cx + half / 3.0,
                    top - fall / 3.0,
                    cx + half,
                    top + fall,
                );
                cr.set_line_width(stroke * d);
                cr.stroke().ok();
            }
        }
        Service::YouTubeMusic => {
            // A play triangle, nudged right so it looks centred.
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.new_path();
            cr.move_to(cx - 0.14 * d, cy - 0.22 * d);
            cr.line_to(cx + 0.24 * d, cy);
            cr.line_to(cx - 0.14 * d, cy + 0.22 * d);
            cr.close_path();
            cr.fill().ok();
        }
    }
}

/// A service's mark, `side` across, for a button.
pub fn mark(service: Service, side: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(side);
    area.set_content_height(side);
    area.set_can_target(false);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |_, cr, w, _| draw_badge(cr, service, 0.0, 0.0, f64::from(w), false));
    area
}

/// Made-up artwork for the demo, `px` square: a muted field with a bright disc, its hue and
/// the disc's place picked by `seed`, as the macOS window draws them.
pub fn demo_cover(seed: usize, px: i32) -> Option<gdk::Texture> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, px, px).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        let hue = (seed as f64 * 0.161).fract();
        let (r, g, b) = hsb(hue, 0.55, 0.42);
        cr.set_source_rgb(r, g, b);
        cr.paint().ok();
        let (r, g, b) = hsb((hue + 0.09).fract(), 0.45, 0.88);
        cr.set_source_rgb(r, g, b);
        let s = f64::from(px);
        let d = s * 0.58;
        // Placed as on macOS, whose y grows upwards.
        let (dx, dy) = [(0.62, 0.38), (0.35, 0.6), (0.5, 0.5), (0.7, 0.68)][seed % 4];
        cr.arc(s * dx, s * (1.0 - dy), d / 2.0, 0.0, 2.0 * PI);
        cr.fill().ok();
    }
    texture(surface)
}

/// A drawn surface as a texture.
/// `icon` drawn `size` points square in `color`, at twice the pixels for sharp HiDPI.
pub fn paintable(icon: Icon, size: i32, color: u32) -> Option<gdk::Texture> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size * 2, size * 2).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(2.0, 2.0);
        let (r, g, b) = theme::rgb(color);
        cr.set_source_rgb(r, g, b);
        icon.draw(&cr, f64::from(size));
    }
    texture(surface)
}

pub fn texture(mut surface: cairo::ImageSurface) -> Option<gdk::Texture> {
    surface.flush();
    let (w, h, stride) = (surface.width(), surface.height(), surface.stride());
    let data = surface.data().ok()?.to_vec();
    let bytes = glib::Bytes::from_owned(data);
    Some(
        gdk::MemoryTexture::new(
            w,
            h,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &bytes,
            stride as usize,
        )
        .upcast(),
    )
}

fn hsb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i64).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_to_shapes() {
        for icon in Icon::ALL {
            let ops = parse(icon.svg());
            assert!(!ops.is_empty(), "{icon:?}");
            for op in &ops {
                if let Op::Path(segs) = op {
                    assert!(matches!(segs.first(), Some(Seg::Move(..))), "{icon:?}");
                    assert!(segs.len() > 1, "{icon:?}: {segs:?}");
                }
            }
        }
    }

    #[test]
    fn packed_numbers_split_as_svg_reads_them() {
        assert_eq!(numbers("1-2.5.5,3 4e1"), vec![1.0, -2.5, 0.5, 3.0, 40.0]);
    }

    #[test]
    fn relative_moves_and_arcs_land_where_they_should() {
        let segs = path("M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587z");
        assert_eq!(segs[0], Seg::Move(11.0, 4.702));
        let Seg::Arc { to, sweep, .. } = segs[1] else {
            panic!("{segs:?}")
        };
        assert!((to.0 - 9.797).abs() < 1e-9 && (to.1 - 4.204).abs() < 1e-9);
        assert!(!sweep);
        assert_eq!(segs[2], Seg::Line(6.413, 7.587));
        assert_eq!(segs[3], Seg::Close);
    }

    #[test]
    fn attributes_are_read_whole() {
        let tag = r#"circle cx="12" cy="13" r="3" /"#;
        assert_eq!(attr(tag, "cx"), Some("12"));
        assert_eq!(attr(tag, "r"), Some("3"));
        assert_eq!(attr(tag, "x"), None);
    }

    #[test]
    fn badges_grow_with_the_cover_from_14_to_20() {
        assert_eq!(badge_side(32.0), 14);
        assert_eq!(badge_side(128.0), 20);
        assert_eq!(badge_side(400.0), 20);
    }
}
