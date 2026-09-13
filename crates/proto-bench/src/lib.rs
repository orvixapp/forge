use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkResult {
    pub scenario: String,
    pub iterations: usize,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub max_pss_kib: Option<u64>,
    pub frames: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScenarioThreshold {
    pub p95_ms: Option<f64>,
    pub max_pss_kib: Option<u64>,
    pub max_frames: Option<u64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Thresholds {
    #[serde(flatten)]
    pub scenarios: HashMap<String, ScenarioThreshold>,
}

impl Thresholds {
    #[must_use]
    pub fn violations(&self, result: &BenchmarkResult) -> Vec<String> {
        let Some(limit) = self.scenarios.get(&result.scenario) else {
            return vec![format!("no thresholds configured for {}", result.scenario)];
        };
        let mut violations = Vec::new();
        if limit.p95_ms.is_some_and(|maximum| result.p95_ms > maximum) {
            violations.push(format!("p95 {:.3} ms exceeds limit", result.p95_ms));
        }
        if let (Some(actual), Some(maximum)) = (result.max_pss_kib, limit.max_pss_kib)
            && actual > maximum
        {
            violations.push(format!("PSS {actual} KiB exceeds {maximum} KiB"));
        }
        if let (Some(actual), Some(maximum)) = (result.frames, limit.max_frames)
            && actual > maximum
        {
            violations.push(format!("{actual} idle frames exceeds {maximum}"));
        }
        violations
    }
}

#[must_use]
pub fn summarize(
    scenario: impl Into<String>,
    mut samples_ms: Vec<f64>,
    pss_samples: &[u64],
    frames: Option<u64>,
) -> BenchmarkResult {
    samples_ms.sort_by(f64::total_cmp);
    BenchmarkResult {
        scenario: scenario.into(),
        iterations: samples_ms.len(),
        median_ms: percentile(&samples_ms, 50),
        p95_ms: percentile(&samples_ms, 95),
        max_pss_kib: pss_samples.iter().copied().max(),
        frames,
    }
}

#[must_use]
pub fn percentile(sorted: &[f64], percentile: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = (sorted.len() - 1).saturating_mul(percentile).div_ceil(100);
    sorted[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank_and_handles_empty_input() {
        assert!(percentile(&[], 95).abs() < f64::EPSILON);
        assert!((percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 50) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 95) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summary_sorts_samples_and_tracks_peak_memory() {
        let result = summarize(
            "startup_empty",
            vec![30.0, 10.0, 20.0],
            &[100, 300, 200],
            None,
        );
        assert_eq!(result.iterations, 3);
        assert!((result.median_ms - 20.0).abs() < f64::EPSILON);
        assert!((result.p95_ms - 30.0).abs() < f64::EPSILON);
        assert_eq!(result.max_pss_kib, Some(300));
    }

    #[test]
    fn threshold_check_reports_every_exceeded_budget() {
        let thresholds: Thresholds =
            toml::from_str("[startup_empty]\np95_ms = 25.0\nmax_pss_kib = 200\nmax_frames = 0\n")
                .unwrap();
        let result = summarize("startup_empty", vec![30.0], &[300], Some(1));
        let violations = thresholds.violations(&result);
        assert_eq!(violations.len(), 3);
    }

    #[test]
    fn threshold_check_accepts_results_within_budget() {
        let thresholds: Thresholds = toml::from_str("[ipc_round_trip]\np95_ms = 1.0\n").unwrap();
        let result = summarize("ipc_round_trip", vec![0.5], &[], None);
        assert!(thresholds.violations(&result).is_empty());
    }
}
