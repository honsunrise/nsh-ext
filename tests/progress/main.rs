//! Consumer-visible progress, navigation identity and terminal-state regressions.

use nsh_core::geometry::Percentage;
use nsh_ext::{DecodeError, LoadProgress, LoadState, ProgressTracker};
use serde_json::json;

#[test]
fn unknown_estimate_is_not_zero() {
    let unknown = LoadProgress::from_value(&json!({
        "navigation": 1, "state": "started", "progress": null
    }))
    .unwrap();
    let zero = LoadProgress::from_value(&json!({
        "navigation": 1, "state": "loading", "progress": 0
    }))
    .unwrap();
    assert_eq!(unknown.progress, None);
    assert_eq!(zero.progress, Some(Percentage(0.0)));
}

#[test]
fn unrepresentable_native_values_are_rejected() {
    for fraction in [json!(-0.01), json!(1.01), json!("unknown")] {
        assert!(
            LoadProgress::from_value(&json!({
                "navigation": 1, "state": "loading", "progress": fraction
            }))
            .is_err()
        );
    }
    for navigation in [0, 9_007_199_254_740_992_u64] {
        assert_eq!(
            LoadProgress::from_value(&json!({
                "navigation": navigation, "state": "started", "progress": null
            }))
            .unwrap_err(),
            DecodeError::Navigation
        );
    }
    let boundary = LoadProgress::from_value(&json!({
        "navigation": 9_007_199_254_740_991_u64,
        "state": "loading", "progress": 1
    }))
    .unwrap();
    assert_eq!(boundary.progress, Some(Percentage(1.0)));
}

#[test]
fn an_estimate_of_one_does_not_turn_a_failed_load_into_success() {
    let mut tracker = ProgressTracker::default();
    let navigation = tracker.start().navigation;
    let estimate = tracker.estimate(navigation, Percentage(1.0)).unwrap();
    assert_eq!(estimate.state, LoadState::Loading);
    assert!(tracker.is_active(navigation));
    let failed = tracker.fail(navigation, "connection reset".into()).unwrap();
    assert_eq!(failed.state, LoadState::Failed);
    assert_eq!(failed.progress, None);
    assert_eq!(failed.message.as_deref(), Some("connection reset"));
    assert!(tracker.finish(navigation).is_none());
    assert!(tracker.estimate(navigation, Percentage(1.0)).is_none());
}

#[test]
fn replacing_a_navigation_rejects_old_completions_and_estimates() {
    let mut tracker = ProgressTracker::default();
    let old = tracker.start().navigation;
    tracker.estimate(old, Percentage(0.4)).unwrap();
    let current = tracker.start();
    assert_ne!(current.navigation, old);
    assert_eq!(current.state, LoadState::Started);
    assert_eq!(current.progress, None);
    assert!(tracker.finish(old).is_none());
    assert!(tracker.fail(old, "old failure".into()).is_none());
    assert!(tracker.estimate(old, Percentage(0.9)).is_none());
    let finished = tracker.finish(current.navigation).unwrap();
    assert_eq!(finished.state, LoadState::Finished);
    assert_eq!(finished.progress, Some(Percentage(1.0)));
    assert!(tracker.finish(current.navigation).is_none());
}

#[test]
fn native_estimates_can_change_backwards_but_duplicates_are_not_emitted() {
    let mut tracker = ProgressTracker::default();
    let navigation = tracker.start().navigation;
    assert_eq!(
        tracker
            .estimate(navigation, Percentage(0.8))
            .unwrap()
            .progress,
        Some(Percentage(0.8))
    );
    assert!(tracker.estimate(navigation, Percentage(0.8)).is_none());
    assert_eq!(
        tracker
            .estimate(navigation, Percentage(0.6))
            .unwrap()
            .progress,
        Some(Percentage(0.6))
    );
    assert!(tracker.estimate(navigation, Percentage(f32::NAN)).is_none());
    assert!(
        tracker
            .estimate(navigation, Percentage(f32::INFINITY))
            .is_none()
    );
    assert!(tracker.estimate(navigation, Percentage(-0.1)).is_none());
}

#[test]
fn cancellation_is_terminal_for_that_navigation_only() {
    let mut tracker = ProgressTracker::default();
    let old = tracker.start().navigation;
    assert_eq!(tracker.cancel(old).unwrap().state, LoadState::Cancelled);
    assert!(tracker.finish(old).is_none());
    assert!(tracker.cancel(old).is_none());
    let new = tracker.start().navigation;
    assert!(tracker.is_active(new));
    assert!(tracker.cancel(old).is_none());
    assert_eq!(tracker.finish(new).unwrap().state, LoadState::Finished);
}
