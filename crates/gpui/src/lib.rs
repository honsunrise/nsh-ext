//! Native Shell's `nsh-ext.webview` for the GPUI backend.
//!
//! [`register`] adds the component to a GPUI app's component registry, and the
//! app's `nsh-ext` elements (`nsh_ext::web_view()`, `nativeComponent` from
//! TSX) then render an **owned** wry web view filling their container: a
//! WKWebView on macOS, a WebView2 child window on Windows, and WebKitGTK 4.1
//! (GTK 3) in an X11 child window on Linux. The backend's own WebView is not
//! replaced, patched or reused.
//!
//! ```no_run
//! use nsh_platform_gpui::{AppOptions, AppSource, Root, run_app};
//! use nsh_core::prelude::*;
//!
//! struct App;
//! impl Component for App {
//!     type Props = ();
//!     type Event = NoEvent;
//!     fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
//!         nsh_ext::web_view().src("https://example.com").flex_1()
//!     }
//! }
//!
//! let mut options = AppOptions::default();
//! options.title = "My app".to_owned();
//! let options = nsh_ext_gpui::install(options);
//! let root = Root::of::<App>();
//! run_app(AppSource::Rust(root), options);
//! ```
//!
//! # Loading progress
//!
//! `on_load_progress` receives the wire documented by `nsh-ext`: one
//! `loadProgress` event per change, with a per-view navigation identity, a
//! lifecycle state, and the engine's own estimate when it has one.
//!
//! - macOS: WKWebView's `didStartProvisionalNavigation`, `didFinishNavigation`
//!   and `didFail(Provisional)Navigation` are the lifecycle (cancellations are
//!   stops, not failures); `estimatedProgress` supplies the estimate, and a
//!   completion is matched against the `WKNavigation` it names.
//! - Linux: WebKitGTK's estimated-load-progress is the estimate, its
//!   `load-changed` signals the lifecycle, and `load-failed` the failure (an
//!   interruption is a cancellation, and is emitted before `Finished`).
//! - Windows: WebView2 has no loading estimate (`progress` is null, never zero).
//!   Confirmed success reports `Some(1)`, as on every platform, from
//!   `NavigationCompleted`'s native `IsSuccess`, matched by navigation id.
//!
//! An estimate of one is not success. A screen shot or cookie API is not
//! implemented: the wire has no request for one, and this adapter adds no
//! invented surface.
//!
//! # Native children
//!
//! The page is a native child view or window of GPUI's window, positioned from
//! the component's container bounds in GPUI points and scaled by the window's
//! scale factor. On macOS and Windows it sits above everything GPUI draws, as
//! the backend's own web view does. On Linux it is hosted by a GPUI Fast native
//! composition surface, so GPUI's overlays draw above it; composition falls
//! back to a plain child window (not clipped by its ancestors) when a native
//! surface cannot be made. WebKitGTK embeds into X11 windows only: use X11 or
//! Xwayland on Linux.
//!
//! Native components are trusted host code: the embedding application decides
//! which URLs its page may reach.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

mod component;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod progress;
#[cfg(windows)]
mod windows;

use std::rc::Rc;

use nsh_platform_gpui::{
    AppOptions, ComponentContext, ComponentFactory, Components, NativeComponent,
};

/// The component name this adapter registers (`nsh-ext.webview`).
///
/// [`register`] uses it; an app that registers the factory itself must use the
/// same name, which is the one `nsh_ext::web_view()` sends.
pub const COMPONENT_NAME: &str = nsh_ext::COMPONENT_NAME;

/// Makes the web view for one container (an [`AppOptions::components`] entry).
pub fn factory() -> ComponentFactory {
    Rc::new(|context: ComponentContext| {
        Box::new(component::WebViewComponent::new(context)) as Box<dyn NativeComponent>
    })
}

/// Registers the extension's web view in `components`.
///
/// Register before the app loads: an `nsh-ext.webview` element drawn without a
/// registered factory renders an empty view.
pub fn register(components: &Components) {
    components.register(COMPONENT_NAME, factory());
}

/// [`register`], on the registry inside `options`.
///
/// ```no_run
/// # use nsh_platform_gpui::AppOptions;
/// let options = nsh_ext_gpui::install(AppOptions::default());
/// ```
pub fn install(options: AppOptions) -> AppOptions {
    register(&options.components);
    options
}
