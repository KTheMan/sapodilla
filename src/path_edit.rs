use geo::{BooleanOps, BoundingRect, Coord, Intersects, LineString, MultiPolygon, Polygon};

/// Smooth a path with Chaikin corner cutting while preserving whether it was
/// open or closed. Repeated passes are deliberately capped to keep interactive
/// editing responsive and generated files bounded.
pub fn smooth_path(path: &LineString<f32>, passes: usize) -> LineString<f32> {
    let mut points = path.0.clone();
    let closed = points.len() >= 2 && points.first() == points.last();
    if closed {
        points.pop();
    }
    for _ in 0..passes.min(5) {
        if points.len() < 2 {
            break;
        }
        let mut next = Vec::with_capacity(points.len() * 2 + usize::from(closed));
        if !closed {
            next.push(points[0]);
        }
        let edge_count = if closed {
            points.len()
        } else {
            points.len() - 1
        };
        for index in 0..edge_count {
            let a = points[index];
            let b = points[(index + 1) % points.len()];
            next.push(lerp(a, b, 0.25));
            next.push(lerp(a, b, 0.75));
        }
        if !closed {
            next.push(*points.last().expect("non-empty path"));
        }
        points = next;
    }
    if closed && !points.is_empty() {
        points.push(points[0]);
    }
    LineString(points)
}

/// Boolean-union closed contours. Exterior rings and holes are returned as
/// separate editable cut paths because the cutter consumes independent paths.
pub fn union_paths(paths: &[LineString<f32>]) -> Vec<LineString<f32>> {
    let mut merged = MultiPolygon::<f32>(Vec::new());
    for path in paths {
        let Some(ring) = closed_ring(path) else {
            continue;
        };
        let polygon = Polygon::new(ring, Vec::new());
        let bounds = polygon.bounding_rect();
        if merged.0.is_empty()
            || !merged.0.iter().any(|existing| {
                existing
                    .bounding_rect()
                    .zip(bounds)
                    .is_some_and(|(existing, incoming)| existing.intersects(&incoming))
            })
        {
            merged.0.push(polygon);
        } else {
            merged = merged.union(&polygon);
        }
    }

    let mut result = Vec::new();
    for polygon in merged {
        result.push(polygon.exterior().clone());
        result.extend(polygon.interiors().iter().cloned());
    }
    result
}

fn closed_ring(path: &LineString<f32>) -> Option<LineString<f32>> {
    if path.0.len() < 4 || path.0.first() != path.0.last() {
        return None;
    }
    Some(path.clone())
}

fn lerp(a: Coord<f32>, b: Coord<f32>, t: f32) -> Coord<f32> {
    Coord {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}

#[cfg(test)]
mod tests {
    use geo::{Area, BoundingRect};

    use super::*;

    fn rectangle(x: f32, y: f32, width: f32, height: f32) -> LineString<f32> {
        LineString::from(vec![
            (x, y),
            (x + width, y),
            (x + width, y + height),
            (x, y + height),
            (x, y),
        ])
    }

    #[test]
    fn smoothing_preserves_open_endpoints_and_closed_state() {
        let open = LineString::from(vec![(0., 0.), (10., 0.), (10., 10.)]);
        let smoothed = smooth_path(&open, 1);
        assert_eq!(smoothed.0.first(), open.0.first());
        assert_eq!(smoothed.0.last(), open.0.last());
        assert_eq!(smoothed.0.len(), 6);

        let closed = rectangle(0., 0., 10., 10.);
        let smoothed = smooth_path(&closed, 2);
        assert_eq!(smoothed.0.first(), smoothed.0.last());
        assert_eq!(smoothed.0.len(), 17);
    }

    #[test]
    fn union_merges_overlapping_shapes_without_losing_area() {
        let result = union_paths(&[rectangle(0., 0., 10., 10.), rectangle(5., 0., 10., 10.)]);
        assert_eq!(result.len(), 1);
        let polygon = Polygon::new(result[0].clone(), Vec::new());
        assert!((polygon.unsigned_area() - 150.0).abs() < 0.001);
        let bounds = result[0].bounding_rect().unwrap();
        assert_eq!(bounds.width(), 15.0);
    }

    #[test]
    fn union_ignores_open_and_degenerate_paths() {
        assert!(union_paths(&[LineString::from(vec![(0., 0.), (1., 1.), (2., 0.)])]).is_empty());
    }

    #[test]
    fn union_keeps_large_disjoint_batches_independent() {
        let paths = (0..100)
            .map(|index| rectangle(index as f32 * 20.0, 0.0, 10.0, 10.0))
            .collect::<Vec<_>>();
        let result = union_paths(&paths);
        assert_eq!(result.len(), paths.len());
        assert_eq!(result, paths);
    }
}
