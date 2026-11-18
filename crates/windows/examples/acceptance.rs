//! Acceptance for the real NSH native-component path.
//!
//! It registers the adapter on `AppOptions::components`, runs an NSH
//! `run_app` with a Rust `Root`, and builds the page with
//! `nsh_ext::web_view().on_load_progress(...)`. A local server provides
//! successful pages, a 404 document, a slow navigation, a marker page and a
//! closed local port. The app then walks:
//!
//! 1. a successful page, which reports its viewport and
//!    `devicePixelRatio`; the initial source is empty first, so a clear
//!    before anything loaded must emit nothing and make no page,
//! 2. a marker page whose timer, worker and open request report to the
//!    server, then an empty source: all three must stop when the browser
//!    that owns them is retired,
//! 3. an HTTP 404 that must still finish as a completed document,
//! 4. clearing the source and immediately loading another page,
//! 5. the same source again after an empty one: setting an unchanged
//!    source does not reload, but a revisit after a clear does,
//! 6. replacing a loading page (the old navigation must be cancelled),
//! 7. a refused connection (the new navigation must fail honestly),
//! 8. closing the window early while a load is in flight: disposal emits
//!    nothing (no extra cancellation or failure).
//!
//! The successful page's `innerWidth`/`innerHeight` and `devicePixelRatio`
//! are compared with the WebView2 child window's client area and DPI scale:
//! the controller fills the window's pixels and rasterizes at the window's
//! scale, so the CSS viewport is the client area divided by that scale.
//! On a 2x display this is the exact 2x proof; set `NSH_EXT_ACCEPTANCE_DPI=2`
//! to require that the run is on a 2x display.
//!
//! `cargo run --example acceptance`
//!
//! Exit code 0 passes, 1 fails a contract assertion, 2 times out.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(windows)]
#[path = "support/mod.rs"]
mod support;

static DONE: AtomicBool = AtomicBool::new(false);
static REPORT: Mutex<Report> = Mutex::new(Report {
    source_set: false,
    initial_finished: false,
    marker_finished: false,
    clear_requested: None,
    missing_finished: false,
    rapid_clear_finished: false,
    revisit_armed: false,
    revisit_finished: false,
    replacement_cancelled: false,
    replacement_finished: false,
    refused_failed: false,
    early_teardown: false,
    failure_message: None,
    child: None,
    last_navigation: 0,
    started: 0,
    finished: 0,
    cancelled: 0,
    failed: 0,
    failures: Vec::new(),
});

/// The page's own report of its viewport and rasterization scale.
#[derive(Clone, Debug)]
struct Viewport {
    /// `innerWidth` in CSS pixels.
    width: u32,
    /// `innerHeight` in CSS pixels.
    height: u32,
    /// `devicePixelRatio` as the page sees it.
    dpr: f64,
}

/// The WebView2 child window while the app runs.
#[derive(Clone, Copy, Debug)]
struct Child {
    /// The client area in pixels.
    width: i32,
    height: i32,
    /// The window's DPI scale in pixels per point.
    scale: f64,
}

#[derive(Clone, Debug)]
struct Report {
    /// The first non-empty source was requested; nothing may be emitted
    /// before that.
    source_set: bool,
    initial_finished: bool,
    marker_finished: bool,
    clear_requested: Option<Instant>,
    missing_finished: bool,
    rapid_clear_finished: bool,
    /// The source for the same-URL revisit was set (after a clear).
    revisit_armed: bool,
    revisit_finished: bool,
    replacement_cancelled: bool,
    replacement_finished: bool,
    refused_failed: bool,
    /// The window was closed while a load was in flight.
    early_teardown: bool,
    failure_message: Option<String>,
    child: Option<Child>,
    last_navigation: u64,
    started: u32,
    finished: u32,
    cancelled: u32,
    failed: u32,
    failures: Vec<String>,
}

