//! Opt-in WebView extensions using Native Shell's public native-component API.
//!
//! Register the platform adapter before loading an app, then create [`web_view`].
//! The extension owns its browser view; it does not patch NSH's built-in WebView.
//! Native estimates remain estimates, and an unavailable estimate is [`None`].

use std::fmt;

use nsh_core::element::{
    AnyElement, ElNode, Element, IntoElement, IntoReactive, NativeEvent, NativeView, Reactive,
    native,
};
use nsh_core::{JsonValue, geometry::Percentage};
use serde::{Deserialize, Serialize};

/// The native component name registered by every adapter.
pub const COMPONENT_NAME: &str = "nsh-ext.webview";
/// The native event shared by the Rust wrapper and Java/Swift embedding hosts.
pub const PROGRESS_EVENT: &str = "loadProgress";

/// A native browser's loading lifecycle, separate from its estimated fraction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LoadState {
    /// A top-level navigation started; no estimate is known yet.
    #[default]
    Started,
    /// The engine reported a new estimate, not a success indication.
    Loading,
    /// The engine confirmed successful completion.
    Finished,
    /// The navigation failed.
    Failed,
    /// The host stopped or replaced a navigation.
    Cancelled,
}

/// One navigation's state and its optional, native estimated completion.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LoadProgress {
    /// A per-view navigation identity, beginning at one.
    pub navigation: u64,
    /// The lifecycle state. An estimate of one alone does not imply success.
    pub state: LoadState,
    /// The native estimate in the inclusive range zero to one; unknown stays `None`.
    pub progress: Option<Percentage>,
    /// A diagnostic for a failed navigation.
    pub message: Option<String>,
}

/// Invalid data received from a native progress adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The payload does not have the documented event shape.
    Payload,
    /// A navigation identity is zero or cannot be represented exactly in JavaScript.
    Navigation,
    /// The supplied estimate is outside the allowed finite range.
    Fraction,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Payload => "invalid native load-progress payload",
            Self::Navigation => "invalid native navigation identity",
            Self::Fraction => "native progress must be a finite fraction from zero to one",
        })
    }
}
impl std::error::Error for DecodeError {}

impl LoadProgress {
    /// Decodes a native component event without conflating unknown progress with zero.
    pub fn from_value(value: &JsonValue) -> Result<Self, DecodeError> {
        #[derive(Deserialize)]
        struct Wire {
            navigation: u64,
            state: LoadState,
            progress: Option<f64>,
            message: Option<String>,
        }
        let wire = Wire::deserialize(value).map_err(|_| DecodeError::Payload)?;
        if wire.navigation == 0 || wire.navigation > 9_007_199_254_740_991 {
            return Err(DecodeError::Navigation);
        }
        if wire
            .progress
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(DecodeError::Fraction);
        }
        Ok(Self {
            navigation: wire.navigation,
            state: wire.state,
            progress: wire.progress.map(|value| Percentage(value as f32)),
            message: wire.message,
        })
    }

    /// Encodes the event for `ComponentContext::emit(PROGRESS_EVENT, ...)`.
    pub fn to_value(&self) -> JsonValue {
        serde_json::json!({
            "navigation": self.navigation,
            "state": self.state,
            "progress": self.progress.map(|value| value.0),
            "message": self.message,
        })
    }
}

/// A WebView rendered by the adapter registered as [`COMPONENT_NAME`].
///
/// Standard NSH layout, styling and element-reference builders are available.
/// Native components are trusted host code: the embedding application decides which
/// URLs are allowed and whether untrusted modules may use this component.
#[non_exhaustive]
pub struct WebView(NativeView);

/// Creates an extension WebView. Set [`WebView::src`] before displaying it.
pub fn web_view() -> WebView {
    WebView(native(COMPONENT_NAME))
}

impl Default for WebView {
    fn default() -> Self {
        web_view()
    }
}

