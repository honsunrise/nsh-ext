//! The adapter's own bookkeeping around [`nsh_ext::ProgressTracker`].
//!
//! WebKit identifies a navigation with its `WKNavigation` object; that
//! object's address is this adapter's native navigation key within one
//! browser generation. [`crate::webview`] creates a fresh `WKWebView` (and a
//! fresh callback generation) for every explicit `src` change, so a callback
//! from a discarded browser can never be judged against the replacement's
//! keys. Inside one browser every callback that carries a navigation is
//! checked against the key that is authoritative now, so a late callback from
//! a replaced navigation changes nothing. Callbacks without a navigation
//! (`estimatedProgress` KVO and web content process termination) are guarded
//! by the browser generation and web view in [`crate::webview`].
//!
//! `trackProgress` is only an emission gate. Every native navigation advances
//! [`ProgressTracker`] even while it is off, so identities stay consistent;
//! turning tracking on mid-load never invents a start event. The identity
//! counter belongs to the component: it continues across `src` changes and
//! starts at one again when the component is created again.

use nsh_core::geometry::Percentage;
#[cfg(test)]
use nsh_ext::LoadState;
use nsh_ext::{LoadProgress, ProgressTracker};

/// At most a cancelled navigation and a started one are reported together.
const EVENTS: usize = 2;

/// A small, allocation-free batch of progress events.
///
/// `trackProgress` gates payload construction entirely: when it is off the
/// batch stays empty and no [`LoadProgress`] is emitted.
#[derive(Default)]
pub(crate) struct Events {
    items: [Option<LoadProgress>; EVENTS],
    len: usize,
}

