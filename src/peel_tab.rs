use egui::Pos2;
use geo::{BoundingRect, Contains, Coord, Intersects, LineString, Point, Polygon};

const TAB_SAMPLES: usize = 12;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PeelTab {
    pub path: LineString<f32>,
    pub handle: Coord<f32>,
}

/// Generate tabs only for enabled, top-level contours. Rings enclosed by
/// another enabled contour describe holes, where a tab would cut into the
/// sticker material instead of away from it.
pub(crate) fn peel_tabs(
    paths: &[LineString<f32>],
    enabled: &[bool],
    positions: &[Option<f32>],
) -> Vec<(usize, PeelTab)> {
    let bounds = paths
        .iter()
        .map(LineString::bounding_rect)
        .collect::<Vec<_>>();
    paths
        .iter()
        .enumerate()
        .filter(|(index, path)| {
            enabled.get(*index).copied().unwrap_or(true)
                && path.0.len() >= 4
                && path.0.first() == path.0.last()
        })
        .filter(|(index, path)| {
            !paths
                .iter()
                .enumerate()
                .any(|(container_index, container)| {
                    let bounds_contain_path = match (bounds[container_index], bounds[*index]) {
                        (Some(container), Some(inner)) => {
                            container.min().x <= inner.min().x
                                && container.min().y <= inner.min().y
                                && container.max().x >= inner.max().x
                                && container.max().y >= inner.max().y
                        }
                        _ => false,
                    };
                    container_index != *index
                        && enabled.get(container_index).copied().unwrap_or(true)
                        && container.0.len() >= 4
                        && container.0.first() == container.0.last()
                        && bounds_contain_path
                        && Polygon::new(container.clone(), Vec::new()).contains(*path)
                })
        })
        .filter_map(|(index, path)| {
            peel_tab(path, positions.get(index).copied().flatten()).map(|tab| (index, tab))
        })
        .collect()
}

/// Build a peel-tab cut which follows the contour at its endpoints and bows
/// away from the contour's positive volume. `position` is normalized arc
/// length; `None` chooses the bottom-most centered point for compatibility.
pub(crate) fn peel_tab(path: &LineString<f32>, position: Option<f32>) -> Option<PeelTab> {
    if path.0.len() < 4 || path.0.first() != path.0.last() {
        return None;
    }
    let bounds = path.bounding_rect()?;
    let total = perimeter_length(path);
    if !total.is_finite() || total <= f32::EPSILON {
        return None;
    }
    let position = position
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or_else(|| {
            nearest_perimeter_position(
                path,
                Pos2::new((bounds.min().x + bounds.max().x) / 2.0, bounds.max().y),
            )
            .unwrap_or(0.0)
        });
    let width = (bounds.width() * 0.25).clamp(30.0, 120.0).min(total * 0.4);
    if width <= f32::EPSILON {
        return None;
    }
    let depth = (width * 0.35).clamp(12.0, 42.0).min(total * 0.2);
    let center_distance = position * total;
    let area_twice = signed_area_twice(path);
    if area_twice.abs() <= f32::EPSILON {
        return None;
    }

    let polygon = Polygon::new(path.clone(), Vec::new());
    // A local outward normal can point across a narrow concavity and through
    // another part of the sticker. Reduce depth until the whole bow remains
    // in exterior space; omit the tab if even a shallow useful bow is unsafe.
    for depth_scale in [1.0, 0.75, 0.5, 0.35, 0.25, 0.15, 0.1] {
        let mut points = Vec::with_capacity(TAB_SAMPLES + 1);
        for index in 0..=TAB_SAMPLES {
            let t = index as f32 / TAB_SAMPLES as f32;
            let distance = center_distance + width * (t - 0.5);
            let (point, tangent) = point_and_tangent(path, distance, total)?;
            let outward = if area_twice > 0.0 {
                Coord {
                    x: tangent.y,
                    y: -tangent.x,
                }
            } else {
                Coord {
                    x: -tangent.y,
                    y: tangent.x,
                }
            };
            let offset = if index == 0 || index == TAB_SAMPLES {
                0.0
            } else {
                depth * depth_scale * (std::f32::consts::PI * t).sin()
            };
            points.push(Coord {
                x: point.x + outward.x * offset,
                y: point.y + outward.y * offset,
            });
        }
        if tab_stays_outside(path, &polygon, &points) {
            let handle = points[TAB_SAMPLES / 2];
            return Some(PeelTab {
                path: LineString::new(points),
                handle,
            });
        }
    }
    None
}

fn tab_stays_outside(
    owner: &LineString<f32>,
    polygon: &Polygon<f32>,
    points: &[Coord<f32>],
) -> bool {
    if points.len() < 3
        || points[1..points.len() - 1]
            .iter()
            .any(|point| polygon.contains(&Point::new(point.x, point.y)))
    {
        return false;
    }

    // Trim the two intended endpoint contacts before checking for any other
    // crossing of the owning contour.
    let mut trimmed = points.to_vec();
    const ENDPOINT_TRIM: f32 = 0.01;
    trimmed[0] = lerp_coord(trimmed[0], trimmed[1], ENDPOINT_TRIM);
    let last = trimmed.len() - 1;
    trimmed[last] = lerp_coord(trimmed[last], trimmed[last - 1], ENDPOINT_TRIM);
    !LineString::new(trimmed).intersects(owner)
}

fn lerp_coord(from: Coord<f32>, to: Coord<f32>, amount: f32) -> Coord<f32> {
    Coord {
        x: from.x + (to.x - from.x) * amount,
        y: from.y + (to.y - from.y) * amount,
    }
}

