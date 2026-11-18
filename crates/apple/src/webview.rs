//! The extension-owned browser view: one stable native container per
//! component and one `WKWebView` per applied `src`.
//!
//! The component's `NSView`/`UIView` is the view NSH lays out and it never
//! changes, so replacing a browser cannot disturb the component's placement.
//! Every explicit `src` change destroys the current `WKWebView`, its
//! navigation delegate and its `estimatedProgress` KVO observer, and creates a
//! fresh `WKWebView` with a new callback generation inside the same container.
//! A callback from a discarded browser therefore runs against a stale
//! generation and changes nothing; it can never be mistaken for the
//! replacement's navigation. Disposing the old browser also destroys its
//! page, so the old page's timers and workers stop, and its history is gone.
//! All browser state (including the native navigation identities retained to
//! fence late starts) is released with that browser; only identities needed by
//! the current browser are retained, and a terminal callback releases the ones
//! that are no longer useful.
//!
//! The progress identity counter lives in the component, not the browser: it
//! continues across `src` changes and starts at one for a new component.
//! `trackProgress` is only an emission gate, so it is applied to the same
//! tracker regardless of which browser is current.
//!
//! The dynamic WebKit loading and the KVO/delegate shape follow Native
//! Shell's own WebKit host (Apache-2.0, <https://github.com/longbridge/nsh>),
//! adapted to this extension's container and progress contract.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr,
    rc::{Rc, Weak},
    sync::LazyLock,
};

use nsh_core::JsonValue;
use nsh_ext::{LoadProgress, PROGRESS_EVENT};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject},
    sel,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRect, NSString, NSURL, NSURLRequest};

#[cfg(target_os = "macos")]
use objc2_app_kit::NSView;
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIView;

use crate::platform::{ComponentContext, NativeComponent};
use crate::tracker::NavigationTracker;

#[cfg(target_os = "macos")]
type ContainerView = NSView;
#[cfg(target_os = "ios")]
type ContainerView = UIView;

/// The KVO key WebKit exposes its measured load fraction under.
const PROGRESS_KEY: &str = "estimatedProgress";
/// `NSURLErrorCancelled`, delivered when a navigation is stopped.
const NSURL_ERROR_CANCELLED: isize = -999;
/// `NSViewWidthSizable | NSViewHeightSizable`; UIKit's
/// `UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight` uses
/// the same numeric values.
const AUTORESIZING_FLEXIBLE: usize = 2 | 16;

/// A native navigation retained so its identity cannot be reused.
struct NativeId {
    key: usize,
    /// Held while a could-still-arrive callback of this navigation is
    /// possible, so its address cannot be reused by another one.
    _navigation: Retained<AnyObject>,
}

impl NativeId {
    fn new(navigation: Retained<AnyObject>) -> Self {
        let key = navigation_key(&navigation);
        Self {
            key,
            _navigation: navigation,
        }
    }

    fn from_ref(navigation: &AnyObject) -> Self {
        Self::new(retain_navigation(navigation))
    }
}

fn navigation_key(navigation: &AnyObject) -> usize {
    navigation as *const AnyObject as usize
}

fn retain_navigation(navigation: &AnyObject) -> Retained<AnyObject> {
    // SAFETY: WebKit passes a live WKNavigation to the delegate; retaining it
    // keeps its address unique while a stale start is possible.
    unsafe {
        Retained::retain(navigation as *const AnyObject as *mut AnyObject)
            .expect("a live WKNavigation")
    }
}

/// One extension WebView: a stable native container plus its current browser.
///
/// Created through [`crate::factory`]; the adapter sets the `src` and
/// `trackProgress` props from the shared `nsh_ext::WebView` wrapper. Native
/// components are trusted host code: WebKit loads the URLs the embedding app
/// gives it, and this adapter does not apply Native Shell's URL grants to
/// them.
pub(crate) struct WebViewComponent {
    state: Rc<ComponentState>,
    /// Owns the one deferred `src` application of the current props batch.
    driver: Retained<Driver>,
}

impl WebViewComponent {
    pub(crate) fn new(context: ComponentContext) -> Self {
        let mtm = MainThreadMarker::new()
            .expect("nsh-ext-apple WebViewComponent must be created on the main thread");
        let state = ComponentState::new(context, mtm);
        let driver = Driver::new(mtm, state.clone());
        Self { state, driver }
    }

