//! One extension-owned WebView2 view, parented to an NSH native component's
//! child window.
//!
//! The child window is the component's stable native container: it is made
//! once and outlives every source. The WebView2 environment is made once per
//! profile folder; the browser (a controller and its core) is made per
//! source. An explicit `src` change retires the old browser and makes a new
//! one, and an empty source retires it without making one. That is how a
//! replaced or cleared page stops: `Close` tears down the document,
//! including its timers, workers and requests, instead of relying on a
//! navigation the adapter cannot verify. The cost is that a new browser has
//! no history and a cold start; a page-originated navigation stays inside
//! its browser and is not replayed.
//!
//! Every browser has a generation. Async controller creation and every
//! native event are fenced by the generation they were made for, so a
//! controller that finishes after its source was replaced is closed, and an
//! event from a retired browser changes nothing. Only one controller
//! creation is outstanding at a time, because two controllers must never
//! race for the same parent window.
//!
//! Navigation ids come from WebView2's own `NavigationStarting` and
//! `NavigationCompleted` events; an estimate is never invented, because
//! WebView2 has none to report. `trackProgress` gates emission only: the
//! tracker advances with every native event whether tracking is on or off.
//!
//! The controller is sized in the child window's pixels. Its bounds mode
//! is pinned to raw pixels, and its rasterization scale follows the
//! window's DPI, so the page's CSS viewport is the window's client area
//! divided by the window's scale at any DPI (2x included).
//!
//! Portions of the WebView2 setup were adapted from `nsh-platform-windows`
//! 0.5.1 (Apache-2.0, Longbridge), the release this host was written against,
//! and are kept against the 0.6.0 contract it now targets.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    rc::{Rc, Weak},
    sync::OnceLock,
};

use nsh_ext::{LoadProgress, ProgressTracker};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS, COREWEBVIEW2_PROCESS_FAILED_KIND,
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
        COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED, COREWEBVIEW2_WEB_ERROR_STATUS,
        COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS,
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED,
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET, COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED,
        COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE,
        COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED,
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
        COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED,
        COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE, COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT,
        COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR, ICoreWebView2, ICoreWebView2Controller,
        ICoreWebView2Controller3, ICoreWebView2Environment,
        ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2NavigationStartingEventArgs,
        ICoreWebView2ProcessFailedEventArgs,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler, ProcessFailedEventHandler,
    take_pwstr,
};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
                GetClientRect, RegisterClassW, WINDOW_EX_STYLE, WM_NCDESTROY, WM_SIZE, WNDCLASSW,
                WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_POPUP, WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, Interface, PCWSTR, PWSTR, w},
};

use crate::environment::request_environment;

/// The class of the child window a view is drawn in. The acceptance
/// harness finds this window by this class, so the two names stay in sync.
const CLASS: PCWSTR = w!("nsh_ext_webview2_host");

thread_local! {
    /// Child windows by handle, for `WM_SIZE`: the window's pixel client
    /// area is the controller's bounds, so a resize (from NSH's placement
    /// or from an embedding host) reaches the controller even when the
    /// component's `draw` does not run that frame.
    static WINDOWS: RefCell<HashMap<usize, Weak<HostState>>> = RefCell::new(HashMap::new());
}

/// Why a native WebView2 host could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostError {
    message: String,
}

impl HostError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

/// A WebView2 view owned by this extension, in a child window of its own.
///
/// Internal to the adapter: the app sees only the registered native
/// component. Make it on the UI thread that owns the window, after COM is
/// initialized (`OleInitialize`, as NSH's Windows host does).
pub(crate) struct WebViewHost {
    state: Rc<HostState>,
}

impl WebViewHost {
    /// Makes a host in a new hidden child window.
    ///
    /// With a `parent`, the window starts as its visible child; without one
    /// it stays hidden until NSH adopts it.
    pub(crate) fn new(parent: Option<HWND>) -> Result<Self, HostError> {
        let child = create_child(parent).map_err(|error| {
            HostError::new(format!("cannot create the WebView2 host window: {error}"))
        })?;
        let state = Rc::new(HostState::new(child));
        WINDOWS.with(|windows| {
            windows
                .borrow_mut()
                .insert(child.0 as usize, Rc::downgrade(&state));
        });
        Ok(Self { state })
    }

