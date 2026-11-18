//! WebKitGTK 4.1 on GTK 3, in an X11 child window of GPUI's.
//!
//! Adapted from Native Shell's `nsh-platform-gpui` web view (Apache-2.0,
//! Copyright Longbridge), which places its own wry web view the same way: GTK
//! runs on GPUI's main thread with its loop pumped from GPUI's, and the web
//! view lives in a GPUI Fast native composition surface (an X11 child window of
//! GPUI's) so that GPUI's overlays draw above it and the window's bounds clip
//! it. Without composition (an X11 window, but no native surface) the web view
//! is a child of GPUI's window itself and is not clipped by its ancestors.
//!
//! WebKitGTK only embeds into X11 windows: GPUI's window must be an X11 (or
//! Xwayland) window. Its `load-changed` and `load-failed` signals are the
//! native navigation lifecycle, and `estimated-load-progress` is the engine's
//! own estimate; none of them is derived from page scripts.
//!
//! WebKitGTK gives no navigation id. Its signals for one load are emitted in
//! native order, and `load-failed` ends a load before that load's `Finished` is
//! emitted, so the sink always sees the end of a navigation before anything
//! that could finish it a second time: a failed or interrupted load can never
//! be reported as a success. An interrupted load (a replacement, a stop, a
//! policy change such as a download) is reported as cancelled, not failed.

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use gtk::prelude::{ObjectExt as _, WidgetExt as _};
use nsh_platform_gpui::gpui::{
    AnyWindowHandle, App, AsyncApp, Bounds, DevicePixels, Pixels, Window, WindowCompositionSurface,
    point, size,
};
use webkit2gtk::{LoadEvent, NetworkError, PolicyError, WebViewExt as _};
use wry::{
    Rect, WebViewExtUnix as _,
    dpi::{PhysicalPosition, PhysicalSize},
    raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, WindowHandle, XlibWindowHandle,
    },
};

use crate::progress::Sink;

/// How long a web view's composition surface outlives the web view: GTK
/// destroys the web view's X windows from its loop, and its parent must go
/// after them.
const RELEASE_DELAY: Duration = Duration::from_millis(500);

/// How often pending GTK events are dispatched between GPUI's.
const GTK_POLL: Duration = Duration::from_millis(16);

thread_local! {
    /// GTK is started on this (the main) thread.
    static GTK: Cell<bool> = const { Cell::new(false) };
    /// The web views alive: the pump stops when the last goes.
    static LIVE: Cell<usize> = const { Cell::new(0) };
}

/// One web view's place in [`LIVE`], taken before GTK is started (so the pump
/// never sees the count of a view it is pumping for at zero) and given back
/// when the view goes.
struct Counted;

impl Counted {
    fn new() -> Self {
        LIVE.set(LIVE.get() + 1);
        Self
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        LIVE.set(LIVE.get().saturating_sub(1));
    }
}

/// A wry web view built for this window.
pub(crate) struct View {
    sink: Arc<Sink>,
    progress: Option<Progress>,
    webview: Rc<wry::WebView>,
    composition: Option<Composition>,
    /// The zoom last applied (GPUI points over GDK's integer scale).
    zoom: Option<f64>,
    /// Held for the whole life of the view: [`LIVE`].
    _counted: Counted,
}

impl View {
    /// Builds a hidden web view in a composition surface of `window`.
    pub(crate) fn build(
        builder: wry::WebViewBuilder<'_>,
        window: &mut Window,
        cx: &mut App,
        sink: Arc<Sink>,
    ) -> Result<Self, String> {
        let counted = Counted::new();
        let raw = HasWindowHandle::window_handle(&*window)
            .map_err(|error| format!("the window has no native handle: {error}"))?
            .as_raw();
        let parent = X11Parent::from_raw(raw).ok_or_else(|| {
            "WebKitGTK only embeds into X11 windows; run the app on X11 or Xwayland".to_owned()
        })?;
        ensure_gtk(cx)?;
        let composition = match Composition::new(window, cx) {
            Ok(composition) => Some(composition),
            Err(error) => {
                log::warn!(
                    "nsh-ext webview: no window composition ({error:#}): the page is not clipped"
                );
                None
            }
        };
        let built = match &composition {
            Some(composition) => builder.build_as_child(&composition.parent()?),
            None => builder.build_as_child(&parent),
        };
        let webview = Rc::new(
            built.map_err(|error| format!("WebKitGTK could not make a web view: {error}"))?,
        );
        let progress = Progress::attach(&webview, sink.clone());
        Ok(Self {
            sink,
            progress: Some(progress),
            webview,
            composition,
            zoom: None,
            _counted: counted,
        })
    }