impl WebView {
    /// Loads a URL, or a reactive URL that navigates again when it changes.
    /// An empty source stops the current load and clears the page.
    pub fn src(mut self, source: impl IntoReactive<String>) -> Self {
        self.0 = match source.into_reactive() {
            Reactive::Static(value) => self.0.prop("src", JsonValue::String(value)),
            Reactive::Bound(read) => self.0.prop("src", move || JsonValue::String(read())),
        };
        self
    }

    /// Receives native lifecycle/estimate events. The adapter subscribes on demand.
    pub fn on_load_progress(mut self, listener: impl Fn(&LoadProgress) + 'static) -> Self {
        self.0 = self
            .0
            .prop("trackProgress", JsonValue::Bool(true))
            .on_native_event(move |event: &NativeEvent| {
                if event.name.as_ref() == PROGRESS_EVENT {
                    match LoadProgress::from_value(&event.data) {
                        Ok(progress) => listener(&progress),
                        Err(error) => log::warn!("{error}"),
                    }
                }
            });
        self
    }
}

impl IntoElement for WebView {
    fn into_element(self) -> AnyElement {
        self.0.into_element()
    }
}
impl Element for WebView {
    fn node(&mut self) -> &mut ElNode {
        self.0.node()
    }
}

/// Shared lifecycle bookkeeping for native adapters; no timers or invented estimates.
///
/// Adapters must correlate their own native callbacks with the returned navigation
/// identity. A plain counter cannot identify a stale callback that carries no native
/// identity; those callbacks require backend-specific lifetime/navigation guards.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct ProgressTracker {
    navigation: u64,
    active: bool,
    last: Option<Percentage>,
}

impl ProgressTracker {
    /// Begins a navigation, invalidating every earlier navigation identity.
    pub fn start(&mut self) -> LoadProgress {
        self.navigation = self
            .navigation
            .checked_add(1)
            .expect("navigation counter exhausted");
        assert!(
            self.navigation <= 9_007_199_254_740_991,
            "navigation identity exhausted"
        );
        self.active = true;
        self.last = None;
        self.event(LoadState::Started, None, None)
    }

    /// The current identity, for association with the backend's native navigation.
    pub fn navigation(&self) -> u64 {
        self.navigation
    }

    /// Whether the given navigation is still loading.
    pub fn is_active(&self, navigation: u64) -> bool {
        self.active && self.navigation == navigation
    }

    /// Reports a changed native fraction for the current navigation.
    pub fn estimate(&mut self, navigation: u64, value: Percentage) -> Option<LoadProgress> {
        if !self.is_active(navigation)
            || !value.0.is_finite()
            || !(0.0..=1.0).contains(&value.0)
            || self.last == Some(value)
        {
            return None;
        }
        self.last = Some(value);
        Some(self.event(LoadState::Loading, Some(value), None))
    }

    /// Reports confirmed success, ignoring a late or duplicate completion.
    pub fn finish(&mut self, navigation: u64) -> Option<LoadProgress> {
        self.end(navigation, LoadState::Finished, Some(Percentage(1.0)), None)
    }

    /// Reports a genuine native failure without synthesizing successful completion.
    pub fn fail(&mut self, navigation: u64, message: String) -> Option<LoadProgress> {
        self.end(navigation, LoadState::Failed, None, Some(message))
    }

    /// Reports an explicit stop or source replacement.
    pub fn cancel(&mut self, navigation: u64) -> Option<LoadProgress> {
        self.end(navigation, LoadState::Cancelled, None, None)
    }

    fn end(
        &mut self,
        navigation: u64,
        state: LoadState,
        progress: Option<Percentage>,
        message: Option<String>,
    ) -> Option<LoadProgress> {
        if !self.is_active(navigation) {
            return None;
        }
        self.active = false;
        Some(self.event(state, progress, message))
    }

    fn event(
        &self,
        state: LoadState,
        progress: Option<Percentage>,
        message: Option<String>,
    ) -> LoadProgress {
        LoadProgress {
            navigation: self.navigation,
            state,
            progress,
            message,
        }
    }
}