/// Project a canvas point to the closest location on a contour and return its
/// normalized arc length. This makes dragging stable on irregular outlines.
pub(crate) fn nearest_perimeter_position(path: &LineString<f32>, pointer: Pos2) -> Option<f32> {
    let total = perimeter_length(path);
    if !total.is_finite() || total <= f32::EPSILON {
        return None;
    }
    let mut travelled = 0.0;
    let mut best: Option<(f32, f32)> = None;
    for segment in path.0.windows(2) {
        let dx = segment[1].x - segment[0].x;
        let dy = segment[1].y - segment[0].y;
        let length_squared = dx * dx + dy * dy;
        if !length_squared.is_finite() || length_squared <= f32::EPSILON {
            continue;
        }
        let length = length_squared.sqrt();
        let t = (((pointer.x - segment[0].x) * dx + (pointer.y - segment[0].y) * dy)
            / length_squared)
            .clamp(0.0, 1.0);
        let projected_x = segment[0].x + dx * t;
        let projected_y = segment[0].y + dy * t;
        let distance_squared =
            (pointer.x - projected_x).powi(2) + (pointer.y - projected_y).powi(2);
        if best.is_none_or(|(best_distance, _)| distance_squared < best_distance) {
            best = Some((distance_squared, travelled + length * t));
        }
        travelled += length;
    }
    best.map(|(_, distance)| (distance / total).clamp(0.0, 1.0))
}

fn perimeter_length(path: &LineString<f32>) -> f32 {
    path.0
        .windows(2)
        .map(|segment| (segment[1].x - segment[0].x).hypot(segment[1].y - segment[0].y))
        .filter(|length| length.is_finite())
        .sum()
}

fn signed_area_twice(path: &LineString<f32>) -> f32 {
    path.0
        .windows(2)
        .map(|segment| segment[0].x * segment[1].y - segment[1].x * segment[0].y)
        .sum()
}

fn point_and_tangent(
    path: &LineString<f32>,
    distance: f32,
    total: f32,
) -> Option<(Coord<f32>, Coord<f32>)> {
    let mut target = distance.rem_euclid(total);
    for segment in path.0.windows(2) {
        let dx = segment[1].x - segment[0].x;
        let dy = segment[1].y - segment[0].y;
        let length = dx.hypot(dy);
        if !length.is_finite() || length <= f32::EPSILON {
            continue;
        }
        if target <= length {
            return Some((
                Coord {
                    x: segment[0].x + dx * target / length,
                    y: segment[0].y + dy * target / length,
                },
                Coord {
                    x: dx / length,
                    y: dy / length,
                },
            ));
        }
        target -= length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(clockwise: bool) -> LineString<f32> {
        let mut points = vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 100.0),
            (0.0, 100.0),
            (0.0, 0.0),
        ];
        if clockwise {
            points.reverse();
        }
        LineString::from(points)
    }

    #[test]
    fn default_tab_bows_below_the_positive_volume_for_either_winding() {
        for path in [square(false), square(true)] {
            let tab = peel_tab(&path, None).unwrap();
            assert!(tab.handle.y > 100.0);
            assert_eq!(tab.path.0.first().unwrap().y, 100.0);
            assert_eq!(tab.path.0.last().unwrap().y, 100.0);
        }
    }

    #[test]
    fn explicit_positions_move_around_the_perimeter_and_stay_outside() {
        let path = square(false);
        let top = peel_tab(&path, Some(0.125)).unwrap().handle;
        let right = peel_tab(&path, Some(0.375)).unwrap().handle;
        let bottom = peel_tab(&path, Some(0.625)).unwrap().handle;
        let left = peel_tab(&path, Some(0.875)).unwrap().handle;
        assert!(top.y < 0.0);
        assert!(right.x > 100.0);
        assert!(bottom.y > 100.0);
        assert!(left.x < 0.0);
    }

    #[test]
    fn nearest_projection_returns_normalized_perimeter_distance() {
        let path = square(false);
        assert!(
            (nearest_perimeter_position(&path, Pos2::new(100.0, 50.0)).unwrap() - 0.375).abs()
                < 0.001
        );
        assert!(
            (nearest_perimeter_position(&path, Pos2::new(50.0, 100.0)).unwrap() - 0.625).abs()
                < 0.001
        );
    }

    #[test]
    fn open_and_degenerate_contours_do_not_get_tabs() {
        assert!(peel_tab(&LineString::from(vec![(0.0, 0.0), (1.0, 0.0)]), None).is_none());
        assert!(
            peel_tab(
                &LineString::from(vec![(0.0, 0.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)]),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn nested_hole_contours_do_not_get_peel_tabs() {
        let outer = square(false);
        let hole = LineString::from(vec![
            (25.0, 25.0),
            (75.0, 25.0),
            (75.0, 75.0),
            (25.0, 75.0),
            (25.0, 25.0),
        ]);
        let tabs = peel_tabs(&[outer, hole], &[true, true], &[]);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].0, 0);
    }

    #[test]
    fn tab_shrinks_to_stay_outside_a_narrow_concavity() {
        let path = LineString::from(vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 100.0),
            (52.0, 100.0),
            (52.0, 20.0),
            (48.0, 20.0),
            (48.0, 100.0),
            (0.0, 100.0),
            (0.0, 0.0),
        ]);
        let tab = peel_tab(&path, Some(288.0 / 560.0)).expect("concavity has exterior clearance");
        let polygon = Polygon::new(path.clone(), Vec::new());
        assert!(tab.handle.x < 52.0 && tab.handle.x > 48.0);
        assert!(
            tab.path.0[1..tab.path.0.len() - 1]
                .iter()
                .all(|point| !polygon.contains(&Point::new(point.x, point.y)))
        );
        assert!(tab_stays_outside(&path, &polygon, &tab.path.0));
    }
}