fn update(change: impl FnOnce(&mut Report)) {
    let mut report = REPORT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    change(&mut report);
}

fn fail(message: impl Into<String>) {
    update(|report| report.failures.push(message.into()));
}

#[cfg(windows)]
mod app {
    use std::{
        sync::atomic::{AtomicIsize, Ordering},
        time::{Duration, Instant},
    };

    use nsh_core::{element::desktop as window, prelude::*};
    use nsh_ext::{LoadProgress, LoadState};
    use nsh_platform_windows::windows::{
        Win32::{
            Foundation::{HWND, LPARAM, RECT},
            System::Threading::GetCurrentProcessId,
            UI::{
                HiDpi::GetDpiForWindow,
                WindowsAndMessaging::{
                    EnumChildWindows, EnumWindows, GetClassNameW, GetClientRect,
                    GetWindowThreadProcessId,
                },
            },
        },
        core::BOOL,
    };

    use super::{Child, fail, update};

    /// The adapter's host class, as Windows reports it.
    const HOST_CLASS: &str = "nsh_ext_webview2_host";

    /// The handle the window enumeration found.
    static FOUND: AtomicIsize = AtomicIsize::new(0);

    #[component]
    pub fn Acceptance() -> impl IntoElement {
        let urls = crate::support::urls();
        // An empty source first: creating the component and clearing it
        // before anything loaded must make no browser and emit no event.
        let src = signal(String::new());
        let phase = signal(0u8);
        let window = window::current_window();
        let start_src = src;
        let start = urls.ok.clone();
        set_timeout(Duration::from_millis(250), move || {
            update(|report| report.source_set = true);
            start_src.set(start);
        });
        let listener_src = src;
        let listener_phase = phase;
        let close_window = window.clone();
        let page = nsh_ext::web_view()
            .src(src)
            .on_load_progress(move |progress| {
                record_event(progress);
                let current = listener_phase.get();
                match progress.state {
                    LoadState::Started => match current {
                        4 => {
                            // Nothing but the revisit may start here: setting
                            // an unchanged source must not reload it.
                            update(|report| {
                                if !report.revisit_armed {
                                    report.failures.push(
                                        "setting the unchanged source reloaded it".to_owned(),
                                    );
                                }
                            });
                        }
                        5 => {
                            // The slow page is loading: replace it.
                            listener_phase.set(6);
                            listener_src.set(urls.next.clone());
                        }
                        _ => {}
                    },
                    LoadState::Finished => match current {
                        0 => {
                            update(|report| report.initial_finished = true);
                            listener_phase.set(1);
                            // Let the page report its viewport and
                            // devicePixelRatio before it is replaced.
                            let next_src = listener_src;
                            let next = urls.marker.clone();
                            set_timeout(Duration::from_millis(300), move || {
                                next_src.set(next);
                            });
                        }
                        1 => {
                            update(|report| report.marker_finished = true);
                            listener_phase.set(2);
                            // Let the marker page's timer, worker and open
                            // request report before the browser that owns
                            // them is retired.
                            let clear_src = listener_src;
                            let next_src = listener_src;
                            let next = urls.missing.clone();
                            set_timeout(Duration::from_millis(700), move || {
                                update(|report| report.clear_requested = Some(Instant::now()));
                                clear_src.set(String::new());
                                // The next source waits for the retired
                                // browser to be replaced.
                                set_timeout(Duration::from_millis(1200), move || {
                                    next_src.set(next);
                                });
                            });
                        }
                        2 => {
                            update(|report| report.missing_finished = true);
                            listener_phase.set(3);
                            let clear_src = listener_src;
                            let next_src = listener_src;
                            let next = urls.ok2.clone();
                            clear_src.set(String::new());
                            // Empty source clears; a new source follows
                            // almost immediately and must not be lost.
                            set_timeout(Duration::from_millis(120), move || {
                                next_src.set(next);
                            });
                        }
                        3 => {
                            update(|report| report.rapid_clear_finished = true);
                            listener_phase.set(4);
                            // The same source again is not a change: this
                            // must not reload. Then clear and revisit the
                            // same URL, which must load again.
                            listener_src.set(urls.ok2.clone());
                            let clear_src = listener_src;
                            let revisit_src = listener_src;
                            let revisit = urls.ok2.clone();
                            set_timeout(Duration::from_millis(250), move || {
                                clear_src.set(String::new());
                                set_timeout(Duration::from_millis(150), move || {
                                    update(|report| report.revisit_armed = true);
                                    revisit_src.set(revisit);
                                });
                            });
                        }
                        4 => {
                            update(|report| report.revisit_finished = true);
                            listener_phase.set(5);
                            listener_src.set(urls.slow.clone());
                        }
                        6 => {
                            update(|report| report.replacement_finished = true);
                            listener_phase.set(7);
                            listener_src.set(urls.refused.clone());
                        }
                        other => {
                            fail(format!("unexpected finished in phase {other}"));
                            window.close();
                        }
                    },
                    LoadState::Cancelled => {
                        if current == 6 {
                            // The replacement of the slow page cancelled it.
                            update(|report| report.replacement_cancelled = true);
                        } else {
                            fail(format!("unexpected cancellation in phase {current}"));
                            window.close();
                        }
                    }
                    LoadState::Failed => {
                        if current == 7 {
                            update(|report| {
                                report.refused_failed = true;
                                report.failure_message = progress.message.clone();
                            });
                            listener_phase.set(8);
                            // Close while this last load is still in
                            // flight: disposal must emit nothing (the
                            // counts would change).
                            listener_src.set(urls.slow.clone());
                            let close = close_window.clone();
                            set_timeout(Duration::from_millis(250), move || {
                                update(|report| report.early_teardown = true);
                                close.close();
                            });
                        } else {
                            fail(format!(
                                "unexpected failure in phase {current}: {:?}",
                                progress.message
                            ));
                            window.close();
                        }
                    }
                    _ => {}
                }
            })
            .flex_1();
        view().flex_1().child(page)
    }