    fn apply_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        match name {
            "src" => {
                let source = value.and_then(|value| value.as_str()).unwrap_or_default();
                let previous = self.state.set_source(source.to_owned());
                if source.is_empty() {
                    // Stop and destroy the page now; a queued initial load
                    // would otherwise apply the empty source a second time.
                    self.cancel_scheduled_apply();
                    self.state.apply();
                } else if previous != source {
                    self.schedule_apply();
                }
            }
            "trackProgress" => {
                let enabled = value.and_then(|value| value.as_bool()).unwrap_or(false);
                self.state.set_track(enabled);
            }
            _ => {}
        }
    }

    /// Defers the load until the current props batch has been installed, so
    /// the initial `src` sees `trackProgress` set after it. Repeated calls
    /// coalesce into the one queued application.
    fn schedule_apply(&self) {
        if self.state.scheduled.replace(true) {
            return;
        }
        // SAFETY: the driver implements `nshApplySource` and is alive; the
        // nil object and zero delay are the documented performSelector form.
        let _: () = unsafe {
            msg_send![
                &*self.driver,
                performSelector: sel!(nshApplySource),
                withObject: ptr::null::<AnyObject>(),
                afterDelay: 0.0f64
            ]
        };
    }

    fn cancel_scheduled_apply(&self) {
        self.state.scheduled.set(false);
        let Some(object_class) = AnyClass::get(c"NSObject") else {
            return;
        };
        // SAFETY: cancels the pending `nshApplySource` request on this
        // driver; it is the documented NSObject class method.
        let _: () = unsafe {
            msg_send![
                object_class,
                cancelPreviousPerformRequestsWithTarget: &*self.driver,
                selector: sel!(nshApplySource),
                object: ptr::null::<AnyObject>()
            ]
        };
    }
}

#[cfg(target_os = "macos")]
fn make_container(mtm: MainThreadMarker) -> Retained<ContainerView> {
    NSView::new(mtm)
}

#[cfg(target_os = "ios")]
fn make_container(mtm: MainThreadMarker) -> Retained<ContainerView> {
    UIView::new(mtm)
}

/// Puts a new browser in the stable container: it fills the container now and
/// follows every later resize through the autoresizing mask.
fn attach(container: &ContainerView, web: &AnyObject) {
    // SAFETY: both are live views on the main thread, and a WKWebView is an
    // NSView/UIView. Flexible width and height keep the page sized to the
    // container as NSH lays it out.
    unsafe {
        let bounds: NSRect = msg_send![container, bounds];
        let _: () = msg_send![web, setFrame: bounds];
        let _: () = msg_send![web, setAutoresizingMask: AUTORESIZING_FLEXIBLE];
        let _: () = msg_send![container, addSubview: web];
    }
}

#[cfg(target_os = "macos")]
impl NativeComponent for WebViewComponent {
    fn view(&self) -> Retained<NSView> {
        self.state.container.clone()
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        self.apply_prop(name, value);
    }
}

#[cfg(target_os = "ios")]
impl NativeComponent for WebViewComponent {
    fn view(&self) -> &UIView {
        &self.state.container
    }

    fn set_prop(&mut self, name: &str, value: Option<&JsonValue>) {
        self.apply_prop(name, value);
    }
}

impl Drop for WebViewComponent {
    fn drop(&mut self) {
        // No callback from here on can change state or emit an event. The
        // browser disposal itself is silent: a removed component does not
        // report a cancellation or terminal state for the page it destroys.
        self.state.alive.set(false);
        self.cancel_scheduled_apply();
        let _ = self.state.browser.borrow_mut().take();
    }
}

/// Per-component state shared by the component and every browser generation.
struct ComponentState {
    context: ComponentContext,
    tracker: RefCell<NavigationTracker>,
    /// The latest `src`, applied on the next main-loop turn when non-empty.
    source: RefCell<String>,
    /// False once the component is dropped; callbacks then do nothing.
    alive: Cell<bool>,
    /// A deferred source application is already queued.
    scheduled: Cell<bool>,
    /// Generation of the current browser; every callback must match it.
    generation: Cell<u64>,
    /// The stable native view NSH lays out; it outlives every browser.
    container: Retained<ContainerView>,
    /// The current browser, replaced on every applied `src` change.
    browser: RefCell<Option<Browser>>,
}

