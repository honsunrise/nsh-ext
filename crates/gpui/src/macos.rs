//! WKWebView, a child view of GPUI's window.
//!
//! WebKit's own `WKNavigationDelegate` callbacks are this backend's lifecycle,
//! and the delegate WebKit calls is this extension's. Native Shell's web view
//! crate installs wry's own navigation delegate; the extension wraps it in a
//! proxy that forwards every method wry implements (navigation and response
//! policy, the download hand-offs, commit-time script injection and wry's
//! page-load callback) and adds the callbacks that are the lifecycle:
//!
//! - `webView:didStartProvisionalNavigation:` starts a navigation, at the
//!   moment WebKit has a `WKNavigation` object for it;
//! - `webView:didFinishNavigation:` is success;
//! - `webView:didFailProvisionalNavigation:withError:` and
//!   `webView:didFailNavigation:withError:` are failure, and an error that
//!   means cancellation (`NSURLErrorCancelled`, or a navigation a policy turned
//!   into a download) is a stop, not a failure.
//!
//! Each of those callbacks names its `WKNavigation`, which this backend retains
//! as the identity of the navigation in flight. A callback for another
//! navigation, or for one that already ended, is dropped: a late completion can
//! never finish a replacement, and success and failure always belong to the
//! navigation WebKit named. The identity is cleared when the navigation ends,
//! and every subscription is removed with the view.
//!
//! `estimatedProgress` is observed with key-value observing for the engine's
//! own estimate. It is never used to decide success or failure, and it is only
//! reported while an identified navigation is in flight.

use std::{cell::RefCell, ffi::c_void, rc::Rc, sync::Arc};