    /// A host that could not create its window: every source reports the
    /// failure instead of pretending to load.
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            state: Rc::new(HostState::unavailable(message.into())),
        }
    }

    /// The child window the controller draws in.
    pub(crate) fn window(&self) -> HWND {
        self.state.child
    }

    /// Loads `source`; an empty source clears the view.
    ///
    /// An explicit change retires the current browser and makes a new one
    /// for `source`; an empty source only retires it. A source set while a
    /// controller is still being made is the latest one and waits for that
    /// creation to end.
    pub(crate) fn set_source(&self, source: &str) {
        self.state.set_source(source);
    }

    /// Enables or disables progress events. A setup failure that happened
    /// before tracking was enabled is reported when it is turned on.
    pub(crate) fn set_track_progress(&self, track: bool) {
        self.state.set_track_progress(track);
    }

    /// Receives native lifecycle events while tracking is enabled.
    pub(crate) fn set_progress_handler(&self, handler: impl Fn(&LoadProgress) + 'static) {
        *self.state.handler.borrow_mut() = Some(Rc::new(handler));
    }

    /// Sizes the WebView2 to the child window's client area; `width` and
    /// `height` are the component's frame in points, used only before the
    /// window has been placed.
    pub(crate) fn set_bounds(&self, width: f32, height: f32) {
        self.state.set_bounds(width, height);
    }
}

impl Drop for WebViewHost {
    fn drop(&mut self) {
        // A WebView2 callback may still hold a weak reference to the state;
        // marking it disposed stops that callback from emitting into a
        // dropped app. Disposal emits nothing: the handler is dropped before
        // the controller is closed.
        self.state.disposed.set(true);
        self.state.handler.borrow_mut().take();
    }
}

type ProgressHandler = Rc<dyn Fn(&LoadProgress)>;