impl ComponentState {
    fn new(context: ComponentContext, mtm: MainThreadMarker) -> Rc<Self> {
        Rc::new(Self {
            context,
            tracker: RefCell::new(NavigationTracker::default()),
            source: RefCell::new(String::new()),
            alive: Cell::new(true),
            scheduled: Cell::new(false),
            generation: Cell::new(0),
            container: make_container(mtm),
            browser: RefCell::new(None),
        })
    }

    /// Stores the latest source and returns the previous one.
    fn set_source(&self, source: String) -> String {
        std::mem::replace(&mut *self.source.borrow_mut(), source)
    }

    fn set_track(&self, enabled: bool) {
        self.tracker.borrow_mut().set_track(enabled);
    }

    fn emit(&self, event: &LoadProgress) {
        if self.alive.get() {
            self.context.emit(PROGRESS_EVENT, event.to_value());
        }
    }

    fn emit_events(&self, events: &crate::tracker::Events) {
        for event in events.iter() {
            self.emit(event);
        }
    }

    /// Applies the latest source: empty destroys the page, non-empty loads in
    /// a fresh browser unless the current one already holds that source.
    fn apply(self: &Rc<Self>) {
        if !self.alive.get() {
            return;
        }
        let source = self.source.borrow().clone();
        let unchanged = match self.browser.borrow().as_ref() {
            Some(browser) => browser.source == source,
            None => source.is_empty(),
        };
        if unchanged {
            return;
        }
        self.detach_source();
        // A progress handler can change `src` while the cancellation is
        // delivered; a nested application owns the view then.
        if self.source.borrow().as_str() != source {
            return;
        }
        if source.is_empty() {
            return;
        }
        self.load_source(&source);
    }

    /// Stops and disposes the current browser. The page is destroyed with it,
    /// so a cleared `src` stops the old page's timers and workers rather than
    /// merely replacing its document. Only an active load is reported, as a
    /// cancellation; the disposal itself emits nothing.
    fn detach_source(&self) {
        // Fence every callback from this browser before it can run during
        // teardown.
        self.generation.set(self.generation.get().wrapping_add(1));
        let previous = self.browser.borrow_mut().take();
        let event = self.tracker.borrow_mut().replaced();
        if let Some(event) = event {
            self.emit(&event);
        }
        drop(previous);
    }

    /// Creates a fresh browser generation for `source` and starts loading.
    fn load_source(self: &Rc<Self>, source: &str) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let mtm = MainThreadMarker::new()
            .expect("nsh-ext-apple source application must run on the main thread");
        let Some(browser) = Browser::new(mtm, self, generation, source.to_owned()) else {
            log::error!("nsh-ext-apple: WebKit is unavailable; showing an empty native view");
            let events = self
                .tracker
                .borrow_mut()
                .failed_to_start(|| "WebKit is unavailable".to_owned());
            self.emit_events(&events);
            return;
        };
        let state = browser.state.clone();
        // Install the browser before loading: a callback can then only be
        // judged against the generation that produced it.
        *self.browser.borrow_mut() = Some(browser);
        state.load(source);
    }
}

/// One `WKWebView`, its delegate/KVO handler and its native identities.
struct Browser {
    handler: Retained<Handler>,
    state: Rc<BrowserState>,
    /// The source this browser was created for.
    source: String,
}

