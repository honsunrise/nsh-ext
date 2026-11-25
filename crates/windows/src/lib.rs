//! Windows (WebView2) adapter for [`nsh_ext::web_view`].
//!
//! Register the adapter before loading an app, then the extension's
//! `nsh-ext.webview` component renders an extension-owned WebView2:
//!
//! ```no_run
//! use nsh_platform_windows::AppOptions;
//!
//! let options = AppOptions {
//!     title: "WebView2".to_owned(),
//!     size: (960, 720),
//!     ..Default::default()
//! };
//! nsh_ext_windows::register(&options.components);
//! // Pass `options` and the app to `run_app`.
//! ```
//!
//! WebView2 reports no completion percentage. The adapter therefore emits
//! `started`, `finished`, `failed` and `cancelled`, with `progress` set only
//! when WebView2 itself confirms completion; it never invents an estimate.
//!
//! Each explicit `src` change retires the current browser and loads the new
//! source in a fresh one; an empty source retires it and clears the view.
//! That ends the old page's document, timers and workers with the browser
//! that owns them, at the cost of cross-source history and a cold browser
//! warm-up. Re-setting the same source is not a change and does not reload;
//! a page-originated navigation stays in its browser.
//!
//! Native components are trusted host code: NSH's URL grants for its
//! built-in web view do not sandbox this component. The embedding
//! application decides which sources it passes and whether untrusted
//! modules may use it.

use nsh_core::element::{Registry, native_components, native_module};

#[native_module("nsh-ext-windows")]
fn native(_registry: &mut Registry) {}

#[native_components(windows)]
fn components(components: &Components) {
    register(components);
}

#[cfg(windows)]
mod environment;
#[cfg(windows)]
mod host;

#[cfg(windows)]
use std::rc::Rc;

#[cfg(windows)]
use nsh_ext::{COMPONENT_NAME, PROGRESS_EVENT};
#[cfg(windows)]
use nsh_platform_windows::{
    ComponentContext, ComponentFactory, Components, NativeComponent,
    windows::Win32::Foundation::HWND,
};

#[cfg(windows)]
use crate::host::WebViewHost;

/// Registers the extension's WebView with `components`.
///
/// Call it before loading an app, on the UI thread. It never replaces an
/// NSH built-in: the registration goes on the app's own component registry
/// under [`nsh_ext::COMPONENT_NAME`].
#[cfg(windows)]
pub fn register(components: &Components) {
    components.register(COMPONENT_NAME, component_factory());
}

/// The factory for the extension's native component, for
/// [`AppView::register_component`](nsh_platform_windows::AppView::register_component).
#[cfg(windows)]
pub fn component_factory() -> ComponentFactory {
    Rc::new(|context: ComponentContext| -> Box<dyn NativeComponent> {
        Box::new(WebViewComponent::new(context))
    })
}

/// The native implementation of the `nsh-ext.webview` component.
///
/// Private to the adapter: apps create it through [`nsh_ext::web_view`] and
/// [`register`].
#[cfg(windows)]
struct WebViewComponent {
    host: WebViewHost,
}

#[cfg(windows)]
impl WebViewComponent {
    /// Makes a component for `context`, in a new hidden child window that
    /// NSH parents and places.
    fn new(context: ComponentContext) -> Self {
        let host = WebViewHost::new(None).unwrap_or_else(|error| {
            log::error!("nsh-ext-windows: cannot create the WebView2 host: {error}");
            WebViewHost::unavailable(error.to_string())
        });
        host.set_progress_handler(move |progress| {
            context.emit(PROGRESS_EVENT, progress.to_value());
        });
        Self { host }
    }
}

#[cfg(windows)]
impl NativeComponent for WebViewComponent {
    fn window(&self) -> Option<HWND> {
        let window = self.host.window();
        (!window.is_invalid()).then_some(window)
    }

    fn draw(
        &self,
        _dc: &nsh_platform_windows::windows::Win32::Graphics::Direct2D::ID2D1DeviceContext,
        size: (f32, f32),
    ) {
        // WebView2 draws itself; the controller fills the child window's
        // pixel client area. `draw` reports the frame in points and is used
        // only until NSH has placed the window; `WM_SIZE` then supplies the
        // same pixel size.
        self.host.set_bounds(size.0, size.1);
    }

    fn set_prop(&mut self, name: &str, value: Option<&serde_json::Value>) {
        match name {
            "src" => self
                .host
                .set_source(value.and_then(|value| value.as_str()).unwrap_or_default()),
            "trackProgress" => self
                .host
                .set_track_progress(value.and_then(|value| value.as_bool()).unwrap_or(false)),
            _ => {}
        }
    }
}