struct HostState {
    child: HWND,
    /// The source prop's latest value.
    src: RefCell<String>,
    /// Whether events are emitted.
    track: Cell<bool>,
    handler: RefCell<Option<ProgressHandler>>,
    tracker: RefCell<ProgressTracker>,
    /// The WebView2 navigation the tracker identity belongs to.
    active: Cell<Option<Active>>,
    /// A source navigation has started in the current browser.
    navigated: Cell<bool>,
    webview: RefCell<Option<ICoreWebView2>>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    tokens: Cell<Option<Tokens>>,
    /// The child window's client area (pixels) the controller fills.
    bounds: Cell<(i32, i32)>,
    applied: Cell<Option<(i32, i32)>>,
    /// The rasterization scale the controller was given, in pixels per
    /// point; zero until the controller exists.
    raster_scale: Cell<f32>,
    /// The browser identity: every retirement makes a new one.
    generation: Cell<u64>,
    /// Whether a controller creation is outstanding (for some generation).
    creating: Cell<bool>,
    disposed: Cell<bool>,
    /// The host window could not be created; every source reports it.
    window_failure: RefCell<Option<String>>,
    /// The source a setup failure was already reported for.
    failure_reported: RefCell<Option<String>>,
    /// Setup-failure events that waited for `trackProgress`.
    pending_failure: RefCell<Option<PendingFailure>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Active {
    native: u64,
    navigation: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Tokens {
    starting: i64,
    completed: i64,
    failed: i64,
}

/// A setup failure whose events were made while tracking was off.
struct PendingFailure {
    source: String,
    started: LoadProgress,
    failed: LoadProgress,
}

impl HostState {
    fn new(child: HWND) -> Self {
        Self {
            child,
            src: RefCell::new(String::new()),
            track: Cell::new(false),
            handler: RefCell::new(None),
            tracker: RefCell::new(ProgressTracker::default()),
            active: Cell::new(None),
            navigated: Cell::new(false),
            webview: RefCell::new(None),
            controller: RefCell::new(None),
            tokens: Cell::new(None),
            bounds: Cell::new((0, 0)),
            applied: Cell::new(None),
            raster_scale: Cell::new(0.0),
            generation: Cell::new(0),
            creating: Cell::new(false),
            disposed: Cell::new(false),
            window_failure: RefCell::new(None),
            failure_reported: RefCell::new(None),
            pending_failure: RefCell::new(None),
        }
    }

    fn unavailable(message: String) -> Self {
        let state = Self::new(HWND::default());
        *state.window_failure.borrow_mut() = Some(message);
        state
    }

    /// Whether `generation` is the live browser of a live host.
    fn current(&self, generation: u64) -> bool {
        !self.disposed.get() && self.generation.get() == generation
    }

    fn set_source(self: &Rc<Self>, source: &str) {
        if self.disposed.get() {
            return;
        }
        let previous = self.src.replace(source.to_owned());
        if previous == source {
            return;
        }
        self.retire();
        if self.disposed.get() || self.src.borrow().as_str() != source {
            // Disposed, or a progress handler changed the source while the
            // cancellation was reported; that call owns the view now.
            return;
        }
        if !source.is_empty() {
            Self::ensure_controller(self);
        }
    }

    fn set_track_progress(&self, track: bool) {
        self.track.set(track);
        if track {
            self.report_pending_failure();
        }
    }

    /// Retires the current browser: its events and its page end here.
    ///
    /// The generation is bumped before the native teardown, so a callback
    /// that is already in flight is ignored. The page itself ends with
    /// `Close`, which destroys the document and everything the document
    /// started (timers, workers, requests).
    fn retire(&self) {
        self.generation.set(
            self.generation
                .get()
                .checked_add(1)
                .expect("browser generation exhausted"),
        );
        *self.failure_reported.borrow_mut() = None;
        self.pending_failure.borrow_mut().take();
        let tokens = self.tokens.take();
        let webview = self.webview.borrow_mut().take();
        if let (Some(webview), Some(tokens)) = (&webview, tokens) {
            // SAFETY: tokens of handlers added on this live web view.
            unsafe {
                let _ = webview.remove_NavigationStarting(tokens.starting);
                let _ = webview.remove_NavigationCompleted(tokens.completed);
                let _ = webview.remove_ProcessFailed(tokens.failed);
            }
        }
        drop(webview);
        let controller = self.controller.borrow_mut().take();
        if let Some(controller) = controller {
            // SAFETY: closes the live controller; nothing uses it after
            // this. `Close` is synchronous and does not run `beforeunload`.
            unsafe {
                let _ = controller.SetIsVisible(false);
                let _ = controller.Close();
            }
        }
        self.active.set(None);
        self.navigated.set(false);
        self.applied.set(None);
        self.raster_scale.set(0.0);
        self.cancel_loading();
    }

    /// Reports the loading navigation as cancelled; a finished or idle
    /// tracker emits nothing.
    fn cancel_loading(&self) {
        self.active.set(None);
        let cancelled = {
            let mut tracker = self.tracker.borrow_mut();
            let navigation = tracker.navigation();
            tracker.cancel(navigation)
        };
        self.publish(cancelled);
    }

    /// Asks for a browser for the current source, once.
    ///
    /// The creation is serialized: the parent window never has two
    /// controllers racing for it, and a source that arrives during creation
    /// waits for the in-flight one, which is closed when its generation is
    /// stale.
    fn ensure_controller(self: &Rc<Self>) {
        if self.disposed.get() || self.webview.borrow().is_some() || self.creating.get() {
            return;
        }
        if self.child.is_invalid() {
            if let Some(message) = self.window_failure.borrow().clone() {
                self.report_failure(message);
            }
            return;
        }
        if self.src.borrow().is_empty() {
            return;
        }
        self.creating.set(true);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        request_environment(Box::new(move |environment| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if !state.current(generation) {
                // The source was replaced or cleared while the environment
                // was being made; the latest source may need a browser now.
                state.creating.set(false);
                Self::ensure_controller(&state);
                return;
            }
            match environment {
                Some(environment) => state.make_controller(environment, generation),
                None => {
                    state.creating.set(false);
                    state.report_failure("the WebView2 runtime is not available".to_owned());
                }
            }
        }));
    }

    /// Asks the environment for the controller in this host's window.
    fn make_controller(self: &Rc<Self>, environment: ICoreWebView2Environment, generation: u64) {
        if !self.current(generation) || self.child.is_invalid() {
            self.creating.set(false);
            Self::ensure_controller(self);
            return;
        }
        let weak = Rc::downgrade(self);
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
            move |result, controller| {
                let Some(state) = weak.upgrade() else {
                    // The view went away while WebView2 was making it.
                    close(controller);
                    return Ok(());
                };
                if !state.current(generation) {
                    // This controller belongs to a retired source; its
                    // window must be free before the latest source gets one.
                    close(controller);
                    state.creating.set(false);
                    Self::ensure_controller(&state);
                    return Ok(());
                }
                state.creating.set(false);
                match (result, controller) {
                    (Ok(()), Some(controller)) => state.ready(controller, generation),
                    (Err(error), _) => state
                        .report_failure(format!("cannot create the WebView2 controller: {error}")),
                    (Ok(()), None) => {
                        state.report_failure("WebView2 made no controller".to_owned())
                    }
                }
                Ok(())
            },
        ));
        // SAFETY: a call on the live environment, on the window's thread.
        let made = unsafe { environment.CreateCoreWebView2Controller(self.child, &handler) };
        if let Err(error) = made {
            self.creating.set(false);
            self.report_failure(format!("cannot create the WebView2 controller: {error}"));
        }
    }

    /// The controller is ready: wire navigation, apply the bounds and load
    /// the latest source.
    fn ready(self: &Rc<Self>, controller: ICoreWebView2Controller, generation: u64) {
        if !self.current(generation) {
            // SAFETY: closes a controller whose view is gone.
            unsafe {
                let _ = controller.Close();
            }
            return;
        }
        // SAFETY: WebView2 calls on the live controller, on its thread.
        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(webview) => webview,
            Err(error) => {
                // SAFETY: as above.
                unsafe {
                    let _ = controller.Close();
                }
                self.report_failure(format!("WebView2 has no core: {error}"));
                return;
            }
        };
        // SAFETY: as above; a plain visibility change.
        unsafe {
            let _ = controller.SetIsVisible(true);
            if let Ok(settings) = webview.Settings() {
                let _ = settings.SetIsStatusBarEnabled(false);
            }
        }
        let weak = Rc::downgrade(self);
        let starting = NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
            if let (Some(state), Some(args)) = (weak.upgrade(), args)
                && state.current(generation)
            {
                state.on_starting(&args)?;
            }
            Ok(())
        }));
        let weak = Rc::downgrade(self);
        let completed = NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
            if let (Some(state), Some(args)) = (weak.upgrade(), args)
                && state.current(generation)
            {
                state.on_completed(&args)?;
            }
            Ok(())
        }));
        let weak = Rc::downgrade(self);
        let failed = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
            if let (Some(state), Some(args)) = (weak.upgrade(), args)
                && state.current(generation)
            {
                state.on_process_failed(&args)?;
            }
            Ok(())
        }));
        let mut tokens = Tokens::default();
        // SAFETY: WebView2 calls on the live web view; the handlers hold
        // the view weakly, and go with it.
        let added = unsafe {
            webview
                .add_NavigationStarting(&starting, &mut tokens.starting)
                .and_then(|()| webview.add_NavigationCompleted(&completed, &mut tokens.completed))
                .and_then(|()| webview.add_ProcessFailed(&failed, &mut tokens.failed))
        };
        if let Err(error) = added {
            // SAFETY: a call on the live controller.
            unsafe {
                let _ = controller.Close();
            }
            self.report_failure(format!("cannot subscribe to WebView2 navigation: {error}"));
            return;
        }
        if !self.current(generation) {
            // A callback cannot have run here, but keep the invariant
            // explicit: never install a retired browser.
            // SAFETY: a call on the live web view and controller.
            unsafe {
                let _ = webview.remove_NavigationStarting(tokens.starting);
                let _ = webview.remove_NavigationCompleted(tokens.completed);
                let _ = webview.remove_ProcessFailed(tokens.failed);
                let _ = controller.Close();
            }
            return;
        }
        self.tokens.set(Some(tokens));
        *self.controller.borrow_mut() = Some(controller.clone());
        *self.webview.borrow_mut() = Some(webview.clone());
        self.applied.set(None);
        self.raster_scale.set(0.0);
        self.navigated.set(false);
        if let Some((width, height)) = self.client_size() {
            self.bounds.set((width, height));
        }
        self.apply_bounds(&controller);
        let source = self.src.borrow().clone();
        if !source.is_empty() {
            self.navigate_now(&webview, &source, generation);
        }
    }

    /// Applies the window's rasterization scale and the pixel bounds.
    ///
    /// The controller's bounds mode is pinned to raw pixels, so the bounds
    /// are the child window's client area as Windows reports it. The
    /// rasterization scale is the window's DPI over 96, which is what makes
    /// a 2x window render at `devicePixelRatio` 2 with a CSS viewport of
    /// half its pixel width. WebView2's own monitor-scale detection is
    /// turned off: this adapter drives the scale from the window's DPI.
    fn apply_bounds(&self, controller: &ICoreWebView2Controller) {
        let scale = self.scale();
        if (self.raster_scale.get() - scale).abs() > f32::EPSILON {
            match controller.cast::<ICoreWebView2Controller3>() {
                Ok(controller3) => {
                    // SAFETY: calls on the live controller; the bounds mode
                    // is set before the bounds, and the scale belongs to
                    // that mode.
                    unsafe {
                        let _ = controller3.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS);
                        let _ = controller3.SetShouldDetectMonitorScaleChanges(false);
                        let _ = controller3.SetRasterizationScale(scale as f64);
                    }
                    self.raster_scale.set(scale);
                }
                Err(_) => {
                    log::warn!(
                        "nsh-ext webview: WebView2 cannot set a rasterization scale; \
                         the page renders at 1x"
                    );
                    self.raster_scale.set(scale);
                }
            }
        }
        let (width, height) = self.bounds.get();
        if self.applied.get() == Some((width, height)) {
            return;
        }
        let rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        // SAFETY: a plain bounds change on the live controller.
        unsafe {
            let _ = controller.SetBounds(rect);
        }
        self.applied.set(Some((width, height)));
    }

    /// The child window's DPI in pixels per point.
    fn scale(&self) -> f32 {
        if self.child.is_invalid() {
            return 1.0;
        }
        // SAFETY: a DPI getter on this thread's window.
        let dpi = unsafe { GetDpiForWindow(self.child) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }

    /// The child window's client area in pixels; `None` before NSH places
    /// the window (there is nothing to fill yet).
    fn client_size(&self) -> Option<(i32, i32)> {
        if self.child.is_invalid() {
            return None;
        }
        let mut rect = RECT::default();
        // SAFETY: a plain client-size read on this thread's live window.
        if unsafe { GetClientRect(self.child, &mut rect) }.is_err() {
            return None;
        }
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        (width > 0 && height > 0).then_some((width, height))
    }

    /// Sizes the controller to the child window's client area.
    ///
    /// `draw` reports the component's frame in points. The window's pixel
    /// client area is the authority, because that is the area the page must
    /// fill; the frame's points are converted to pixels only before the
    /// window has a client area, so a host that never places the window
    /// still gets its frame.
    fn set_bounds(&self, width: f32, height: f32) {
        if self.disposed.get() {
            return;
        }
        let size = self.client_size().unwrap_or_else(|| {
            let scale = self.scale();
            (pixels(width, scale), pixels(height, scale))
        });
        self.apply_size(size.0, size.1);
    }

    /// Applies the size and re-reads the window's scale: the window may
    /// have moved to a monitor with a different DPI while keeping its size.
    fn apply_size(&self, width: i32, height: i32) {
        if self.bounds.get() != (width, height) {
            self.bounds.set((width, height));
        }
        let controller = self.controller.borrow().clone();
        if let Some(controller) = controller {
            self.apply_bounds(&controller);
        }
    }

    /// `WM_SIZE`: the child window's client area in pixels is the bounds.
    fn resized_from_window(&self, width: i32, height: i32) {
        if self.disposed.get() || width <= 0 || height <= 0 {
            return;
        }
        self.apply_size(width, height);
    }

    /// A top-level navigation started in WebView2.
    fn on_starting(
        self: &Rc<Self>,
        args: &ICoreWebView2NavigationStartingEventArgs,
    ) -> windows::core::Result<()> {
        if self.disposed.get() {
            return Ok(());
        }
        let mut native = 0u64;
        // SAFETY: a getter on the live event arguments.
        unsafe {
            args.NavigationId(&mut native)?;
        }
        let source = self.src.borrow().clone();
        if source.is_empty() {
            return Ok(());
        }
        let mut uri = PWSTR::null();
        // SAFETY: a getter on the live event arguments; `take_pwstr` frees
        // the string WebView2 allocated.
        let uri = unsafe { args.Uri(&mut uri) }
            .map(|()| take_pwstr(uri))
            .unwrap_or_default();
        if !self.navigated.get() && initial_blank(&uri, &source) {
            // The controller's initial blank document, not the source.
            return Ok(());
        }
        if let Some(active) = self.active.get() {
            if active.native == native {
                // A redirect (or a second starting callback) of the same
                // native navigation keeps the root identity.
                return Ok(());
            }
            // WebView2 started a different navigation: the old one is
            // cancelled explicitly rather than silently replaced.
            self.active.set(None);
            let cancelled = self.tracker.borrow_mut().cancel(active.navigation);
            self.publish(cancelled);
            if self.disposed.get() || self.src.borrow().as_str() != source {
                // Disposed, or a progress handler replaced the source
                // while the old navigation was cancelled; that navigation
                // owns the view.
                return Ok(());
            }
        }
        self.pending_failure.borrow_mut().take();
        self.navigated.set(true);
        let started = self.tracker.borrow_mut().start();
        let navigation = started.navigation;
        self.active.set(Some(Active { native, navigation }));
        self.publish(Some(started));
        Ok(())
    }

    /// WebView2 confirmed or failed the navigation it started.
    fn on_completed(
        self: &Rc<Self>,
        args: &ICoreWebView2NavigationCompletedEventArgs,
    ) -> windows::core::Result<()> {
        if self.disposed.get() {
            return Ok(());
        }
        let mut native = 0u64;
        // SAFETY: a getter on the live event arguments.
        unsafe {
            args.NavigationId(&mut native)?;
        }
        let Some(active) = self.active.get() else {
            return Ok(());
        };
        if active.native != native {
            // A late callback for a navigation a newer one replaced.
            return Ok(());
        }
        self.active.set(None);
        let mut success = windows::core::BOOL(0);
        // `IsSuccess` is the navigation outcome. An HTTP error page (404,
        // say) is still a completed navigation; a connection failure is
        // not. Do not infer failure from the HTTP status alone.
        // SAFETY: a getter on the live event arguments.
        let success = unsafe { args.IsSuccess(&mut success) }
            .map(|()| success.as_bool())
            .unwrap_or(false);
        if success {
            let finished = self.tracker.borrow_mut().finish(active.navigation);
            self.publish(finished);
        } else {
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS(0);
            // SAFETY: a getter on the live event arguments.
            unsafe {
                let _ = args.WebErrorStatus(&mut status);
            }
            if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED {
                let cancelled = self.tracker.borrow_mut().cancel(active.navigation);
                self.publish(cancelled);
                return Ok(());
            }
            let message = if self.track.get() {
                describe(status)
            } else {
                String::new()
            };
            let failed = self.tracker.borrow_mut().fail(active.navigation, message);
            self.publish(failed);
        }
        Ok(())
    }

    /// WebView2's renderer (or another process) failed. A pending
    /// navigation would otherwise stay active forever.
    ///
    /// The browser is not torn down here: this runs inside a WebView2
    /// callback. A later explicit source change retires it (closing a
    /// controller whose browser process exited is harmless), so a fresh
    /// source loads into a new browser instead of failing forever.
    fn on_process_failed(
        self: &Rc<Self>,
        args: &ICoreWebView2ProcessFailedEventArgs,
    ) -> windows::core::Result<()> {
        if self.disposed.get() {
            return Ok(());
        }
        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
        // SAFETY: a getter on the live event arguments.
        unsafe {
            args.ProcessFailedKind(&mut kind)?;
        }
        let message = process_failure(kind);
        if let Some(active) = self.active.take() {
            let failed = self.tracker.borrow_mut().fail(active.navigation, message);
            self.publish(failed);
        } else {
            log::warn!("nsh-ext webview: {message}");
        }
        Ok(())
    }

    /// Navigates a freshly made web view to `source`.
    fn navigate_now(self: &Rc<Self>, webview: &ICoreWebView2, source: &str, generation: u64) {
        if !self.current(generation) || self.src.borrow().as_str() != source {
            return;
        }
        let uri = HSTRING::from(source);
        // SAFETY: a call on the live web view with a valid string.
        if let Err(error) = unsafe { webview.Navigate(&uri) } {
            self.report_failure(format!("cannot navigate to {source}: {error}"));
        }
    }

    /// Reports a setup failure for the current source, with a real
    /// navigation identity even when no navigation could start.
    ///
    /// The identity advances while tracking is off too; its events are
    /// kept until tracking is enabled, so lifecycle and emission do not
    /// disagree.
    fn report_failure(self: &Rc<Self>, message: String) {
        if self.disposed.get() {
            return;
        }
        let generation = self.generation.get();
        let source = self.src.borrow().clone();
        if source.is_empty() {
            return;
        }
        {
            let mut reported = self.failure_reported.borrow_mut();
            if reported.as_deref() == Some(source.as_str()) {
                return;
            }
            *reported = Some(source.clone());
        }
        self.cancel_loading();
        if !self.current(generation) || self.src.borrow().as_str() != source {
            return;
        }
        let started = self.tracker.borrow_mut().start();
        let navigation = started.navigation;
        let failed = self
            .tracker
            .borrow_mut()
            .fail(navigation, message)
            .expect("a newly started navigation is active");
        if !self.track.get() {
            *self.pending_failure.borrow_mut() = Some(PendingFailure {
                source,
                started,
                failed,
            });
            return;
        }
        self.publish(Some(started));
        if self.disposed.get() {
            return;
        }
        // The failure is real for the identity just started, even if the
        // progress handler replaced the source while it was reported.
        self.publish(Some(failed));
    }

    /// Emits a setup failure that was made while tracking was off.
    fn report_pending_failure(&self) {
        if self.disposed.get() {
            return;
        }
        let Some(PendingFailure {
            source,
            started,
            failed,
        }) = self.pending_failure.borrow_mut().take()
        else {
            return;
        };
        if self.src.borrow().as_str() != source {
            return;
        }
        self.publish(Some(started));
        if self.disposed.get() {
            return;
        }
        self.publish(Some(failed));
    }

    /// Emits `progress` only while tracking is enabled, without holding a
    /// borrow while the handler runs.
    fn publish(&self, progress: Option<LoadProgress>) {
        let Some(progress) = progress else {
            return;
        };
        if self.disposed.get() || !self.track.get() {
            return;
        }
        let handler = self.handler.borrow().clone();
        if let Some(handler) = handler {
            handler(&progress);
        }
    }
}