impl Browser {
    fn new(
        mtm: MainThreadMarker,
        shared: &Rc<ComponentState>,
        generation: u64,
        source: String,
    ) -> Option<Self> {
        let web = make_web_view()?;
        let state = Rc::new(BrowserState {
            shared: Rc::downgrade(shared),
            generation,
            web: web.clone(),
            observed: Cell::new(false),
            expected: RefCell::new(None),
            current_native: RefCell::new(None),
            retired: RefCell::new(Vec::new()),
        });
        let handler = Handler::new(mtm, state.clone());
        // SAFETY: `web` is a live WKWebView on the main thread; `handler`
        // lives as long as this browser.
        unsafe {
            let _: () = msg_send![&*web, setNavigationDelegate: &*handler];
            let key = NSString::from_str(PROGRESS_KEY);
            let _: () = msg_send![
                &*web,
                addObserver: &*handler,
                forKeyPath: &*key,
                options: 1usize,
                context: ptr::null_mut::<c_void>()
            ];
        }
        state.observed.set(true);
        attach(&shared.container, &web);
        Some(Self {
            handler,
            state,
            source,
        })
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // SAFETY: the web view is live and `handler` was registered as its
        // KVO observer and navigation delegate in `Browser::new`. The
        // observer is removed before the delegate, and both before the web
        // view and its page are released.
        unsafe {
            if self.state.observed.replace(false) {
                let key = NSString::from_str(PROGRESS_KEY);
                let _: () = msg_send![
                    &*self.state.web,
                    removeObserver: &*self.handler,
                    forKeyPath: &*key
                ];
            }
            let _: () = msg_send![&*self.state.web, stopLoading];
            let _: () =
                msg_send![&*self.state.web, setNavigationDelegate: ptr::null::<AnyObject>()];
            let _: () = msg_send![&*self.state.web, removeFromSuperview];
        }
    }
}

/// The state of one browser generation.
struct BrowserState {
    /// Cleared when the component is gone; never a strong cycle.
    shared: Weak<ComponentState>,
    /// The generation the component must still agree with.
    generation: u64,
    web: Retained<AnyObject>,
    /// The `estimatedProgress` observation is registered.
    observed: Cell<bool>,
    /// The navigation `loadRequest:` returned, waiting for its didStart.
    expected: RefCell<Option<NativeId>>,
    /// The navigation whose callbacks are authoritative now.
    current_native: RefCell<Option<NativeId>>,
    /// Navigations whose late didStart must be ignored. Retaining each keeps
    /// its address from being reused by another navigation; a terminal
    /// callback removes it and the whole list dies with the browser.
    retired: RefCell<Vec<NativeId>>,
}

impl BrowserState {
    /// The component, only while this browser is still the current one.
    fn live(&self) -> Option<Rc<ComponentState>> {
        let shared = self.shared.upgrade()?;
        if !shared.alive.get() || shared.generation.get() != self.generation {
            return None;
        }
        Some(shared)
    }

    fn is_current_web(&self, web: &AnyObject) -> bool {
        ptr::eq(Retained::as_ptr(&self.web), web as *const AnyObject)
    }

    fn retire_expected(&self) {
        if let Some(id) = self.expected.borrow_mut().take() {
            self.retired.borrow_mut().push(id);
        }
    }

    fn retire_current_native(&self) {
        if let Some(id) = self.current_native.borrow_mut().take() {
            self.retired.borrow_mut().push(id);
        }
    }

    fn is_retired(&self, key: usize) -> bool {
        self.retired.borrow().iter().any(|id| id.key == key)
    }

    fn forget_native(&self, key: usize) {
        let expected = {
            let slot = self.expected.borrow();
            slot.as_ref().is_some_and(|id| id.key == key)
        };
        if expected {
            let _ = self.expected.borrow_mut().take();
        }
        let current = {
            let slot = self.current_native.borrow();
            slot.as_ref().is_some_and(|id| id.key == key)
        };
        if current {
            let _ = self.current_native.borrow_mut().take();
        }
        // A terminal callback releases the identity kept only to fence a
        // start that can no longer arrive.
        self.retired.borrow_mut().retain(|id| id.key != key);
    }

    fn expect_navigation(&self, navigation: Retained<AnyObject>) {
        // A newer loadRequest supersedes the one still waiting for didStart.
        self.retire_expected();
        *self.expected.borrow_mut() = Some(NativeId::new(navigation));
    }