    /// Loads `url`, reporting a request WebKitGTK never accepted as a failure.
    pub(crate) fn load_url(&self, url: &str) -> Result<(), String> {
        self.webview.load_url(url).map_err(|error| {
            let message = format!("WebKitGTK cannot load {url}: {error}");
            self.sink.request_failed(message.clone());
            message
        })
    }

    /// Places the web view at `bounds` (GPUI points).
    pub(crate) fn place(&mut self, bounds: &Bounds<Pixels>, scale: f32) -> Result<(), String> {
        // Showing realizes GTK's child window; bounds applied before this can
        // be replaced by its initial allocation.
        self.webview
            .set_visible(true)
            .map_err(|error| format!("the page cannot be shown: {error}"))?;
        self.set_zoom(scale)?;
        let x = f32::from(bounds.origin.x);
        let y = f32::from(bounds.origin.y);
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        let device = |value: f32| (value * scale).round() as i32;
        match &self.composition {
            Some(composition) => {
                composition.set_bounds(Bounds::new(
                    point(DevicePixels(device(x)), DevicePixels(device(y))),
                    size(DevicePixels(device(width)), DevicePixels(device(height))),
                ))?;
                // wry divides these by GDK's factor (not GPUI's), which is
                // what GTK lays a widget out with: the surface's device
                // pixels, whose origin is the container.
                self.webview
                    .set_bounds(Rect {
                        position: PhysicalPosition::new(0i32, 0).into(),
                        size: PhysicalSize::new(device(width), device(height)).into(),
                    })
                    .map_err(|error| format!("the page cannot be sized: {error}"))?;
                composition.set_visible(true)?;
            }
            None => {
                // No surface between the page and GPUI's window: wry's child
                // window is at the container's place, in window device pixels.
                self.webview
                    .set_bounds(Rect {
                        position: PhysicalPosition::new(device(x), device(y)).into(),
                        size: PhysicalSize::new(device(width), device(height)).into(),
                    })
                    .map_err(|error| format!("the page cannot be sized: {error}"))?;
            }
        }
        Ok(())
    }

    /// Hides the web view (its container is not drawn).
    pub(crate) fn hide(&mut self) -> Result<(), String> {
        if let Some(composition) = &self.composition {
            composition.set_visible(false)?;
        }
        self.webview
            .set_visible(false)
            .map_err(|error| format!("the page cannot be hidden: {error}"))
    }

    /// Makes the page's CSS pixels GPUI's points (GDK's scale is an integer,
    /// GPUI's need not be).
    fn set_zoom(&mut self, scale: f32) -> Result<(), String> {
        let gdk = self.webview.webview().scale_factor().max(1);
        let zoom = f64::from(scale) / f64::from(gdk);
        if self.zoom == Some(zoom) {
            return Ok(());
        }
        self.webview
            .zoom(zoom)
            .map_err(|error| format!("the page cannot be scaled: {error}"))?;
        self.zoom = Some(zoom);
        Ok(())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // Native callbacks go before anything they can reach.
        self.progress.take();
        if let Some(composition) = self.composition.take() {
            // The web view is destroyed first, from GTK's loop; the surface
            // after it.
            composition.release(self.webview.clone());
        }
    }
}

/// The web view's native lifecycle signals.
struct Progress {
    webview: webkit2gtk::WebView,
    handlers: Vec<gtk::glib::SignalHandlerId>,
}