    fn record_event(progress: &LoadProgress) {
        // While the app runs its window exists: record what the WebView2
        // host window is, for the sizing and DPI checks. A window that has
        // not been placed yet reports no client area; keep the last real
        // size instead of overwriting it with zero.
        let child = child_metrics().filter(|child| child.width > 0 && child.height > 0);
        update(|report| {
            if !report.source_set {
                report
                    .failures
                    .push("a load event arrived before the first source was set".to_owned());
            }
            if report.last_navigation == 0
                && (progress.state != LoadState::Started || progress.navigation != 1)
            {
                // Clearing before anything loaded must not consume an
                // identity or emit a clear navigation.
                report.failures.push(format!(
                    "the first event was {:?} for navigation {}",
                    progress.state, progress.navigation
                ));
            }
            if progress.navigation == 0 || progress.navigation < report.last_navigation {
                report.failures.push(format!(
                    "navigation identity went backwards: {}",
                    progress.navigation
                ));
            } else {
                report.last_navigation = progress.navigation;
            }
            if let Some(child) = child {
                report.child = Some(child);
            }
            match progress.state {
                LoadState::Started => report.started += 1,
                LoadState::Finished => report.finished += 1,
                LoadState::Cancelled => report.cancelled += 1,
                LoadState::Failed => report.failed += 1,
                _ => {}
            }
            if progress.state == LoadState::Loading {
                report
                    .failures
                    .push("WebView2 reported a synthetic progress estimate".to_owned());
            }
            if progress.state == LoadState::Started && progress.progress.is_some() {
                report
                    .failures
                    .push("started carried a numeric estimate".to_owned());
            }
            if progress.state == LoadState::Failed && progress.progress.is_some() {
                report
                    .failures
                    .push("failed carried a progress value".to_owned());
            }
            if progress.state == LoadState::Cancelled && progress.progress.is_some() {
                report
                    .failures
                    .push("cancelled carried a progress value".to_owned());
            }
            if progress.state == LoadState::Finished
                && progress.progress.map(|value| value.0) != Some(1.0)
            {
                report
                    .failures
                    .push("finished did not report confirmed completion".to_owned());
            }
            if progress.progress.is_some_and(|progress| progress.0 == 0.0) {
                // Windows has no estimate: zero is not "unknown".
                report
                    .failures
                    .push("zero was used for unknown progress".to_owned());
            }
        });
    }

