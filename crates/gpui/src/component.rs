//! The `nsh-ext.webview` native component: an owned wry web view filling its
//! Native Shell container.
//!
//! Native Shell calls [`NativeComponent::render`] while prepainting (the only
//! place with the GPUI window the web view is built in) and
//! [`NativeComponent::placed`] with the container's window bounds after each
//! frame it draws. The web view is therefore:
//!
//! - built on the first frame after the `src` prop arrived, with no URL, and
//!   navigated once its native progress callbacks are installed (so the first
//!   navigation cannot outrun them);
//! - hidden until the first placement, then given the container's bounds in
//!   GPUI points, which wry converts with the window's own scale factor;
//! - re-placed when the bounds or the scale factor change, and hidden when the
//!   container is not drawn (gone, hidden, or empty);
//! - retired on explicit source changes before the new browser is created,
//!   fencing old callbacks; dropped silently with the component.

use std::sync::Arc;

use nsh_core::JsonValue;
use nsh_platform_gpui::{
    ComponentContext, NativeComponent,
    gpui::{
        AnyElement, App, Bounds, IntoElement, ParentElement, Pixels, Styled, Window, div, px, rgb,
    },
};
use wry::WebViewBuilder;

use crate::progress::Sink;

#[cfg(target_os = "linux")]
use crate::linux as backend;
#[cfg(target_os = "macos")]
use crate::macos as backend;
#[cfg(windows)]
use crate::windows as backend;

/// One `nsh-ext.webview` element's web view.
pub(crate) struct WebViewComponent {
    /// Events and the app's listener; it holds the component's context.
    sink: Arc<Sink>,
    /// The source last asked for; `None` is an empty or removed `src`.
    source: Option<String>,
    view: Option<backend::View>,
    /// Why no web view can be shown (drawn instead of the page).
    error: Option<String>,
    /// The window's scale factor at the last frame.
    scale: f32,
    /// The bounds the native view was last placed at, in GPUI points.
    placed: Option<(f32, f32, f32, f32)>,
}

impl WebViewComponent {
    pub(crate) fn new(context: ComponentContext) -> Self {
        Self {
            sink: Arc::new(Sink::new(context)),
            source: None,
            view: None,
            error: None,
            scale: 1.0,
            placed: None,
        }
    }

    /// Reads a `src` prop: a string, empty when it was removed or empty.
    fn source_of(value: Option<&JsonValue>) -> Option<String> {
        match value? {
            JsonValue::String(source) if !source.is_empty() => Some(source.clone()),
            JsonValue::String(_) | JsonValue::Null => None,
            _ => {
                log::warn!("nsh-ext webview: `src` must be a string; ignoring it");
                None
            }
        }
    }

    /// Navigates the new browser after its callbacks and props are installed.
    fn navigate(&mut self) {
        let (Some(view), Some(source)) = (self.view.as_ref(), self.source.as_ref()) else {
            return;
        };
        let result = view.load_url(source);
        if let Err(message) = result {
            log::warn!("nsh-ext webview: {message}");
        }
    }

    fn build(&mut self, window: &mut Window, cx: &mut App) {
        if self.source.is_none() {
            return;
        }
        let builder = WebViewBuilder::new().with_visible(false);
        match backend::View::build(builder, window, cx, self.sink.clone()) {
            Ok(view) => {
                self.view = Some(view);
                self.placed = None;
                // The props batch is complete: the native callbacks are
                // installed, so this first navigation is reported.
                self.navigate();
            }
            Err(message) => {
                log::warn!("nsh-ext webview: {message}");
                self.sink.request_failed(message.clone());
                self.error = Some(message);
            }
        }
    }

    /// Draws what says the web view could not be created.
    fn unavailable(&self, message: &str) -> AnyElement {
        div()
            .size_full()
            .p_3()
            .bg(rgb(0x1b1f24))
            .text_color(rgb(0xe6edf3))
            .child(format!("WebView unavailable: {message}"))
            .into_any_element()
    }
}

impl NativeComponent for WebViewComponent {
    fn render(&mut self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let scale = window.scale_factor();
        if scale != self.scale {
            // wry converts bounds with the window's scale; place them again.
            self.scale = scale;
            self.placed = None;
        }
        if self.view.is_none() && self.error.is_none() && self.source.is_some() {
            self.build(window, cx);
        }
        match &self.error {
            Some(message) => Some(self.unavailable(message)),
            // The native web view sits above this transparent box.
            None => Some(div().size_full().into_any_element()),
        }
    }

    fn placed(&mut self, bounds: Option<Bounds<Pixels>>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(bounds) =
            bounds.filter(|bounds| bounds.size.width > px(0.0) && bounds.size.height > px(0.0))
        else {
            self.placed = None;
            if let Err(message) = view.hide() {
                log::debug!("nsh-ext webview: {message}");
            }
            return;
        };
        let key = (
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        );
        if self.placed == Some(key) {
            return;
        }
        match view.place(&bounds, self.scale) {
            Ok(()) => self.placed = Some(key),
            Err(message) => log::warn!("nsh-ext webview: {message}"),
        }
    }

    fn set_prop(
        &mut self,
        name: &str,
        value: Option<&JsonValue>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        match name {
            "src" => {
                let next = Self::source_of(value);
                if next == self.source {
                    return;
                }
                self.source = next;
                // Retire the callback capability before dropping a native
                // browser: destruction itself may synchronously call back.
                self.sink = Arc::new(self.sink.next_view());
                self.view.take();
                self.error = None;
                self.placed = None;
            }
            "trackProgress" => {
                let listen = value.and_then(JsonValue::as_bool).unwrap_or(false);
                self.sink.set_listening(listen);
            }
            _ => {}
        }
    }
}

impl Drop for WebViewComponent {
    fn drop(&mut self) {
        self.sink.close();
        self.view.take();
    }
}
