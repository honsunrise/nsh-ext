//! Linux GTK 4 / WebKitGTK 6 loading-progress adapter for [`nsh_ext`](https://docs.rs/nsh-ext).
//!
//! [`register`] installs an extension-owned native component. Each explicit
//! `src` change retires its browser before starting another, isolating late
//! callbacks at the cost of cross-source history and browser warm-up.
//! Clearing works while hidden; a new source waits for the view to be mapped.
//! Page-originated navigation stays in its browser. `trackProgress` gates
//! emission, and estimates come from WebKit.

use nsh_core::element::{Registry, native_components, native_module};

#[native_module("nsh-ext-linux")]
fn native(_registry: &mut Registry) {}

#[cfg(target_os = "linux")]
mod adapter;
#[cfg(target_os = "linux")]
pub use adapter::register;

#[native_components(linux)]
fn components(components: &nsh_platform_linux::Components) {
    register(components);
}
