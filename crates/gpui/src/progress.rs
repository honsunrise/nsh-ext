//! The native progress sink shared by the three backends.
//!
//! Native engines report navigations through callbacks that carry whatever
//! identity the engine has (WebView2's navigation ids, WKWebView's
//! `WKNavigation` objects, WebKitGTK's ordered per-view signal sequence). Each
//! backend checks that identity itself and reports only the lifecycle of the
//! navigation that identity names; this sink maps those reports onto one
//! [`ProgressTracker`]:
//!
//! - a native start begins a new navigation, cancelling the one it replaced;
//! - estimates belong to the current navigation only;
//! - success and failure come from the engine's native completion and failure
//!   callbacks, never from an estimate or from a frame;
//! - after a navigation ended, a late report of it finds no active navigation
//!   and is dropped;
//! - a requested load that never reached the engine is reported as a real
//!   navigation that started and failed under one identity.
//!
//! The lifecycle is always tracked, so a listener added during a load sees it.
//! [`LoadProgress`] payloads are built and emitted only while the app listens
//! (`trackProgress`): the events of one native signal are collected in a fixed
//! buffer, and the failure message is only formatted when it is needed, so an
//! unobserved navigation allocates nothing per callback.
//!
//! The sink keeps the component's [`ComponentContext`], which is how it sends
//! `loadProgress` events. The component owns the sink, so this is not a cycle.

use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(not(windows))]
use nsh_core::geometry::Percentage;
use nsh_ext::{LoadProgress, PROGRESS_EVENT, ProgressTracker};
use nsh_platform_gpui::ComponentContext;

/// The most events one native signal produces: the navigation it replaced is
/// cancelled, the new one starts, and a request that never reached the engine
/// fails under that same identity.
const MAX_EVENTS: usize = 3;

/// One web view's lifecycle and the app's listener.
pub(crate) struct Sink {
    context: ComponentContext,
    state: Arc<Mutex<State>>,
    generation: u64,
}

#[derive(Default)]
struct State {
    tracker: ProgressTracker,
    /// Whether the app wants progress events (`trackProgress`).
    listening: bool,
    generation: u64,
}

/// The events of one native signal, collected without allocating.
#[derive(Default)]
struct Events {
    items: [Option<LoadProgress>; MAX_EVENTS],
    len: usize,
}

impl Events {
    /// Records `event` when the app listens; an unobserved event is dropped.
    fn push(&mut self, event: Option<LoadProgress>, listening: bool) {
        if !listening {
            return;
        }
        let Some(event) = event else {
            return;
        };
        match self.items.get_mut(self.len) {
            Some(slot) => {
                *slot = Some(event);
                self.len += 1;
            }
            None => debug_assert!(
                false,
                "one native signal produced more than {MAX_EVENTS} events"
            ),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &LoadProgress> {
        self.items[..self.len].iter().filter_map(Option::as_ref)
    }
}

impl Sink {
    pub(crate) fn new(context: ComponentContext) -> Self {
        Self {
            context,
            state: Arc::new(Mutex::new(State::default())),
            generation: 0,
        }
    }

    /// Retires callbacks of the previous browser while preserving component
    /// navigation numbering. Explicit source changes create a new browser.
    pub(crate) fn next_view(&self) -> Self {
        self.cancel();
        let generation = {
            let mut state = self.lock();
            state.generation = state
                .generation
                .checked_add(1)
                .expect("browser generation exhausted");
            state.generation
        };
        Self {
            context: self.context.clone(),
            state: self.state.clone(),
            generation,
        }
    }

    /// Disposal is silent; queued native callbacks can no longer emit.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        if state.generation == self.generation {
            state.generation = state
                .generation
                .checked_add(1)
                .expect("browser generation exhausted");
            let navigation = state.tracker.navigation();
            let _ = state.tracker.cancel(navigation);
        }
    }

    /// Subscribes (or unsubscribes) the app's `loadProgress` listener.
    pub(crate) fn set_listening(&self, listening: bool) {
        self.lock().listening = listening;
    }

    /// A native navigation started (a page load, a link, a history move).
    pub(crate) fn started(&self) {
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            let listening = state.listening;
            let current = state.tracker.navigation();
            if listening && state.tracker.is_active(current) {
                // The new native navigation replaced this one.
                events.push(state.tracker.cancel(current), listening);
            }
            events.push(Some(state.tracker.start()), listening);
        }
        self.emit(&events);
    }

    #[cfg(not(windows))]
    /// The engine has a new estimate for the current navigation.
    pub(crate) fn estimate(&self, fraction: f64) {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return;
        }
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            if !state.listening {
                return;
            }
            let navigation = state.tracker.navigation();
            events.push(
                state
                    .tracker
                    .estimate(navigation, Percentage(fraction as f32)),
                true,
            );
        }
        self.emit(&events);
    }

    /// The engine confirmed that the current navigation completed.
    pub(crate) fn finished(&self) {
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            let listening = state.listening;
            let navigation = state.tracker.navigation();
            events.push(state.tracker.finish(navigation), listening);
        }
        self.emit(&events);
    }

    /// The engine reported a failed navigation; `message` is only asked for
    /// when the app is listening.
    pub(crate) fn failed(&self, message: impl FnOnce() -> String) {
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            let listening = state.listening;
            let navigation = state.tracker.navigation();
            if state.tracker.is_active(navigation) {
                let message = if listening { message() } else { String::new() };
                events.push(state.tracker.fail(navigation, message), listening);
            }
        }
        self.emit(&events);
    }

    /// The engine stopped the current navigation at the host's or the page's
    /// request (a replaced load, a download, a stopped provisional load).
    pub(crate) fn cancel(&self) {
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            let listening = state.listening;
            let navigation = state.tracker.navigation();
            events.push(state.tracker.cancel(navigation), listening);
        }
        self.emit(&events);
    }

    /// A requested load never reached the engine (an invalid URL, a web view
    /// that could not be created): it starts and fails under one identity, so
    /// the app sees the same started-then-failed pair a native rejection gives.
    pub(crate) fn request_failed(&self, message: String) {
        let mut events = Events::default();
        {
            let mut state = self.lock();
            if state.generation != self.generation {
                return;
            }
            let listening = state.listening;
            let current = state.tracker.navigation();
            if listening && state.tracker.is_active(current) {
                events.push(state.tracker.cancel(current), listening);
            }
            let started = state.tracker.start();
            let navigation = started.navigation;
            events.push(Some(started), listening);
            events.push(state.tracker.fail(navigation, message), listening);
        }
        self.emit(&events);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Sends the events collected for one native signal. Nothing reaches the
    /// app unless it listens, and nothing is converted to a payload otherwise.
    fn emit(&self, events: &Events) {
        {
            let state = self.lock();
            if state.generation != self.generation || !state.listening {
                return;
            }
        }
        for event in events.iter() {
            self.context.emit(PROGRESS_EVENT, event.to_value());
        }
    }
}