    fn load(&self, source: &str) {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(source)) else {
            self.failed_to_start(|| format!("invalid URL: {source}"));
            return;
        };
        let request = NSURLRequest::requestWithURL(&url);
        // SAFETY: `web` is this generation's live WKWebView and `request` is
        // a valid request.
        let navigation: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*self.web, loadRequest: &*request] };
        match navigation {
            Some(navigation) => self.expect_navigation(navigation),
            None => self.failed_to_start(|| "WKWebView did not start the navigation".to_owned()),
        }
    }

    fn failed_to_start(&self, message: impl FnOnce() -> String) {
        let Some(shared) = self.live() else {
            return;
        };
        let events = shared.tracker.borrow_mut().failed_to_start(message);
        shared.emit_events(&events);
    }

    fn navigation_started(&self, web: &AnyObject, navigation: Option<&AnyObject>) {
        let Some(shared) = self.live() else {
            return;
        };
        if !self.is_current_web(web) {
            return;
        }
        let Some(navigation) = navigation else {
            return;
        };
        let key = navigation_key(navigation);

        if self.is_retired(key) {
            // A replaced navigation's late start: its native identity is
            // still retained, so no new navigation can reuse the address.
            return;
        }
        let already_current = {
            let slot = self.current_native.borrow();
            slot.as_ref().is_some_and(|id| id.key == key)
        };
        if already_current {
            // A duplicate start for the same navigation keeps its identity.
            return;
        }

        let expected = {
            let mut slot = self.expected.borrow_mut();
            if slot.as_ref().is_some_and(|id| id.key == key) {
                slot.take()
            } else {
                None
            }
        };
        if expected.is_none() {
            // A page- or user-originated navigation supersedes the pending
            // loadRequest, whose late start must not take the view back.
            self.retire_expected();
        }
        // A navigation that already started cannot start again, but its
        // identity is retained until its terminal callback so its address
        // cannot be reused by a later navigation.
        self.retire_current_native();
        let current = expected.unwrap_or_else(|| NativeId::from_ref(navigation));
        *self.current_native.borrow_mut() = Some(current);
        let events = shared.tracker.borrow_mut().started(key);
        shared.emit_events(&events);
    }

    fn progress_changed(&self, web: &AnyObject, fraction: f64) {
        let Some(shared) = self.live() else {
            return;
        };
        if !self.is_current_web(web) {
            return;
        }
        let event = shared.tracker.borrow_mut().estimate(fraction);
        if let Some(event) = event {
            shared.emit(&event);
        }
    }

    /// WebKit also calls this for HTTP 4xx/5xx responses: an error status is
    /// not a native load failure, so it is deliberately not inspected here.
    /// A 404 or 500 page with a document reaches `Finished`.
    fn navigation_finished(&self, web: &AnyObject, navigation: Option<&AnyObject>) {
        let Some(shared) = self.live() else {
            return;
        };
        if !self.is_current_web(web) {
            return;
        }
        let Some(navigation) = navigation else {
            return;
        };
        let key = navigation_key(navigation);
        self.forget_native(key);
        let event = shared.tracker.borrow_mut().finished(key);
        if let Some(event) = event {
            shared.emit(&event);
        }
    }

    fn navigation_failed(
        &self,
        web: &AnyObject,
        navigation: Option<&AnyObject>,
        error: &AnyObject,
    ) {
        let Some(shared) = self.live() else {
            return;
        };
        if !self.is_current_web(web) {
            return;
        }
        let Some(navigation) = navigation else {
            return;
        };
        let key = navigation_key(navigation);
        self.forget_native(key);
        let tracking = shared.tracker.borrow().track_enabled();
        let failure = describe_failure(error, tracking);
        let event = if failure.cancelled {
            shared.tracker.borrow_mut().cancelled(key)
        } else {
            shared.tracker.borrow_mut().failed(key, failure.message)
        };
        if let Some(event) = event {
            shared.emit(&event);
        }
    }

    fn process_terminated(&self, web: &AnyObject) {
        let Some(shared) = self.live() else {
            return;
        };
        if !self.is_current_web(web) {
            return;
        }
        let _ = self.current_native.borrow_mut().take();
        let event = shared.tracker.borrow_mut().terminated();
        if let Some(event) = event {
            shared.emit(&event);
        }
    }
}

struct Failure {
    cancelled: bool,
    message: String,
}