use block2::DynBlock;
use nsh_platform_gpui::gpui::{App, Bounds, Pixels, Window};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, ProtocolObject, Sel},
};
use objc2_foundation::{
    NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions,
    NSObjectNSKeyValueObserverRegistration as _, NSObjectProtocol, NSString, NSURLErrorCancelled,
};
#[allow(deprecated)]
use objc2_web_kit::WebKitErrorFrameLoadInterruptedByPolicyChange;
use objc2_web_kit::{
    WKDownload, WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKWebView,
};
use wry::{
    Rect, WebViewExtMacOS as _, WryWebView,
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
    /// Builds a hidden web view as a child view of `window`.
    pub(crate) fn build(
        builder: wry::WebViewBuilder<'_>,
        window: &mut Window,
        _cx: &mut App,
        sink: Arc<Sink>,
    ) -> Result<Self, String> {
        let webview = Rc::new(
            builder
                .build_as_child(&*window)
                .map_err(|error| format!("WKWebView could not make a web view: {error}"))?,
        );
        let progress = Progress::attach(&webview, sink.clone())?;
        Ok(Self {
            sink,
            progress: Some(progress),
            webview,
        })
    }

    /// Loads `url`, reporting a request WebKit never accepted as a failure.
    pub(crate) fn load_url(&self, url: &str) -> Result<(), String> {
        self.webview.load_url(url).map_err(|error| {
            let message = format!("WKWebView cannot load {url}: {error}");
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
        // The delegate and the observations go before the web view they use.
        self.progress.take();
    }
}

/// The navigation whose callbacks this view accepts.
#[derive(Default)]
enum Native {
    /// No navigation is in flight (none started, or the last one ended).
    #[default]
    Idle,
    /// A navigation started without a `WKNavigation` object; it is accepted
    /// until it ends, but a callback that does name a navigation is not.
    Unidentified,
    /// The navigation WebKit identified with this object.
    Identified(Retained<WKNavigation>),
}

impl Native {
    fn in_flight(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Whether `navigation` is the navigation callbacks are accepted for.
    fn matches(&self, navigation: Option<&WKNavigation>) -> bool {
        match (self, navigation) {
            (Self::Identified(current), Some(navigation)) => {
                std::ptr::eq(Retained::as_ptr(current), navigation)
            }
            (Self::Unidentified, _) => true,
            _ => false,
        }
    }
}

/// The retained identity of the navigation in flight.
type Identity = Rc<RefCell<Native>>;

/// Starts tracking `navigation` as the one in flight.
fn begin(identity: &RefCell<Native>, navigation: Option<&WKNavigation>) {
    // Retaining the navigation keeps its identity comparable until it is
    // replaced, even after WebKit has finished with it.
    let begun = navigation.map_or(Native::Unidentified, |navigation| {
        Native::Identified(navigation.retain())
    });
    *identity.borrow_mut() = begun;
}

/// Ends the navigation this callback names; `false` if it was not the one in
/// flight (a late callback of a navigation that already ended).
fn end(identity: &RefCell<Native>, navigation: Option<&WKNavigation>) -> bool {
    let mut current = identity.borrow_mut();
    if !current.matches(navigation) {
        return false;
    }
    *current = Native::Idle;
    true
}

/// The extension's `WKNavigationDelegate`: the lifecycle plus a forwarding
/// proxy for wry's own delegate.
struct NavigationDelegateIvars {
    sink: Arc<Sink>,
    identity: Identity,
    forward: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NshExtGpuiNavigationDelegate"]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationDelegateIvars]
    struct NavigationDelegate;

    unsafe impl NSObjectProtocol for NavigationDelegate {}

    unsafe impl WKNavigationDelegate for NavigationDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, _web_view: &WKWebView, navigation: Option<&WKNavigation>) {
            begin(&self.ivars().identity, navigation);
            self.ivars().sink.started();
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, web_view: &WKWebView, navigation: Option<&WKNavigation>) {
            self.forward_commit(web_view, navigation);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web_view: &WKWebView, navigation: Option<&WKNavigation>) {
            self.forward_finish(web_view, navigation);
            if end(&self.ivars().identity, navigation) {
                self.ivars().sink.finished();
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            web_view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.forward_provisional_failure(web_view, navigation, error);
            self.failed(navigation, error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            web_view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.forward_failure(web_view, navigation, error);
            self.failed(navigation, error);
        }

        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_navigation_action(
            &self,
            web_view: &WKWebView,
            action: &WKNavigationAction,
            decision_handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            self.forward_navigation_action(web_view, action, decision_handler);
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_navigation_response(
            &self,
            web_view: &WKWebView,
            response: &WKNavigationResponse,
            decision_handler: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            self.forward_navigation_response(web_view, response, decision_handler);
        }

        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn navigation_action_became_download(
            &self,
            web_view: &WKWebView,
            action: &WKNavigationAction,
            download: &WKDownload,
        ) {
            self.forward_action_download(web_view, action, download);
        }

        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn navigation_response_became_download(
            &self,
            web_view: &WKWebView,
            response: &WKNavigationResponse,
            download: &WKDownload,
        ) {
            self.forward_response_download(web_view, response, download);
        }
    }
);

impl NavigationDelegate {
    fn new(
        sink: Arc<Sink>,
        identity: Identity,
        forward: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let delegate = mtm.alloc::<Self>().set_ivars(NavigationDelegateIvars {
            sink,
            identity,
            forward,
        });
        // SAFETY: `NSObject`'s initializer, called on an allocated instance
        // whose ivars are set.
        unsafe { msg_send![super(delegate), init] }
    }

    /// Whether wry's delegate implements `selector` (its methods are optional).
    fn forwards(&self, selector: Sel) -> bool {
        let protocol: &ProtocolObject<dyn WKNavigationDelegate> = &self.ivars().forward;
        let forward: &AnyObject = protocol.as_ref();
        // SAFETY: wry's retained delegate is an NSObject implementing this
        // selector query; the argument is a valid Objective-C selector.
        unsafe { msg_send![forward, respondsToSelector: selector] }
    }

    /// The failure of the navigation this callback names, if it is the one in
    /// flight: managed as a stop when the engine means cancellation, and failed
    /// with the engine's own message otherwise.
    fn failed(&self, navigation: Option<&WKNavigation>, error: &NSError) {
        if !end(&self.ivars().identity, navigation) {
            return;
        }
        if is_cancellation(error) {
            self.ivars().sink.cancel();
        } else {
            let sink = self.ivars().sink.clone();
            sink.failed(|| describe(error));
        }
    }

    fn forward_commit(&self, web_view: &WKWebView, navigation: Option<&WKNavigation>) {
        if !self.forwards(objc2::sel!(webView:didCommitNavigation:)) {
            return;
        }
        // SAFETY: the target implements the selector (checked), and the
        // arguments are the ones WebKit passed to this method.
        unsafe {
            self.ivars()
                .forward
                .webView_didCommitNavigation(web_view, navigation);
        }
    }

    fn forward_finish(&self, web_view: &WKWebView, navigation: Option<&WKNavigation>) {
        if !self.forwards(objc2::sel!(webView:didFinishNavigation:)) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_didFinishNavigation(web_view, navigation);
        }
    }

    fn forward_provisional_failure(
        &self,
        web_view: &WKWebView,
        navigation: Option<&WKNavigation>,
        error: &NSError,
    ) {
        if !self.forwards(objc2::sel!(webView:didFailProvisionalNavigation:withError:)) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_didFailProvisionalNavigation_withError(web_view, navigation, error);
        }
    }

    fn forward_failure(
        &self,
        web_view: &WKWebView,
        navigation: Option<&WKNavigation>,
        error: &NSError,
    ) {
        if !self.forwards(objc2::sel!(webView:didFailNavigation:withError:)) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_didFailNavigation_withError(web_view, navigation, error);
        }
    }

    fn forward_navigation_action(
        &self,
        web_view: &WKWebView,
        action: &WKNavigationAction,
        decision_handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
    ) {
        if !self.forwards(objc2::sel!(
            webView:decidePolicyForNavigationAction:decisionHandler:
        )) {
            // No delegate to decide: allow the navigation, as WebKit does
            // without a delegate. The handler must still be called once.
            decision_handler.call((WKNavigationActionPolicy::Allow,));
            return;
        }
        // SAFETY: the target implements the selector (checked), and the
        // arguments are the ones WebKit passed to this method.
        unsafe {
            self.ivars()
                .forward
                .webView_decidePolicyForNavigationAction_decisionHandler(
                    web_view,
                    action,
                    decision_handler,
                );
        }
    }

    fn forward_navigation_response(
        &self,
        web_view: &WKWebView,
        response: &WKNavigationResponse,
        decision_handler: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
    ) {
        if !self.forwards(objc2::sel!(
            webView:decidePolicyForNavigationResponse:decisionHandler:
        )) {
            decision_handler.call((WKNavigationResponsePolicy::Allow,));
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_decidePolicyForNavigationResponse_decisionHandler(
                    web_view,
                    response,
                    decision_handler,
                );
        }
    }

    fn forward_action_download(
        &self,
        web_view: &WKWebView,
        action: &WKNavigationAction,
        download: &WKDownload,
    ) {
        if !self.forwards(objc2::sel!(webView:navigationAction:didBecomeDownload:)) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_navigationAction_didBecomeDownload(web_view, action, download);
        }
    }

    fn forward_response_download(
        &self,
        web_view: &WKWebView,
        response: &WKNavigationResponse,
        download: &WKDownload,
    ) {
        if !self.forwards(objc2::sel!(webView:navigationResponse:didBecomeDownload:)) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            self.ivars()
                .forward
                .webView_navigationResponse_didBecomeDownload(web_view, response, download);
        }
    }
}