    /// The extension's WebView2 child window: its client area and DPI.
    fn child_metrics() -> Option<Child> {
        unsafe extern "system" fn on_child(hwnd: HWND, _: LPARAM) -> BOOL {
            let mut name = [0u16; 64];
            // SAFETY: a class-name read on the window being enumerated.
            let length = unsafe { GetClassNameW(hwnd, &mut name) };
            let length = length.max(0) as usize;
            let class: Vec<u16> = HOST_CLASS.encode_utf16().collect();
            if length == class.len() && name[..length] == class[..] {
                FOUND.store(hwnd.0 as isize, Ordering::SeqCst);
                return BOOL(0);
            }
            BOOL(1)
        }

        unsafe extern "system" fn on_top(hwnd: HWND, _: LPARAM) -> BOOL {
            let mut process = 0u32;
            // SAFETY: plain reads of the window's process; the thread id
            // the call also returns is not needed.
            let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
            // SAFETY: the process id of this executable.
            if process != unsafe { GetCurrentProcessId() } {
                return BOOL(1);
            }
            // SAFETY: the same class-name read for the top-level window
            // itself (the host is a pop-up until NSH adopts it).
            let _ = unsafe { on_child(hwnd, LPARAM(0)) };
            if FOUND.load(Ordering::SeqCst) != 0 {
                return BOOL(0);
            }
            // SAFETY: enumerating this window's children.
            unsafe {
                let _ = EnumChildWindows(Some(hwnd), Some(on_child), LPARAM(0));
            }
            if FOUND.load(Ordering::SeqCst) != 0 {
                return BOOL(0);
            }
            BOOL(1)
        }

        FOUND.store(0, Ordering::SeqCst);
        // SAFETY: plain Win32 enumeration of this process' windows; the
        // callbacks only read window properties.
        unsafe {
            let _ = EnumWindows(Some(on_top), LPARAM(0));
        }
        let raw = FOUND.load(Ordering::SeqCst);
        if raw == 0 {
            return None;
        }
        let hwnd = HWND(raw as *mut core::ffi::c_void);
        let mut rect = RECT::default();
        // SAFETY: plain reads on the window the enumeration found.
        unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
        // SAFETY: a DPI getter on that window.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 {
            return None;
        }
        Some(Child {
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            scale: f64::from(dpi) / 96.0,
        })
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use nsh_platform_windows::{AppOptions, AppSource, Root, run_app};

    let server = support::Server::start();
    support::set_urls(server.urls.clone());