/// Reads the `NSError` WebKit reports, separating a cancelled navigation
/// (the host stopped or replaced it) from a genuine load failure.
fn describe_failure(error: &AnyObject, with_message: bool) -> Failure {
    // SAFETY: WebKit's delegate passes an NSError and these are its documented
    // getters.
    let (domain, code, description): (Retained<NSString>, isize, Retained<NSString>) = unsafe {
        (
            msg_send![error, domain],
            msg_send![error, code],
            msg_send![error, localizedDescription],
        )
    };
    let domain = domain.to_string();
    let cancelled = domain == "NSURLErrorDomain" && code == NSURL_ERROR_CANCELLED;
    let message = if with_message {
        let message = description.to_string();
        if message.is_empty() {
            format!("navigation failed ({domain} {code})")
        } else {
            message
        }
    } else {
        String::new()
    };
    Failure { cancelled, message }
}

struct DriverIvars {
    state: Rc<ComponentState>,
}

define_class!(
    /// Owns the deferred first application of `src` for one component. It is
    /// independent of the browser so replacing the browser cannot drop a
    /// queued application.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DriverIvars]
    #[name = "NshExtWebViewDriver"]
    struct Driver;

    unsafe impl NSObjectProtocol for Driver {}

    impl Driver {
        #[unsafe(method(nshApplySource))]
        fn apply_source(&self) {
            if !self.ivars().state.alive.get() || !self.ivars().state.scheduled.replace(false) {
                return;
            }
            let state = self.ivars().state.clone();
            state.apply();
        }
    }
);

impl Driver {
    fn new(mtm: MainThreadMarker, state: Rc<ComponentState>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DriverIvars { state });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

struct HandlerIvars {
    state: Rc<BrowserState>,
}

define_class!(
    /// Navigation delegate and `estimatedProgress` observer of one browser.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = HandlerIvars]
    #[name = "NshExtWebViewHandler"]
    struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    impl Handler {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, web: &AnyObject, navigation: Option<&AnyObject>) {
            self.ivars().state.navigation_started(web, navigation);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web: &AnyObject, navigation: Option<&AnyObject>) {
            self.ivars().state.navigation_finished(web, navigation);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, web: &AnyObject, navigation: Option<&AnyObject>, error: &AnyObject) {
            self.ivars().state.navigation_failed(web, navigation, error);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            web: &AnyObject,
            navigation: Option<&AnyObject>,
            error: &AnyObject,
        ) {
            self.ivars().state.navigation_failed(web, navigation, error);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_terminated(&self, web: &AnyObject) {
            self.ivars().state.process_terminated(web);
        }

        /// WebKit's measured fraction. The observed object must be this
        /// browser's web view; the browser generation check then rejects a
        /// discarded view's observation.
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            key_path: &NSString,
            object: &AnyObject,
            _change: &AnyObject,
            _context: *mut c_void,
        ) {
            if key_path.to_string() != PROGRESS_KEY {
                return;
            }
            // SAFETY: KVO calls this for the WKWebView the observation was
            // added to, and `estimatedProgress` is its documented getter.
            let fraction: f64 = unsafe { msg_send![object, estimatedProgress] };
            self.ivars().state.progress_changed(object, fraction);
        }
    }
);

impl Handler {
    fn new(mtm: MainThreadMarker, state: Rc<BrowserState>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { state });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Loads WebKit on first use, so an app without this component does not pay
/// for the framework at launch.
fn webkit() -> bool {
    static LOADED: LazyLock<bool> = LazyLock::new(|| {
        let path = c"/System/Library/Frameworks/WebKit.framework/WebKit";
        // SAFETY: loads a system framework; the handle is deliberately never
        // closed.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) };
        if handle.is_null() {
            log::error!("nsh-ext-apple: cannot load WebKit");
        }
        !handle.is_null()
    });
    *LOADED
}

fn make_web_view() -> Option<Retained<AnyObject>> {
    if !webkit() {
        return None;
    }
    let web_class = AnyClass::get(c"WKWebView")?;
    let configuration_class = AnyClass::get(c"WKWebViewConfiguration")?;
    // SAFETY: WebKit's documented initializer, called on the main thread.
    unsafe {
        let configuration: Retained<AnyObject> = msg_send![configuration_class, new];
        let allocated: Allocated<AnyObject> = msg_send![web_class, alloc];
        let web: Retained<AnyObject> = msg_send![
            allocated,
            initWithFrame: NSRect::ZERO,
            configuration: &*configuration
        ];
        Some(web)
    }
}
