//! Line icons for the launcher, drawn with egui shapes (designed on an 18×18 grid, scaled to
//! the target rectangle): no icon font needed, sharp at any size.

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Monitor,
    Clock,
    Gear,
    Star,
    Plus,
    Pencil,
    Plug,
    Unplug,
    Search,
    Eye,
    EyeOff,
    User,
    Building,
    Server,
    File,
    Exit,
    Dots,
    Close,
    Folder,
    Lock,
}

struct Pen<'a> {
    p: &'a Painter,
    o: Pos2,
    s: f32,
    stroke: Stroke,
    color: Color32,
}

impl Pen<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        self.o + vec2(x, y) * self.s
    }
    fn line(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::line(v, self.stroke));
    }
    fn closed(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::closed_line(v, self.stroke));
    }
    fn circle(&self, x: f32, y: f32, r: f32) {
        self.p.circle_stroke(self.pt(x, y), r * self.s, self.stroke);
    }
    fn disc(&self, x: f32, y: f32, r: f32) {
        self.p.circle_filled(self.pt(x, y), r * self.s, self.color);
    }
    fn rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
        self.p.rect_stroke(
            Rect::from_min_max(self.pt(x0, y0), self.pt(x1, y1)),
            r * self.s,
            self.stroke,
            eframe::egui::StrokeKind::Middle,
        );
    }
    /// Arc around (cx, cy) from angle a0 to a1 (radians, 0 = right, clockwise on screen).
    fn arc(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) {
        let n = 24;
        let pts: Vec<(f32, f32)> = (0..=n)
            .map(|i| {
                let a = a0 + (a1 - a0) * i as f32 / n as f32;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        self.line(&pts);
    }
}

fn star_points(cx: f32, cy: f32, r_out: f32, r_in: f32) -> Vec<(f32, f32)> {
    (0..10)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            let r = if i % 2 == 0 { r_out } else { r_in };
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// Paints `icon` into `rect` (square recommended).
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = rect.width().min(rect.height());
    let s = side / 18.0;
    let o = rect.center() - vec2(9.0, 9.0) * s;
    let pen = Pen {
        p,
        o,
        s,
        stroke: Stroke::new((1.5 * s).max(1.2), color),
        color,
    };
    use std::f32::consts::PI;
    match icon {
        Icon::Monitor => {
            pen.rect(2.0, 3.0, 16.0, 12.5, 1.5);
            pen.line(&[(9.0, 12.5), (9.0, 15.0)]);
            pen.line(&[(5.5, 15.5), (12.5, 15.5)]);
        }
        Icon::Clock => {
            pen.circle(9.0, 9.0, 7.0);
            pen.line(&[(9.0, 4.8), (9.0, 9.0), (12.0, 10.8)]);
        }
        Icon::Gear => {
            pen.circle(9.0, 9.0, 2.6);
            pen.circle(9.0, 9.0, 5.4);
            for i in 0..8 {
                let a = i as f32 * PI / 4.0;
                pen.line(&[
                    (9.0 + 5.4 * a.cos(), 9.0 + 5.4 * a.sin()),
                    (9.0 + 7.6 * a.cos(), 9.0 + 7.6 * a.sin()),
                ]);
            }
        }
        Icon::Star => pen.closed(&star_points(9.0, 9.6, 7.6, 3.4)),
        Icon::Plus => {
            pen.line(&[(9.0, 3.5), (9.0, 14.5)]);
            pen.line(&[(3.5, 9.0), (14.5, 9.0)]);
        }
        Icon::Pencil => {
            pen.closed(&[
                (3.0, 15.0),
                (3.8, 11.6),
                (12.0, 3.4),
                (14.6, 6.0),
                (6.4, 14.2),
            ]);
            pen.line(&[(10.6, 4.8), (13.2, 7.4)]);
        }
        Icon::Plug | Icon::Unplug => {
            pen.line(&[(6.8, 2.5), (6.8, 6.0)]);
            pen.line(&[(11.2, 2.5), (11.2, 6.0)]);
            pen.closed(&[
                (4.5, 6.0),
                (13.5, 6.0),
                (13.5, 8.5),
                (11.0, 12.0),
                (7.0, 12.0),
                (4.5, 8.5),
            ]);
            pen.line(&[(9.0, 12.0), (9.0, 16.0)]);
            if icon == Icon::Unplug {
                pen.line(&[(2.5, 2.5), (15.5, 15.5)]);
            }
        }
        Icon::Search => {
            pen.circle(7.8, 7.8, 4.8);
            pen.line(&[(11.4, 11.4), (15.5, 15.5)]);
        }
        Icon::Eye | Icon::EyeOff => {
            pen.arc(9.0, 15.0, 8.5, -PI * 0.83, -PI * 0.17);
            pen.arc(9.0, 3.0, 8.5, PI * 0.17, PI * 0.83);
            pen.circle(9.0, 9.0, 2.4);
            if icon == Icon::EyeOff {
                pen.line(&[(3.0, 15.0), (15.0, 3.0)]);
            }
        }
        Icon::User => {
            pen.circle(9.0, 6.0, 3.2);
            pen.arc(9.0, 16.0, 6.0, PI, 2.0 * PI);
        }
        Icon::Building => {
            pen.rect(3.5, 2.5, 11.5, 15.5, 1.0);
            pen.line(&[(11.5, 7.0), (15.0, 7.0), (15.0, 15.5), (11.5, 15.5)]);
            for y in [5.5_f32, 8.5, 11.5] {
                pen.line(&[(5.8, y), (6.8, y)]);
                pen.line(&[(8.4, y), (9.4, y)]);
            }
        }
        Icon::Server => {
            pen.rect(3.0, 2.5, 15.0, 15.5, 1.5);
            pen.line(&[(6.0, 6.5), (12.0, 6.5)]);
            pen.line(&[(6.0, 9.5), (12.0, 9.5)]);
            pen.line(&[(6.0, 12.5), (10.0, 12.5)]);
        }
        Icon::File => {
            pen.closed(&[
                (4.0, 2.0),
                (11.0, 2.0),
                (14.5, 5.5),
                (14.5, 16.0),
                (4.0, 16.0),
            ]);
            pen.line(&[(11.0, 2.0), (11.0, 5.5), (14.5, 5.5)]);
            pen.line(&[(6.5, 9.0), (12.0, 9.0)]);
            pen.line(&[(6.5, 12.0), (12.0, 12.0)]);
        }
        Icon::Exit => {
            pen.line(&[(10.0, 2.5), (3.0, 2.5), (3.0, 15.5), (10.0, 15.5)]);
            pen.line(&[(7.5, 9.0), (16.0, 9.0)]);
            pen.line(&[(12.8, 5.8), (16.0, 9.0), (12.8, 12.2)]);
        }
        Icon::Dots => {
            for y in [4.0, 9.0, 14.0] {
                pen.disc(9.0, y, 1.4);
            }
        }
        Icon::Close => {
            pen.line(&[(4.0, 4.0), (14.0, 14.0)]);
            pen.line(&[(14.0, 4.0), (4.0, 14.0)]);
        }
        Icon::Lock => {
            pen.rect(3.5, 8.0, 14.5, 16.0, 1.5);
            pen.arc(
                9.0,
                8.0,
                3.6,
                std::f32::consts::PI,
                2.0 * std::f32::consts::PI,
            );
            pen.line(&[(5.4, 8.0), (5.4, 7.6)]);
            pen.line(&[(12.6, 8.0), (12.6, 7.6)]);
            pen.disc(9.0, 11.6, 1.1);
        }
        Icon::Folder => {
            pen.closed(&[
                (2.0, 4.0),
                (7.0, 4.0),
                (8.5, 6.0),
                (16.0, 6.0),
                (16.0, 15.0),
                (2.0, 15.0),
            ]);
        }
    }
}

/// The monitor picture of the current connection (mockup): bezel, blue screen while
/// connected, grey otherwise.
pub fn screen(p: &Painter, rect: Rect, on: bool) {
    let bezel = Color32::from_rgb(70, 110, 170);
    let w = rect.width();
    let screen_h = rect.height() * 0.78;
    let outer = Rect::from_min_size(rect.min, vec2(w, screen_h));
    p.rect_filled(outer, 6.0, bezel);
    let inner = outer.shrink(5.0);
    if on {
        // vertical blue gradient with a light "bloom"
        let top = Color32::from_rgb(20, 70, 170);
        let bottom = Color32::from_rgb(10, 30, 90);
        let mut mesh = eframe::egui::Mesh::default();
        mesh.colored_vertex(inner.left_top(), top);
        mesh.colored_vertex(inner.right_top(), top);
        mesh.colored_vertex(inner.right_bottom(), bottom);
        mesh.colored_vertex(inner.left_bottom(), bottom);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        p.add(Shape::mesh(mesh));
        // soft light: many faint discs (no visible rings)
        let c = inner.center() + vec2(inner.width() * 0.1, inner.height() * 0.15);
        for i in 0..16 {
            let k = 0.55 - i as f32 * 0.03;
            p.circle_filled(
                c,
                inner.height() * k,
                Color32::from_rgba_unmultiplied(60, 140, 255, 14),
            );
        }
    } else {
        p.rect_filled(inner, 3.0, Color32::from_rgb(28, 36, 50));
    }
    let neck_top = pos2(rect.center().x, outer.bottom());
    p.rect_filled(
        Rect::from_center_size(neck_top + vec2(0.0, 6.0), vec2(w * 0.08, 12.0)),
        0.0,
        bezel,
    );
    p.rect_filled(
        Rect::from_center_size(
            pos2(rect.center().x, outer.bottom() + 13.0),
            vec2(w * 0.36, 4.0),
        ),
        2.0,
        bezel,
    );
}
