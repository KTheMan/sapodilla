use egui::Vec2;
use geo::{Coord, LineString};

/// Procedural cut shapes available to the studio shape designer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProceduralShape {
    Rectangle,
    RoundedRectangle,
    Ellipse,
    Triangle,
    Diamond,
    Pentagon,
    Hexagon,
    Octagon,
    Star5,
    Star8,
    Heart,
    Cross,
    ArrowRight,
    SpeechBubble,
    Tag,
    Shield,
    Cloud,
    Burst,
}

impl ProceduralShape {
    pub const ALL: [Self; 18] = [
        Self::Rectangle,
        Self::RoundedRectangle,
        Self::Ellipse,
        Self::Triangle,
        Self::Diamond,
        Self::Pentagon,
        Self::Hexagon,
        Self::Octagon,
        Self::Star5,
        Self::Star8,
        Self::Heart,
        Self::Cross,
        Self::ArrowRight,
        Self::SpeechBubble,
        Self::Tag,
        Self::Shield,
        Self::Cloud,
        Self::Burst,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Rectangle => "Rectangle",
            Self::RoundedRectangle => "Rounded rectangle",
            Self::Ellipse => "Ellipse",
            Self::Triangle => "Triangle",
            Self::Diamond => "Diamond",
            Self::Pentagon => "Pentagon",
            Self::Hexagon => "Hexagon",
            Self::Octagon => "Octagon",
            Self::Star5 => "5-point star",
            Self::Star8 => "8-point star",
            Self::Heart => "Heart",
            Self::Cross => "Cross",
            Self::ArrowRight => "Arrow",
            Self::SpeechBubble => "Speech bubble",
            Self::Tag => "Tag",
            Self::Shield => "Shield",
            Self::Cloud => "Cloud",
            Self::Burst => "Burst",
        }
    }
}

/// Generate a closed path fitted inside `size`.
pub fn generate(shape: ProceduralShape, size: Vec2) -> LineString<f32> {
    let (w, h) = (size.x.max(1.0), size.y.max(1.0));
    let points = match shape {
        ProceduralShape::Rectangle => vec![(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)],
        ProceduralShape::RoundedRectangle => rounded_rectangle(w, h),
        ProceduralShape::Ellipse => radial(w, h, 64, |_| 1.0, -90.0),
        ProceduralShape::Triangle => polygon(w, h, 3, -90.0),
        ProceduralShape::Diamond => polygon(w, h, 4, -90.0),
        ProceduralShape::Pentagon => polygon(w, h, 5, -90.0),
        ProceduralShape::Hexagon => polygon(w, h, 6, 0.0),
        ProceduralShape::Octagon => polygon(w, h, 8, 22.5),
        ProceduralShape::Star5 => star(w, h, 5, 0.44),
        ProceduralShape::Star8 => star(w, h, 8, 0.52),
        ProceduralShape::Heart => heart(w, h),
        ProceduralShape::Cross => scale_points(
            w,
            h,
            &[
                (0.34, 0.),
                (0.66, 0.),
                (0.66, 0.34),
                (1., 0.34),
                (1., 0.66),
                (0.66, 0.66),
                (0.66, 1.),
                (0.34, 1.),
                (0.34, 0.66),
                (0., 0.66),
                (0., 0.34),
                (0.34, 0.34),
            ],
        ),
        ProceduralShape::ArrowRight => scale_points(
            w,
            h,
            &[
                (0., 0.3),
                (0.58, 0.3),
                (0.58, 0.),
                (1., 0.5),
                (0.58, 1.),
                (0.58, 0.7),
                (0., 0.7),
            ],
        ),
        ProceduralShape::SpeechBubble => scale_points(
            w,
            h,
            &[
                (0.12, 0.),
                (0.88, 0.),
                (1., 0.12),
                (1., 0.7),
                (0.88, 0.82),
                (0.58, 0.82),
                (0.36, 1.),
                (0.4, 0.82),
                (0.12, 0.82),
                (0., 0.7),
                (0., 0.12),
            ],
        ),
        ProceduralShape::Tag => scale_points(
            w,
            h,
            &[
                (0., 0.18),
                (0.18, 0.),
                (1., 0.),
                (1., 1.),
                (0.18, 1.),
                (0., 0.82),
            ],
        ),
        ProceduralShape::Shield => scale_points(
            w,
            h,
            &[
                (0.5, 0.),
                (1., 0.15),
                (0.9, 0.68),
                (0.5, 1.),
                (0.1, 0.68),
                (0., 0.15),
            ],
        ),
        ProceduralShape::Cloud => radial(w, h, 96, |a| 0.84 + 0.16 * (5. * a).cos().abs(), -90.),
        ProceduralShape::Burst => star(w, h, 18, 0.76),
    };
    close(points)
}