impl Drop for HostState {
    fn drop(&mut self) {
        self.disposed.set(true);
        // Dropped first so no callback can reach the app during teardown.
        self.handler.get_mut().take();
        self.pending_failure.get_mut().take();
        WINDOWS.with(|windows| {
            windows.borrow_mut().remove(&(self.child.0 as usize));
        });
        let tokens = self.tokens.take();
        if let Some(webview) = self.webview.get_mut().take()
            && let Some(tokens) = tokens
        {
            // SAFETY: tokens of handlers added on this web view.
            unsafe {
                let _ = webview.remove_NavigationStarting(tokens.starting);
                let _ = webview.remove_NavigationCompleted(tokens.completed);
                let _ = webview.remove_ProcessFailed(tokens.failed);
            }
        }
        if let Some(controller) = self.controller.get_mut().take() {
            // SAFETY: a plain close on the live controller.
            unsafe {
                let _ = controller.SetIsVisible(false);
                let _ = controller.Close();
            }
        }
        if !self.child.is_invalid() {
            // SAFETY: a plain destroy of the window this host created.
            unsafe {
                let _ = DestroyWindow(self.child);
            }
        }
    }
}

/// Closes a controller made for a view that is gone or retired.
fn close(controller: Option<ICoreWebView2Controller>) {
    if let Some(controller) = controller {
        // SAFETY: closes a controller nothing else uses.
        unsafe {
            let _ = controller.Close();
        }
    }
}

