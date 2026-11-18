//! The iOS, Mac Catalyst and macOS adapter for `nsh-ext`'s WebView component.
//!
//! Register [`COMPONENT_NAME`](nsh_ext::COMPONENT_NAME) with Native Shell
//! before the app loads, then use `nsh_ext::web_view()` in Rust or
//! `nativeComponent("nsh-ext.webview", …)` in TSX:
//!
//! ```no_run
//! # #[cfg(target_os = "macos")]
//! # fn example(app: &nsh_platform_macos::AppView) {
//! nsh_ext_apple::register(app);
//! # }
//! ```
//!
//! A host that runs an app through `run_app` registers through the platform's
//! process-wide hook instead:
//!
//! ```no_run
//! # #[cfg(target_os = "macos")]
//! # fn example() {
//! nsh_platform_macos::set_components(nsh_ext_apple::register_components);
//! # }
//! ```
//!
//! The adapter owns a stable native container and one `WKWebView` inside it;
//! it never replaces Native Shell's built-in web client or its delegates. It
//! reports only the lifecycle and fraction WebKit provides:
//!
//! - `src` is deferred by one main-loop turn so the initial load sees the
//!   `trackProgress` prop that arrives later in the same props batch. Setting
//!   a different non-empty `src` cancels the load in flight (reported as
//!   `Cancelled`), destroys the current browser and page (its history and
//!   running timers/workers included) and loads the new source in a fresh
//!   `WKWebView` inside the same container. An empty or removed `src`
//!   cancels the active load and destroys the page the same way, without
//!   reporting a blank navigation or any event for the disposal.
//! - `trackProgress` (false by default) is purely an emission gate: every
//!   native navigation still advances its identity, and enabling it mid-load
//!   reports the next native estimate/finish without inventing a start.
//! - `loadProgress` carries a per-component navigation identity, the native
//!   lifecycle state, WebKit's own `estimatedProgress` (or null), and a
//!   failure diagnostic. A KVO estimate of one is still `Loading`;
//!   `Finished` comes only from `webView:didFinishNavigation:`. An HTTP error
//!   document (a 404 or 500 page) finishes; a native connection or TLS
//!   failure does not.
//! - Navigation identities belong to the component: they continue when
//!   `src` changes, and a component that is removed and added again starts
//!   again at one. Page-originated navigations stay inside the current
//!   browser.
//!
//! Native components are trusted host code. The embedding app decides which
//! URLs are acceptable: this adapter does not apply Native Shell's URL
//! grants to the page it loads.

#![cfg_attr(
    not(any(target_os = "ios", target_os = "macos")),
    allow(dead_code, unused_imports)
)]

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
compile_error!(
    "nsh-ext-apple supports only iOS (including Mac Catalyst) and macOS; use another nsh-ext adapter for this target"
);

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod tracker;
#[cfg(any(target_os = "ios", target_os = "macos"))]
mod webview;

#[cfg(target_os = "ios")]
/// The Native Shell platform crate this adapter targets (UIKit, including
/// Mac Catalyst).
pub use nsh_platform_ios as platform;
#[cfg(target_os = "macos")]
/// The Native Shell platform crate this adapter targets (AppKit).
pub use nsh_platform_macos as platform;

#[cfg(any(target_os = "ios", target_os = "macos"))]
pub use nsh_ext::{COMPONENT_NAME, LoadProgress, LoadState, PROGRESS_EVENT};
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub use platform::{ComponentContext, ComponentFactory, Components, NativeComponent};
#[cfg(any(target_os = "ios", target_os = "macos"))]
use webview::WebViewComponent;

use std::rc::Rc;

/// Makes the factory that creates one extension component per element.
///
/// This is the value to pass to `Components::register` or
/// `AppView::register_component` when a host does its own registration.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn factory() -> ComponentFactory {
    Rc::new(|context| -> Box<dyn NativeComponent> { Box::new(WebViewComponent::new(context)) })
}

/// Registers the extension WebView with an app view.
///
/// Call this after the `AppView` is created and before it loads an app: the
/// registration applies to elements created from then on.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn register(app: &platform::AppView) {
    app.register_component(COMPONENT_NAME, factory());
}

/// Registers the extension WebView on a component registry.
///
/// This has the exact signature of the platform crates' `set_components`
/// hook, so standalone hosts can write
/// `platform::set_components(nsh_ext_apple::register_components)` before
/// `run_app`.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn register_components(components: &Components) {
    components.register(COMPONENT_NAME, factory());
}