/// Whether WebKit ended the navigation without the page failing: the host or
/// the page cancelled it, or a policy turned the response into a download.
fn is_cancellation(error: &NSError) -> bool {
    let domain = error.domain().to_string();
    if domain == "NSURLErrorDomain" {
        return error.code() == NSURLErrorCancelled;
    }
    // The policy-interruption code is deprecated as a name in WebKit, but the
    // value is the one WebKit still reports for this navigation outcome.
    #[allow(deprecated)]
    let policy_change = WebKitErrorFrameLoadInterruptedByPolicyChange as isize;
    domain == "WebKitErrorDomain" && error.code() == policy_change
}

/// WebKit's failure, as a diagnostic: its own description plus the stable
/// domain and code.
fn describe(error: &NSError) -> String {
    let description = error.localizedDescription().to_string();
    let domain = error.domain().to_string();
    format!("{description} ({domain} {})", error.code())
}

/// The web view's owned delegate and its `estimatedProgress` observation.
struct Progress {
    view: Retained<WryWebView>,
    observer: Retained<ProgressObserver>,
    /// The extension's delegate; the web view holds it weakly, so this is what
    /// keeps it alive for as long as the view uses it.
    _delegate: Retained<NavigationDelegate>,
    /// The delegate wry installed (kept alive by wry's web view as well).
    forward: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
}