impl Progress {
    fn attach(webview: &wry::WebView, sink: Arc<Sink>) -> Self {
        let view = webview.webview();
        let started = {
            let sink = sink.clone();
            view.connect_load_changed(move |view, event| match event {
                LoadEvent::Started => sink.started(),
                // The engine's own estimate at a native point, whether or not
                // the property notifies again.
                LoadEvent::Committed => sink.estimate(view.estimated_load_progress()),
                // A load that failed was ended by its `load-failed`, which
                // WebKitGTK emits before this signal, so this is a success.
                LoadEvent::Finished if !view.is_loading() => sink.finished(),
                _ => {}
            })
        };
        let failed = {
            let sink = sink.clone();
            view.connect_load_failed(move |view, _, url, error| {
                // A cancelled older in-page load cannot terminate its successor.
                if view.is_loading() {
                    return true;
                }
                // `load-failed` is emitted before the `Finished` of the same
                // load, so ending the navigation here keeps a failed or
                // interrupted load from later being reported as a success.
                if error.matches(NetworkError::Cancelled)
                    || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
                {
                    // A replaced load, a stopped one or a download: the
                    // navigation ended without failing the page.
                    log::debug!("nsh-ext webview: the load of {url} was interrupted");
                    sink.cancel();
                    return true;
                }
                sink.failed(|| format!("{error}"));
                // Suppress WebKit's default error-document navigation: it is
                // not a new page/host navigation and must not become success.
                true
            })
        };
        let estimated = {
            let sink = sink;
            view.connect_estimated_load_progress_notify(move |view| {
                sink.estimate(view.estimated_load_progress());
            })
        };
        Self {
            webview: view,
            handlers: vec![started, failed, estimated],
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        for handler in std::mem::take(&mut self.handlers) {
            self.webview.disconnect(handler);
        }
    }
}

/// Starts GTK on its X11 backend once, pumping its main loop from GPUI's.
fn ensure_gtk(cx: &mut App) -> Result<(), String> {
    if GTK.get() {
        return Ok(());
    }
    if !gtk::is_initialized_main_thread() {
        // wry downcasts the default GDK display to X11's.
        gtk::gdk::set_allowed_backends("x11");
        gtk::init().map_err(|error| {
            format!("GTK did not start ({error}); WebKitGTK needs GTK 3 and an X11 display")
        })?;
    }
    GTK.set(true);
    cx.spawn(async move |cx| {
        loop {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            if LIVE.get() == 0 {
                // Started again by the next web view.
                GTK.set(false);
                break;
            }
            cx.background_executor().timer(GTK_POLL).await;
        }
    })
    .detach();
    Ok(())
}

/// A GPUI Fast composition surface holding the web view: an X11 child window
/// of GPUI's, so GPUI's overlays draw above the page.
struct Composition {
    surface: WindowCompositionSurface,
    window: AnyWindowHandle,
    cx: AsyncApp,
    /// Its removal was started (by [`Composition::release`]).
    released: Cell<bool>,
}

impl Composition {
    fn new(window: &Window, cx: &mut App) -> anyhow::Result<Self> {
        let surface = window
            .enable_window_composition()?
            .create_native_surface()?;
        Ok(Self {
            surface,
            window: window.window_handle(),
            cx: cx.to_async(),
            released: Cell::new(false),
        })
    }

    /// Its X11 window, as the parent wry builds the web view in.
    fn parent(&self) -> Result<X11Parent, String> {
        let handle = self
            .surface
            .platform_surface()
            .and_then(|surface| surface.platform_handle())
            .map_err(|error| error.to_string())?;
        handle
            .downcast::<RawWindowHandle>()
            .ok()
            .and_then(|handle| X11Parent::from_raw(*handle))
            .ok_or_else(|| "the composition surface is not an X11 window".to_owned())
    }

    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<(), String> {
        self.surface
            .platform_surface()
            .and_then(|surface| surface.set_bounds(bounds))
            .map_err(|error| format!("the composition surface cannot be sized: {error}"))
    }

    fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.surface
            .platform_surface()
            .and_then(|surface| surface.set_visible(visible))
            .map_err(|error| format!("the composition surface cannot be toggled: {error}"))
    }

    /// Removes the surface, after the web view's own windows went (later: the
    /// web view may go while GPUI's app is borrowed).
    fn release(self, webview: Rc<wry::WebView>) {
        self.released.set(true);
        let surface = self.surface.clone();
        let window = self.window;
        let cx = self.cx.clone();
        cx.spawn(async move |cx| {
            drop(webview);
            remove_surface(cx, window, surface).await;
        })
        .detach();
    }
}

impl Drop for Composition {
    fn drop(&mut self) {
        if self.released.replace(true) {
            return;
        }
        // The web view could not be built: the surface still goes.
        let surface = self.surface.clone();
        let window = self.window;
        let cx = self.cx.clone();
        cx.spawn(async move |cx| remove_surface(cx, window, surface).await)
            .detach();
    }
}

/// Removes `surface` once GTK has destroyed what its web view left.
async fn remove_surface(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    surface: WindowCompositionSurface,
) {
    // GTK destroys a web view's X windows from its loop: let it, before their
    // parent goes (an X error on a destroyed window aborts GTK).
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
    cx.background_executor().timer(RELEASE_DELAY).await;
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
    let removed = window.update(cx, |_, window, _| {
        window
            .enable_window_composition()?
            .remove_surface(surface.id())
    });
    if let Ok(Err(error)) = removed {
        log::warn!("nsh-ext webview: cannot remove its composition surface: {error:#}");
    }
}

/// An X11 window as the Xlib parent wry wants (GPUI reports its X11 windows
/// through XCB).
struct X11Parent(XlibWindowHandle);

impl X11Parent {
    fn from_raw(handle: RawWindowHandle) -> Option<Self> {
        match handle {
            RawWindowHandle::Xcb(handle) => {
                let mut xlib = XlibWindowHandle::new(handle.window.get().into());
                xlib.visual_id = handle.visual_id.map_or(0, |id| id.get().into());
                Some(Self(xlib))
            }
            RawWindowHandle::Xlib(handle) => Some(Self(handle)),
            _ => None,
        }
    }
}

impl HasWindowHandle for X11Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: the X11 window outlives the web view, which goes before the
        // composition surface (and with GPUI's window).
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xlib(self.0)) })
    }
}
