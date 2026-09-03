use geo::LineString;
use serde::{Deserialize, Serialize};

use crate::studio;

/// Physical operation assigned to an individual contour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CutMode {
    #[default]
    Kiss,
    Perforation,
    Disabled,
}

impl CutMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kiss => "Kiss cut",
            Self::Perforation => "Perforation",
            Self::Disabled => "No cut",
        }
    }
}

/// Resolves the legacy "all paths perforation" switch without allowing it to
/// revive explicitly disabled paths. Output and preview both use this mapping.
pub fn effective_cut_modes(
    path_count: usize,
    modes: &[CutMode],
    all_perforation: bool,
) -> Vec<CutMode> {
    (0..path_count)
        .map(|index| match modes.get(index).copied() {
            Some(CutMode::Disabled) => CutMode::Disabled,
            Some(mode) if !all_perforation => mode,
            _ if all_perforation => CutMode::Perforation,
            _ => CutMode::Kiss,
        })
        .collect()
}

/// A pressure-homogeneous group ready to serialize to the plotter language.
/// Kiss cuts intentionally precede perforation cuts so the sheet remains
/// registered while its sticker outlines are cut.
#[derive(Clone, Debug, PartialEq)]
pub struct CutPhase {
    pub mode: CutMode,
    pub pressure: u8,
    pub paths: Vec<LineString<f32>>,
}

pub fn plan_cut_phases(
    paths: &[LineString<f32>],
    modes: &[CutMode],
    kiss_pressure: u8,
    perforation_pressure: u8,
    dash: f32,
    gap: f32,
) -> Vec<CutPhase> {
    let mut kiss = Vec::new();
    let mut perforation = Vec::new();

    for (index, path) in paths.iter().enumerate() {
        match modes.get(index).copied().unwrap_or_default() {
            CutMode::Kiss if path.0.len() >= 2 => kiss.push(path.clone()),
            CutMode::Perforation if path.0.len() >= 2 => {
                perforation.extend(studio::perf_cut(path, dash, gap));
            }
            CutMode::Kiss | CutMode::Perforation | CutMode::Disabled => {}
        }
    }

    let mut phases = Vec::with_capacity(2);
    if !kiss.is_empty() {
        phases.push(CutPhase {
            mode: CutMode::Kiss,
            pressure: kiss_pressure,
            paths: kiss,
        });
    }
    if !perforation.is_empty() {
        phases.push(CutPhase {
            mode: CutMode::Perforation,
            pressure: perforation_pressure,
            paths: perforation,
        });
    }
    phases
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(y: f32) -> LineString<f32> {
        LineString::from(vec![(0.0, y), (20.0, y)])
    }

    #[test]
    fn plans_kiss_before_perforation_and_uses_individual_pressures() {
        let phases = plan_cut_phases(
            &[line(0.0), line(1.0)],
            &[CutMode::Perforation, CutMode::Kiss],
            42,
            53,
            5.0,
            2.0,
        );
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].mode, CutMode::Kiss);
        assert_eq!(phases[0].pressure, 42);
        assert_eq!(phases[0].paths, vec![line(1.0)]);
        assert_eq!(phases[1].mode, CutMode::Perforation);
        assert_eq!(phases[1].pressure, 53);
        assert!(phases[1].paths.len() > 1);
    }

    #[test]
    fn defaults_unassigned_paths_to_kiss_and_omits_disabled() {
        let phases = plan_cut_phases(
            &[line(0.0), line(1.0)],
            &[CutMode::Disabled],
            40,
            50,
            5.0,
            2.0,
        );
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].mode, CutMode::Kiss);
        assert_eq!(phases[0].paths, vec![line(1.0)]);
    }

    #[test]
    fn empty_and_degenerate_paths_create_no_phases() {
        assert!(plan_cut_phases(&[], &[], 42, 53, 5.0, 2.0).is_empty());
        let phases = plan_cut_phases(&[LineString::from(vec![(1.0, 1.0)])], &[], 42, 53, 5.0, 2.0);
        assert!(phases.is_empty());
    }

    #[test]
    fn global_perforation_preserves_disabled_paths() {
        assert_eq!(
            effective_cut_modes(
                3,
                &[CutMode::Kiss, CutMode::Disabled, CutMode::Perforation],
                true,
            ),
            [
                CutMode::Perforation,
                CutMode::Disabled,
                CutMode::Perforation
            ]
        );
    }
}