fn close(points: Vec<(f32, f32)>) -> LineString<f32> {
    let mut out = points
        .into_iter()
        .map(|(x, y)| Coord { x, y })
        .collect::<Vec<_>>();
    if let Some(first) = out.first().copied()
        && out.last() != Some(&first)
    {
        out.push(first);
    }
    LineString::new(out)
}

fn radial(
    w: f32,
    h: f32,
    count: usize,
    radius: impl Fn(f32) -> f32,
    start_degrees: f32,
) -> Vec<(f32, f32)> {
    (0..count)
        .map(|i| {
            let a = start_degrees.to_radians() + std::f32::consts::TAU * i as f32 / count as f32;
            let r = radius(a);
            (w * (0.5 + 0.5 * r * a.cos()), h * (0.5 + 0.5 * r * a.sin()))
        })
        .collect()
}

fn polygon(w: f32, h: f32, sides: usize, start: f32) -> Vec<(f32, f32)> {
    radial(w, h, sides, |_| 1., start)
}

fn star(w: f32, h: f32, points: usize, inner: f32) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(points * 2);
    for i in 0..points * 2 {
        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / points as f32;
        let r = if i % 2 == 0 { 1. } else { inner };
        out.push((w * (0.5 + 0.5 * r * a.cos()), h * (0.5 + 0.5 * r * a.sin())));
    }
    out
}

fn rounded_rectangle(w: f32, h: f32) -> Vec<(f32, f32)> {
    let r = (w.min(h) * 0.18).min(w / 2.).min(h / 2.);
    let corners = [
        (w - r, r, -90.),
        (w - r, h - r, 0.),
        (r, h - r, 90.),
        (r, r, 180.),
    ];
    corners
        .into_iter()
        .flat_map(|(cx, cy, start): (f32, f32, f32)| {
            (0..=8).map(move |i| {
                let a = (start + 90. * i as f32 / 8.).to_radians();
                (cx + r * a.cos(), cy + r * a.sin())
            })
        })
        .collect()
}

fn heart(w: f32, h: f32) -> Vec<(f32, f32)> {
    let raw = (0..96)
        .map(|i| {
            let t = std::f32::consts::TAU * i as f32 / 96.;
            let x = 16. * t.sin().powi(3);
            let y = -(13. * t.cos() - 5. * (2. * t).cos() - 2. * (3. * t).cos() - (4. * t).cos());
            (x, y)
        })
        .collect::<Vec<_>>();
    fit(raw, w, h)
}

fn fit(points: Vec<(f32, f32)>, w: f32, h: f32) -> Vec<(f32, f32)> {
    let min_x = points.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
    points
        .into_iter()
        .map(|(x, y)| {
            (
                (x - min_x) / (max_x - min_x) * w,
                (y - min_y) / (max_y - min_y) * h,
            )
        })
        .collect()
}

fn scale_points(w: f32, h: f32, points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    points.iter().map(|(x, y)| (x * w, y * h)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::BoundingRect;

    #[test]
    fn catalog_shapes_are_closed_finite_and_bounded() {
        let size = Vec2::new(320., 180.);
        for shape in ProceduralShape::ALL {
            let path = generate(shape, size);
            assert!(path.0.len() >= 4, "{}", shape.name());
            assert_eq!(path.0.first(), path.0.last(), "{}", shape.name());
            assert!(path.0.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
            let bounds = path.bounding_rect().unwrap();
            assert!(
                bounds.min().x >= -0.001 && bounds.min().y >= -0.001,
                "{}",
                shape.name()
            );
            assert!(
                bounds.max().x <= size.x + 0.001 && bounds.max().y <= size.y + 0.001,
                "{}",
                shape.name()
            );
        }
    }

    #[test]
    fn shape_names_are_unique() {
        let mut names = ProceduralShape::ALL.map(ProceduralShape::name);
        names.sort_unstable();
        names
            .windows(2)
            .for_each(|pair| assert_ne!(pair[0], pair[1]));
    }
}