impl Progress {
    fn attach(webview: &wry::WebView, sink: Arc<Sink>) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            "WKWebView delegation must be installed on the main thread".to_owned()
        })?;
        let view = webview.webview();
        // SAFETY: `navigationDelegate` is a WKWebView property, read on the
        // main thread.
        let forward = unsafe { view.navigationDelegate() }.ok_or_else(|| {
            "the web view has no WKWebView navigation delegate to wrap".to_owned()
        })?;

        let identity: Identity = Rc::new(RefCell::new(Native::Idle));
        let delegate =
            NavigationDelegate::new(sink.clone(), identity.clone(), forward.clone(), mtm);
        // SAFETY: the property is set on the main thread and the delegate is
        // retained by `Self` for as long as the web view uses it.
        unsafe {
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        }

        let observer = ProgressObserver::new(sink, identity, mtm);
        let observed: &NSObject = &view;
        let registering: &NSObject = &observer;
        let estimated = NSString::from_str("estimatedProgress");
        // SAFETY: `observed` is the WKWebView whose `estimatedProgress` is
        // KVO-compliant; `registering` is retained by `Self` and removed in
        // `Drop`, before it goes. The context is unused, so null is valid.
        unsafe {
            observed.addObserver_forKeyPath_options_context(
                registering,
                &estimated,
                NSKeyValueObservingOptions::New,
                std::ptr::null_mut(),
            );
        }

        Ok(Self {
            view,
            observer,
            _delegate: delegate,
            forward,
        })
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let observed: &NSObject = &self.view;
        let registering: &NSObject = &self.observer;
        let estimated = NSString::from_str("estimatedProgress");
        // SAFETY: this is the matching removal of the registration made in
        // `attach`, with the same observer and context.
        unsafe {
            observed.removeObserver_forKeyPath_context(
                registering,
                &estimated,
                std::ptr::null_mut(),
            );
            // Hand the web view back with the delegate wry installed, so wry's
            // own behaviour is what remains if the view outlives this wrapper.
            self.view.setNavigationDelegate(Some(&*self.forward));
        }
    }
}

/// What the estimate observation reports to.
struct ProgressObserverIvars {
    sink: Arc<Sink>,
    identity: Identity,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NshExtGpuiProgressObserver"]
    #[thread_kind = MainThreadOnly]
    #[ivars = ProgressObserverIvars]
    struct ProgressObserver;

    unsafe impl NSObjectProtocol for ProgressObserver {}

    impl ProgressObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(
            &self,
            key_path: Option<&NSString>,
            object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut c_void,
        ) {
            let Some(key_path) = key_path else {
                return;
            };
            if key_path.to_string() != "estimatedProgress" {
                return;
            }
            let Some(object) = object else {
                return;
            };
            // An estimate belongs to a native navigation in flight; one that
            // arrives after it ended has no navigation to be reported against.
            let in_flight = {
                let identity = self.ivars().identity.borrow();
                identity.in_flight()
            };
            if !in_flight {
                return;
            }
            // SAFETY: the observed object is the WKWebView this observer was
            // registered on, read on the main thread.
            let estimated: f64 = unsafe { msg_send![object, estimatedProgress] };
            self.ivars().sink.estimate(estimated);
        }
    }
);

impl ProgressObserver {
    fn new(sink: Arc<Sink>, identity: Identity, mtm: MainThreadMarker) -> Retained<Self> {
        let observer = mtm
            .alloc::<Self>()
            .set_ivars(ProgressObserverIvars { sink, identity });
        // SAFETY: `NSObject`'s initializer, called on an allocated instance
        // whose ivars are set.
        unsafe { msg_send![super(observer), init] }
    }
}