impl Events {
    fn push(&mut self, event: Option<LoadProgress>) {
        if let Some(event) = event
            && self.len < EVENTS
        {
            self.items[self.len] = Some(event);
            self.len += 1;
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &LoadProgress> {
        self.items.iter().take(self.len).filter_map(Option::as_ref)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Correlates WebKit navigations with the extension's navigation identities.
///
/// `current` is the `WKNavigation` that is authoritative in this browser
/// generation. `loading` tracks whether that navigation is still in flight.
/// The native key space is per browser: a source change destroys the
/// `WKWebView` and calls [`Self::replaced`], so no later callback of the
/// discarded browser can become current.
#[derive(Debug, Default)]
pub(crate) struct NavigationTracker {
    tracker: ProgressTracker,
    current: Option<usize>,
    loading: bool,
    track: bool,
}

impl NavigationTracker {
    /// Enables or disables event emission. Identities advance either way;
    /// enabling mid-load only means the next native estimate, finish or
    /// failure is reported. No `Started` is synthesized.
    pub(crate) fn set_track(&mut self, enabled: bool) {
        self.track = enabled;
    }

    /// Whether `trackProgress` is on: callers avoid building messages and
    /// payloads when it is off.
    pub(crate) fn track_enabled(&self) -> bool {
        self.track
    }

    /// A top-level `WKNavigation` began.
    pub(crate) fn started(&mut self, key: usize) -> Events {
        let mut events = Events::default();

        if self.current == Some(key) {
            // WebKit can repeat a start for the same navigation (a
            // redirect keeps its identity); it is not a new load.
            self.loading = true;
            return events;
        }

        // WebKit may start a new navigation before the old one reports a
        // failure. That is a replacement, not a failure of the new one.
        if self.active() {
            let previous = self.tracker.navigation();
            let cancelled = self.tracker.cancel(previous);
            if self.track {
                events.push(cancelled);
            }
        }

        self.current = Some(key);
        self.loading = true;
        // Always track, even when events are off: the identity must not
        // change when tracking is enabled later.
        let started = self.tracker.start();
        if self.track {
            events.push(Some(started));
        }
        events
    }

    /// The native fraction WebKit measured for the current navigation.
    pub(crate) fn estimate(&mut self, fraction: f64) -> Option<LoadProgress> {
        if !self.loading || self.current.is_none() || !fraction.is_finite() {
            return None;
        }
        let navigation = self.tracker.navigation();
        let event = self
            .tracker
            .estimate(navigation, Percentage(fraction as f32));
        if self.track { event } else { None }
    }

    /// `webView:didFinishNavigation:`: WebKit confirmed success. An HTTP
    /// error document (a 404 or 500 page) finishes like any other document;
    /// only a native load failure is reported as failed.
    pub(crate) fn finished(&mut self, key: usize) -> Option<LoadProgress> {
        self.end(key, |tracker, navigation| tracker.finish(navigation))
    }

    /// A genuine native failure.
    pub(crate) fn failed(&mut self, key: usize, message: String) -> Option<LoadProgress> {
        self.end(key, |tracker, navigation| tracker.fail(navigation, message))
    }

    /// WebKit cancelled the navigation (`NSURLErrorCancelled`), or the page
    /// stopped it.
    pub(crate) fn cancelled(&mut self, key: usize) -> Option<LoadProgress> {
        self.end(key, |tracker, navigation| tracker.cancel(navigation))
    }

    /// The web content process died, which has no `WKNavigation` argument:
    /// its native guard is the per-browser handler, and the failure applies
    /// to whichever navigation is current.
    pub(crate) fn terminated(&mut self) -> Option<LoadProgress> {
        let key = self.current?;
        self.failed(key, "the web content process terminated".to_owned())
    }

    /// The host replaced or cleared `src`, stopping the navigation in
    /// flight. Returns the cancellation to report before the browser is
    /// discarded. The native binding is cleared: the caller destroys the
    /// browser whose keys produced it.
    pub(crate) fn replaced(&mut self) -> Option<LoadProgress> {
        let event = if self.active() {
            let navigation = self.tracker.navigation();
            self.tracker.cancel(navigation)
        } else {
            None
        };
        self.current = None;
        self.loading = false;
        if self.track { event } else { None }
    }

    /// A request that never became a native navigation (an invalid URL, an
    /// unavailable WebKit, or `loadRequest:` returning nil) still has an
    /// honest lifecycle: started, then failed. The identity advances even
    /// when emission is off.
    pub(crate) fn failed_to_start(&mut self, message: impl FnOnce() -> String) -> Events {
        let mut events = Events::default();
        self.cancel_active_silently();
        self.current = None;
        self.loading = false;
        let started = self.tracker.start();
        let navigation = started.navigation;
        if !self.track {
            let _ = self.tracker.fail(navigation, String::new());
            return events;
        }
        events.push(Some(started));
        events.push(self.tracker.fail(navigation, message()));
        events
    }

    fn end(
        &mut self,
        key: usize,
        finish: impl FnOnce(&mut ProgressTracker, u64) -> Option<LoadProgress>,
    ) -> Option<LoadProgress> {
        if !self.loading || self.current != Some(key) {
            return None;
        }
        self.current = None;
        self.loading = false;
        if !self.active() {
            return None;
        }
        let navigation = self.tracker.navigation();
        let event = finish(&mut self.tracker, navigation);
        if self.track { event } else { None }
    }

    fn active(&self) -> bool {
        self.tracker.is_active(self.tracker.navigation())
    }

    fn cancel_active_silently(&mut self) {
        if self.active() {
            let navigation = self.tracker.navigation();
            let _ = self.tracker.cancel(navigation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(events: &Events) -> Vec<LoadState> {
        events.iter().map(|event| event.state).collect()
    }

    #[test]
    fn a_navigation_is_started_estimated_and_finished() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);

        let started = tracker.started(0x100);
        assert_eq!(states(&started), vec![LoadState::Started]);
        assert_eq!(started.iter().next().unwrap().navigation, 1);
        assert_eq!(started.iter().next().unwrap().progress, None);

        let estimate = tracker.estimate(0.25).expect("a native estimate");
        assert_eq!(estimate.state, LoadState::Loading);
        assert_eq!(estimate.navigation, 1);
        assert_eq!(estimate.progress.map(|value| value.0), Some(0.25));
        assert!(tracker.estimate(0.25).is_none(), "duplicates are dropped");

        let finished = tracker.finished(0x100).expect("success");
        assert_eq!(finished.state, LoadState::Finished);
        assert_eq!(finished.progress.map(|value| value.0), Some(1.0));
        assert!(tracker.finished(0x100).is_none());
    }

    #[test]
    fn zero_is_a_real_estimate_and_one_is_not_success() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);

        let zero = tracker.estimate(0.0).expect("zero is a native estimate");
        assert_eq!(zero.state, LoadState::Loading);
        assert_eq!(zero.progress.map(|value| value.0), Some(0.0));
        assert_eq!(tracker.estimate(0.0), None, "duplicates are dropped");

        let one = tracker.estimate(1.0).expect("one is still an estimate");
        assert_eq!(one.state, LoadState::Loading);
        assert_eq!(one.progress.map(|value| value.0), Some(1.0));
        assert_eq!(tracker.finished(0x100).unwrap().state, LoadState::Finished);
    }

    #[test]
    fn a_duplicate_start_keeps_the_identity() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);

        assert!(tracker.started(0x100).is_empty(), "no second identity");
        assert_eq!(tracker.estimate(0.3).unwrap().navigation, 1);
    }

    #[test]
    fn a_replacement_cancels_the_old_navigation_and_stale_callbacks_do_nothing() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);
        tracker.estimate(0.4);