/// The child window's messages: `WM_SIZE` keeps the controller at the
/// window's actual pixel size even if the component's `draw` is skipped.
unsafe extern "system" fn host_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => {
            let mut rect = RECT::default();
            // SAFETY: a plain client-size read on this thread's window.
            if unsafe { GetClientRect(window, &mut rect) }.is_ok() {
                let state = WINDOWS
                    .with(|windows| windows.borrow().get(&(window.0 as usize)).cloned())
                    .and_then(|weak| weak.upgrade());
                if let Some(state) = state {
                    state.resized_from_window(rect.right - rect.left, rect.bottom - rect.top);
                }
            }
            // SAFETY: forwarding a message the adapter does not handle.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_NCDESTROY => {
            WINDOWS.with(|windows| {
                windows.borrow_mut().remove(&(window.0 as usize));
            });
            // SAFETY: forwarding the final message.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        // SAFETY: forwarding a message the adapter does not handle.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Makes the child window the controller fills.
fn create_child(parent: Option<HWND>) -> windows::core::Result<HWND> {
    let instance = instance()?;
    static REGISTERED: OnceLock<()> = OnceLock::new();
    let _ = REGISTERED.get_or_init(|| {
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(host_proc),
            hInstance: instance,
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: the class structure is valid for the call and its name
        // lives for the program; a duplicate registration is harmless.
        unsafe {
            let _ = RegisterClassW(&class);
        }
    });
    let style = if parent.is_some() {
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN
    } else {
        WS_POPUP | WS_CLIPSIBLINGS | WS_CLIPCHILDREN
    };
    // SAFETY: plain window creation with a registered class; NSH adopts
    // the window when the component is placed.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!(""),
            style,
            0,
            0,
            0,
            0,
            parent,
            None,
            Some(instance),
            None,
        )
    }
}

