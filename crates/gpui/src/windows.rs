//! WebView2, a child window of GPUI's.
//!
//! WebView2 has no progress fraction: loads are indeterminate, so no numeric
//! estimate is ever reported here (the wire keeps `progress` null rather than
//! inventing zero). Wry's own `PageLoadEvent::Finished` is driven by
//! `NavigationCompleted` whether or not the navigation succeeded, so this
//! backend does not use it: `IsSuccess` is the native verdict, read from the
//! completion in [`Progress`], and a failure carries WebView2's
//! `WebErrorStatus`. `NavigationStarting` and `NavigationCompleted` both carry
//! a native navigation id, which keeps a cancelled navigation's completion
//! from being attributed to its replacement.

use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nsh_platform_gpui::gpui::{App, Bounds, Pixels, Window};
use webview2_com::{
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_WEB_ERROR_STATUS, COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
        ICoreWebView2, ICoreWebView2NavigationCompletedEventHandler,
        ICoreWebView2NavigationStartingEventHandler,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
};
use windows_core::BOOL;
use wry::{
    Rect, WebViewExtWindows as _,
    dpi::{LogicalPosition, LogicalSize},
};

use crate::progress::Sink;

/// A wry web view built for this window.
pub(crate) struct View {
    sink: Arc<Sink>,
    progress: Option<Progress>,
    webview: Rc<wry::WebView>,
}

impl View {
    /// Builds a hidden web view as a child window of `window`.
    pub(crate) fn build(
        builder: wry::WebViewBuilder<'_>,
        window: &mut Window,
        _cx: &mut App,
        sink: Arc<Sink>,
    ) -> Result<Self, String> {
        let webview = Rc::new(
            builder
                .build_as_child(&*window)
                .map_err(|error| format!("WebView2 could not make a web view: {error}"))?,
        );
        let progress = Progress::attach(&webview, sink.clone())?;
        Ok(Self {
            sink,
            progress: Some(progress),
            webview,
        })
    }

    /// Loads `url`, reporting a request WebView2 never accepted as a failure.
    pub(crate) fn load_url(&self, url: &str) -> Result<(), String> {
        self.webview.load_url(url).map_err(|error| {
            let message = format!("WebView2 cannot load {url}: {error}");
            self.sink.request_failed(message.clone());
            message
        })
    }

    /// Places the web view at `bounds` (GPUI points).
    pub(crate) fn place(&mut self, bounds: &Bounds<Pixels>, _scale: f32) -> Result<(), String> {
        let position = LogicalPosition::new(
            f64::from(f32::from(bounds.origin.x)),
            f64::from(f32::from(bounds.origin.y)),
        );
        let size = LogicalSize::new(
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        );
        self.webview
            .set_bounds(Rect {
                position: position.into(),
                size: size.into(),
            })
            .map_err(|error| format!("the page cannot be sized: {error}"))?;
        self.webview
            .set_visible(true)
            .map_err(|error| format!("the page cannot be shown: {error}"))
    }

    /// Hides the web view (its container is not drawn).
    pub(crate) fn hide(&mut self) -> Result<(), String> {
        self.webview
            .set_visible(false)
            .map_err(|error| format!("the page cannot be hidden: {error}"))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // Event handlers are removed before the web view they observe.
        self.progress.take();
    }
}

/// The web view's native navigation events.
struct Progress {
    webview: ICoreWebView2,
    starting: i64,
    completed: i64,
    // Registered handlers: WebView2 holds a reference, and these keep the
    // interfaces alive for the tokens' lifetime.
    _starting: ICoreWebView2NavigationStartingEventHandler,
    _completed: ICoreWebView2NavigationCompletedEventHandler,
}

impl Progress {
    fn attach(webview: &wry::WebView, sink: Arc<Sink>) -> Result<Self, String> {
        let core = webview.webview();
        let current = Arc::new(AtomicU64::new(0));

        let ids = current.clone();
        let started = sink.clone();
        let starting_handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
            let mut redirected = BOOL(0);
            if let Some(args) = args {
                let mut navigation = 0u64;
                // SAFETY: WebView2 owns the arguments for the call.
                if unsafe { args.NavigationId(&mut navigation) }.is_ok() {
                    ids.store(navigation, Ordering::Relaxed);
                }
                // SAFETY: WebView2 owns the arguments for the call.
                let _ = unsafe { args.IsRedirected(&mut redirected) };
            }
            if !redirected.as_bool() {
                // A redirect continues the navigation it started.
                started.started();
            }
            Ok(())
        }));
        let mut starting = 0i64;
        // SAFETY: the handler is an interface of this web view's event type,
        // kept alive in `Progress` and removed with `starting` in `Drop`.
        unsafe { core.add_NavigationStarting(&starting_handler, &mut starting) }
            .map_err(|error| format!("WebView2 cannot watch navigations: {error}"))?;

        let ids = current.clone();
        let finished = sink;
        let completed_handler =
            NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut navigation = 0u64;
                // SAFETY: WebView2 owns the arguments for the call.
                let _ = unsafe { args.NavigationId(&mut navigation) };
                if navigation != ids.load(Ordering::Relaxed) {
                    // A navigation replaced before it completed.
                    return Ok(());
                }
                let mut success = BOOL(0);
                // SAFETY: WebView2 owns the arguments for the call.
                unsafe { args.IsSuccess(&mut success) }?;
                if success.as_bool() {
                    finished.finished();
                    return Ok(());
                }
                let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                // SAFETY: WebView2 owns the arguments for the call.
                let _ = unsafe { args.WebErrorStatus(&mut status) };
                if status.0 == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED.0 {
                    // A navigation stopped by the host, not a failure.
                    finished.cancel();
                } else {
                    finished.failed(|| describe(status));
                }
                Ok(())
            }));
        let mut completed = 0i64;
        // SAFETY: as above: the handler matches this event, stays alive in
        // `Progress`, and its token is removed in `Drop`.
        unsafe { core.add_NavigationCompleted(&completed_handler, &mut completed) }
            .map_err(|error| format!("WebView2 cannot watch completions: {error}"))?;

        Ok(Self {
            webview: core,
            starting,
            completed,
            _starting: starting_handler,
            _completed: completed_handler,
        })
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // SAFETY: both tokens came from this web view's `add_*` calls.
        unsafe {
            let _ = self.webview.remove_NavigationStarting(self.starting);
            let _ = self.webview.remove_NavigationCompleted(self.completed);
        }
    }
}

/// WebView2's failure, as a diagnostic (its status codes are stable).
fn describe(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> String {
    match status.0 {
        1 => "the certificate's common name is incorrect",
        2 => "the certificate expired",
        3 => "the client certificate contains errors",
        4 => "the certificate was revoked",
        5 => "the certificate is invalid",
        6 => "the server is unreachable",
        7 => "the connection timed out",
        8 => "the server's response was invalid",
        9 => "the connection was aborted",
        10 => "the connection was reset",
        11 => "the network is disconnected",
        12 => "the connection could not be made",
        13 => "the host name could not be resolved",
        14 => "the navigation was cancelled",
        15 => "the redirect failed",
        16 => "the engine reported an unexpected error",
        17 => "authentication credentials are required",
        18 => "proxy authentication credentials are required",
        _ => "the navigation failed",
    }
    .to_owned()
}