    std::thread::spawn(|| {
        // Every explicit source change makes a new browser; allow for the
        // cold starts of several of them plus the deliberate waits.
        for _ in 0..600 {
            if DONE.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("FAIL: acceptance timed out waiting for NSH/WebView2");
        std::process::exit(2);
    });

    let options = AppOptions {
        title: "nsh-ext Windows acceptance".to_owned(),
        app_id: "dev.nsh.ext.windows.acceptance".to_owned(),
        size: (640, 520),
        ..Default::default()
    };
    nsh_ext_windows::register(&options.components);
    let _ = run_app(AppSource::Rust(Root::of::<app::Acceptance>()), options);
    DONE.store(true, Ordering::SeqCst);
    check()
}

/// The `/ok` page's last viewport report, once the server has it.
#[cfg(windows)]
fn ok_viewport() -> Option<Viewport> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let found = support::viewports()
            .into_iter()
            .rfind(|viewport| viewport.page == "/ok")
            .map(|viewport| Viewport {
                width: viewport.width,
                height: viewport.height,
                dpr: viewport.dpr,
            });
        if found.is_some() || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The size the acceptance window and its page are laid out at.
#[cfg(windows)]
fn has_expected_viewport(viewport: &Viewport) -> bool {
    (400..=700).contains(&viewport.width) && (300..=550).contains(&viewport.height)
}

/// The page's CSS viewport must be the window's pixels divided by its DPI
/// scale, and its `devicePixelRatio` must be that scale. This is a units
/// check: the controller's bounds are the child window's pixels (raw-pixel
/// bounds mode) and its rasterization scale is the window's DPI, so a 2x
/// window has `devicePixelRatio` 2 and half the CSS width of its pixels.
#[cfg(windows)]
fn check_rasterization(viewport: &Viewport, child: Option<Child>, failures: &mut Vec<String>) {
    let Some(child) = child else {
        failures.push("the WebView2 child window was not found".to_owned());
        return;
    };
    if child.width <= 0 || child.height <= 0 {
        failures.push(format!(
            "the WebView2 child window had no client area: {}x{}",
            child.width, child.height
        ));
        return;
    }
    if child.scale <= 0.0 {
        failures.push(format!(
            "the WebView2 child window reported DPI scale {}",
            child.scale
        ));
        return;
    }
    if (viewport.dpr - child.scale).abs() > 1e-6 {
        failures.push(format!(
            "the page's devicePixelRatio is {} but the window's DPI scale is {}: \
             the controller is not rasterized at the window's scale",
            viewport.dpr, child.scale
        ));
    }
    // `innerWidth` is whole CSS pixels; one pixel of rounding is allowed.
    let css_width = f64::from(child.width) / child.scale;
    let css_height = f64::from(child.height) / child.scale;
    if (f64::from(viewport.width) - css_width).abs() > 1.0 {
        failures.push(format!(
            "the page's CSS width is {} but the window's {} pixels at scale {} are {} CSS pixels",
            viewport.width, child.width, child.scale, css_width
        ));
    }
    if (f64::from(viewport.height) - css_height).abs() > 1.0 {
        failures.push(format!(
            "the page's CSS height is {} but the window's {} pixels at scale {} are {} CSS pixels",
            viewport.height, child.height, child.scale, css_height
        ));
    }
    if let Ok(required) = std::env::var("NSH_EXT_ACCEPTANCE_DPI")
        && let Ok(required) = required.parse::<f64>()
        && (child.scale - required).abs() > 1e-6
    {
        failures.push(format!(
            "NSH_EXT_ACCEPTANCE_DPI requires a {required}x display, \
             but the window's DPI scale is {}",
            child.scale
        ));
    }
}

/// The marker page's timer, worker and request must stop when the source is
/// cleared. A document rewritten in place keeps all three alive; retiring
/// the browser that owns the page destroys them, so their silence after the
/// clear is the proof that the old page's execution ended.
#[cfg(windows)]
fn check_clear(report: &Report, failures: &mut Vec<String>) {
    const GRACE: Duration = Duration::from_millis(500);
    let Some(clear_at) = report.clear_requested else {
        failures.push("the acceptance never cleared the source".to_owned());
        return;
    };
    let beats = support::beats();
    let before = |beat: support::Beat| {
        beats
            .iter()
            .filter(|(kind, at)| *kind == beat && *at < clear_at)
            .count()
    };
    let timers_before = before(support::Beat::Timer);
    let workers_before = before(support::Beat::Worker);
    if timers_before < 2 {
        failures.push(format!(
            "the marker page's timer sent only {timers_before} beats before the clear; \
             the clear test would be vacuous"
        ));
    }
    if workers_before < 2 {
        failures.push(format!(
            "the marker page's worker sent only {workers_before} beats before the clear; \
             the clear test would be vacuous"
        ));
    }
    let stream = support::stream();
    if stream.opened == 0 {
        failures.push("the marker page never opened its long-lived request".to_owned());
    }
    let alive_after = beats
        .iter()
        .filter(|(_, at)| *at >= clear_at + GRACE)
        .count();
    if alive_after > 0 {
        failures.push(format!(
            "the old page kept running after the source was cleared: \
             {alive_after} timer/worker beats arrived more than {GRACE:?} after the clear"
        ));
    }
    if stream.closed.is_none() {
        failures.push(
            "the old page's long-lived request stayed open after the source was cleared".to_owned(),
        );
    }
}

#[cfg(windows)]
fn check() -> std::process::ExitCode {
    let report = REPORT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut failures = report.failures.clone();
    let viewport = ok_viewport();
    if !report.initial_finished {
        failures.push("the initial page did not finish".to_owned());
    }
    if !report.marker_finished {
        failures.push("the marker page did not finish".to_owned());
    }
    check_clear(&report, &mut failures);
    if !report.missing_finished {
        failures
            .push("the 404 page after the clear did not finish as a completed document".to_owned());
    }
    if !report.rapid_clear_finished {
        failures.push("rapid clear then load did not finish".to_owned());
    }
    if !report.revisit_finished {
        failures.push("revisiting the same URL after a clear did not finish".to_owned());
    }
    if !report.replacement_cancelled {
        failures.push("the replaced navigation was not cancelled".to_owned());
    }
    if !report.replacement_finished {
        failures.push("the replacement page did not finish".to_owned());
    }
    if !report.refused_failed {
        failures.push("the refused connection did not fail".to_owned());
    }
    if !report.early_teardown {
        failures.push("the window was not closed during a load".to_owned());
    }
    // The final load's Started races the timer that closes the window: it
    // may or may not have been delivered. Everything else is exact, and
    // disposal must not add a cancellation or failure of its own.
    if !matches!(report.started, 8 | 9)
        || report.finished != 6
        || report.cancelled != 1
        || report.failed != 1
    {
        failures.push(format!(
            "unexpected native callback counts: started={}, finished={}, cancelled={}, failed={}",
            report.started, report.finished, report.cancelled, report.failed
        ));
    }
    if report.failure_message.as_deref().is_none_or(str::is_empty) {
        failures.push("the refused connection had no diagnostic".to_owned());
    }
    match &viewport {
        Some(viewport) => {
            if !has_expected_viewport(viewport) {
                failures.push(format!(
                    "NSH did not size the WebView2 viewport to the 640x520 window: {}x{} CSS px",
                    viewport.width, viewport.height
                ));
            }
            check_rasterization(viewport, report.child, &mut failures);
        }
        None => {
            failures.push("the page did not report its viewport and devicePixelRatio".to_owned())
        }
    }
    if failures.is_empty() {
        let child = report.child.unwrap_or(Child {
            width: 0,
            height: 0,
            scale: 0.0,
        });
        let viewport = viewport.expect("checked above");
        println!(
            "PASS: NSH native component started from an empty source without events, loaded pages, \
             retired the browser that owned the marker page (its timer, worker and request \
             stopped), finished a 404, survived rapid clear->src, kept an unchanged source from \
             reloading while a cleared same-URL revisit loaded, cancelled a replaced load, failed \
             a refused connection honestly, tore down during a load without extra events, and \
             filled the window at scale {} (page {}x{} CSS px, window {}x{} px, \
             devicePixelRatio {})",
            child.scale, viewport.width, viewport.height, child.width, child.height, viewport.dpr
        );
        std::process::ExitCode::SUCCESS
    } else {
        for failure in failures {
            eprintln!("FAIL: {failure}");
        }
        std::process::ExitCode::FAILURE
    }
}

#[cfg(not(windows))]
fn main() {}