/// This module's handle, as the window class's instance.
fn instance() -> windows::core::Result<HINSTANCE> {
    // SAFETY: the module handle of this executable.
    unsafe { GetModuleHandleW(None) }.map(|module| HINSTANCE(module.0))
}

/// Whether a starting navigation is the controller's initial blank
/// document rather than `source`.
fn initial_blank(uri: &str, source: &str) -> bool {
    uri.eq_ignore_ascii_case("about:blank") && !source.eq_ignore_ascii_case("about:blank")
}

/// Whole pixels for a length in points at `scale`, never negative or
/// non-finite.
fn pixels(value: f32, scale: f32) -> i32 {
    if value.is_finite() && scale.is_finite() && scale > 0.0 {
        (value.max(0.0) * scale).round() as i32
    } else {
        0
    }
}

/// A readable message for WebView2's navigation error.
fn describe(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> String {
    let reason = match status {
        COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT => "cannot connect",
        COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => "timed out",
        COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED => "disconnected",
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED => "connection aborted",
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET => "connection reset",
        COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED => "host name not resolved",
        COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE => "server unreachable",
        COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED => "redirect failed",
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => "navigation canceled",
        COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE => {
            "invalid server response"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT => {
            "certificate common name is incorrect"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED => "certificate expired",
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID => "certificate is invalid",
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED => "certificate revoked",
        COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS => {
            "client certificate contains errors"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR => "unexpected error",
        _ => "navigation failed",
    };
    format!("{reason} (WebView2 status {})", status.0)
}

/// A readable message for a WebView2 process failure.
fn process_failure(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> String {
    let reason = match kind {
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
            "the WebView2 browser process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED => {
            "a WebView2 frame renderer process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED => "the WebView2 GPU process exited",
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_BROKER_PROCESS_EXITED => {
            "the WebView2 PPAPI broker process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_PPAPI_PLUGIN_PROCESS_EXITED => {
            "a WebView2 PPAPI plugin process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
            "the WebView2 renderer process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
            "the WebView2 renderer process stopped responding"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_SANDBOX_HELPER_PROCESS_EXITED => {
            "the WebView2 sandbox helper process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED => {
            "a WebView2 utility process exited"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED => {
            "an unknown WebView2 process exited"
        }
        _ => "a WebView2 process failed",
    };
    format!("{reason} (kind {})", kind.0)
}
