//! The typed component TSX imports from `nsh-ext`.
//!
//! It is a thin typed face over the same `nsh-ext.webview` native component
//! [`web_view`](crate::web_view) makes: its props follow the native protocol,
//! and its events reuse [`LoadProgress`]'s wire validation.

use nsh_core::element::{Event, IntoElement, NativeEvent, Prop, Props, Scope, component, native};
use nsh_core::{JsonValue, geometry::Percentage};

use super::{COMPONENT_NAME, LoadProgress, LoadState, PROGRESS_EVENT};

/// The props of the typed [`WebView`] component.
#[derive(Debug, Clone, PartialEq, Props)]
pub(crate) struct WebViewProps {
    /// The page URL. An empty source stops the current load and clears the page.
    pub src: Prop<String>,
    /// Subscribe to `onLoadProgress`. Defaults to `false`.
    #[prop(default)]
    pub track_progress: Prop<bool>,
}

/// The events of the typed [`WebView`] component.
#[derive(Debug, Clone, PartialEq, Event)]
pub(crate) enum WebViewEvent {
    /// A native navigation's lifecycle and optional estimate.
    LoadProgress {
        /// The per-view navigation identity.
        navigation: u64,
        /// The navigation's lifecycle state.
        state: LoadState,
        /// The native estimate, or `null` when unknown.
        progress: Option<Percentage>,
        /// A failure's diagnostic, or `null`.
        message: Option<String>,
    },
}

/// A WebView from the host adapter registered as `nsh-ext.webview`.
///
/// `src` is live: changing it navigates the existing view. `trackProgress`
/// opts into native load events; `onLoadProgress` receives each decoded
/// event, with an unknown estimate and an absent diagnostic kept as `null`.
/// The Rust [`web_view`](crate::web_view) keeps its existing builder and
/// typed [`LoadProgress`] callback.
#[component]
pub(crate) fn WebView(props: WebViewProps, cx: Scope<WebViewEvent>) -> impl IntoElement {
    let src = props.src;
    let track_progress = props.track_progress;
    native(COMPONENT_NAME)
        .prop("src", move || JsonValue::String(src.get()))
        .prop("trackProgress", move || {
            JsonValue::Bool(track_progress.get())
        })
        .on_native_event(move |event: &NativeEvent| {
            if event.name.as_ref() == PROGRESS_EVENT {
                match LoadProgress::from_value(&event.data) {
                    Ok(progress) => cx.emit(WebViewEvent::LoadProgress {
                        navigation: progress.navigation,
                        state: progress.state,
                        progress: progress.progress,
                        message: progress.message,
                    }),
                    Err(error) => log::warn!("{error}"),
                }
            }
        })
}