        let events = tracker.started(0x200);
        assert_eq!(
            states(&events),
            vec![LoadState::Cancelled, LoadState::Started]
        );
        let identities: Vec<_> = events.iter().map(|event| event.navigation).collect();
        assert_eq!(identities, vec![1, 2]);

        assert!(
            tracker.finished(0x100).is_none(),
            "the old navigation is stale"
        );
        assert!(tracker.failed(0x100, "late".into()).is_none());
        assert_eq!(tracker.estimate(0.7).unwrap().navigation, 2);

        let failed = tracker.failed(0x200, "offline".into()).expect("failure");
        assert_eq!(failed.state, LoadState::Failed);
        assert_eq!(failed.progress, None);
        assert_eq!(failed.message.as_deref(), Some("offline"));
    }

    #[test]
    fn a_source_change_clears_the_native_binding_but_keeps_the_counter() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);

        let cancelled = tracker.replaced().expect("the active load is cancelled");
        assert_eq!(cancelled.state, LoadState::Cancelled);
        assert_eq!(cancelled.navigation, 1);

        // The discarded browser's callbacks have no current navigation.
        assert!(tracker.finished(0x100).is_none());
        assert!(tracker.failed(0x100, "late".into()).is_none());
        assert!(tracker.estimate(0.9).is_none());

        let events = tracker.started(0x300);
        assert_eq!(states(&events), vec![LoadState::Started]);
        assert_eq!(
            events.iter().next().unwrap().navigation,
            2,
            "the component keeps counting across browsers"
        );
    }

    #[test]
    fn replacing_when_nothing_is_active_emits_nothing() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        assert_eq!(tracker.replaced(), None);

        tracker.started(0x100);
        tracker.finished(0x100);
        assert_eq!(tracker.replaced(), None);
    }

    #[test]
    fn identities_advance_while_progress_is_off() {
        let mut tracker = NavigationTracker::default();
        assert!(
            tracker.started(0x100).is_empty(),
            "off allocates no payload"
        );
        assert!(tracker.estimate(0.3).is_none());
        assert!(tracker.finished(0x100).is_none());

        tracker.set_track(true);
        let started = tracker.started(0x200);
        assert_eq!(states(&started), vec![LoadState::Started]);
        assert_eq!(
            started.iter().next().unwrap().navigation,
            2,
            "the off navigation still consumed identity 1"
        );
    }

    #[test]
    fn enabling_mid_load_does_not_synthesize_started() {
        let mut tracker = NavigationTracker::default();
        tracker.started(0x100);
        assert!(tracker.estimate(0.2).is_none());

        tracker.set_track(true);
        let estimate = tracker.estimate(0.6).expect("the real next estimate");
        assert_eq!(estimate.state, LoadState::Loading);
        assert_eq!(estimate.navigation, 1);
        assert_eq!(tracker.finished(0x100).unwrap().navigation, 1);
    }

    #[test]
    fn turning_progress_off_drops_callbacks_without_a_late_start() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);
        tracker.set_track(false);
        assert!(tracker.estimate(0.8).is_none());
        assert!(tracker.finished(0x100).is_none());
        tracker.set_track(true);
        assert!(tracker.estimate(0.9).is_none());
    }

    #[test]
    fn a_request_that_never_started_reports_started_then_failed() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        let events = tracker.failed_to_start(|| "invalid URL".into());
        assert_eq!(states(&events), vec![LoadState::Started, LoadState::Failed]);
        let failed = events.iter().last().unwrap();
        assert_eq!(failed.message.as_deref(), Some("invalid URL"));
        assert_eq!(failed.progress, None);
    }

    #[test]
    fn a_failed_start_advances_identity_even_when_off() {
        let mut tracker = NavigationTracker::default();
        assert!(tracker.failed_to_start(|| "unavailable".into()).is_empty());
        tracker.set_track(true);
        let started = tracker.started(0x100);
        assert_eq!(started.iter().next().unwrap().navigation, 2);
    }

    #[test]
    fn termination_fails_the_current_navigation_only() {
        let mut tracker = NavigationTracker::default();
        tracker.set_track(true);
        tracker.started(0x100);
        let failed = tracker.terminated().expect("failed");
        assert_eq!(failed.state, LoadState::Failed);
        assert_eq!(failed.navigation, 1);
        assert!(tracker.terminated().is_none());
    }
}
