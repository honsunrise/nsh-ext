//! Native acceptance harness for the extension WebView.
//!
//! Six real components exercise the contract through Native Shell:
//!
//! - a self-contained page that must reach `Finished` after a real native
//!   intermediate estimate (`success`);
//! - a connection to a freshly freed loopback port that WebKit must refuse,
//!   reported as `Failed` (never as success because of an HTTP status)
//!   (`failure`);
//! - a navigation replaced while its TLS connection hangs, which must report
//!   `Cancelled` for the old identity and then finish the new one
//!   (`replacement`);
//! - a source revisit: a page, an HTTP 500 error *document* that must still
//!   finish, the page again, then a refused connection that must fail, with
//!   identities continuing across the browser replacements (`revisit`);
//! - a page whose timer and worker request the fixture server, then an empty
//!   `src`: the active load is cancelled, the page must be destroyed (both
//!   counters stop changing), and no blank navigation may be reported
//!   (`clear`);
//! - a component removed mid-load (silent teardown), then added again, whose
//!   fresh browser must start again at navigation identity one (`teardown`).
//!
//! It exits nonzero on any contract violation, timeout, early close, or a
//! missing phase. The same assertions run on macOS, iOS Simulator and Mac
//! Catalyst. The HTTP fixtures exist only for this harness and are served from
//! loopback; the app bundles allow arbitrary loads so the simulator can reach
//! them.
//!
//! ```sh
//! cargo run --manifest-path crates/apple/Cargo.toml --example acceptance
//! crates/apple/examples/run-ios-simulator.sh acceptance
//! crates/apple/examples/run-catalyst.sh acceptance
//! ```

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod apple {
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::{
            OnceLock,
            atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use nsh_core::prelude::*;
    use nsh_ext::{LoadProgress, LoadState};

    #[cfg(target_os = "ios")]
    use nsh_platform_ios as platform;
    #[cfg(target_os = "macos")]
    use nsh_platform_macos as platform;

    use platform::{AppOptions, AppSource, Root, run_app, set_components};

    const SUCCESS_PAGE: &str = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Cmeta%20name=viewport%20content=%22width=device-width,%20initial-scale=1%22%3E%3Cstyle%3Ebody%7Bfont:16px%20-apple-system,sans-serif;margin:2rem;%7D%3C/style%3E%3Ch1%3Ensh-ext%20acceptance%3C/h1%3E%3Cp%3EThis%20page%20must%20reach%20Finished.%3C/p%3E";
    const SUCCESS: &str = "success";
    const FAILURE: &str = "failure";
    const REPLACEMENT: &str = "replacement";
    const REVISIT: &str = "revisit";
    const CLEAR: &str = "clear";
    const TEARDOWN: &str = "teardown";
    const TIMEOUT: Duration = Duration::from_secs(60);

    /// The fixture page whose timer and worker request `/ping` until the page
    /// is destroyed. `/slow` keeps the document loading meanwhile.
    const CLEAR_PAGE: &str = "<!doctype html><html><head><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head><body><h1>clear fixture</h1><img src=\"/slow\" alt=\"\"><script>setInterval(function () { fetch('/ping?kind=timer').catch(function () {}); }, 50); try { new Worker('/worker.js'); } catch (error) {} </script></body></html>";
    const WORKER_SCRIPT: &str =
        "setInterval(function () { fetch('/ping?kind=worker').catch(function () {}); }, 50);";
    const OK_PAGE: &str =
        "<!doctype html><html><body><h1>ok</h1><p>The page loaded.</p></body></html>";
    /// An HTTP error *document*: WebKit displays it and reports didFinish, so
    /// the adapter must report `Finished`, not `Failed`.
    const ERROR_PAGE: &str = "<!doctype html><html><body><h1>500</h1><p>The server returned an error document.</p></body></html>";

    static FAILURE_PORT: AtomicU16 = AtomicU16::new(0);
    static HANG_PORT: AtomicU16 = AtomicU16::new(0);
    static ASSET_PORT: AtomicU16 = AtomicU16::new(0);
    static HTTP_PORT: AtomicU16 = AtomicU16::new(0);
    static ASSET_RELEASE: OnceLock<mpsc::Sender<()>> = OnceLock::new();
    static ACTIONS: OnceLock<mpsc::Sender<Action>> = OnceLock::new();

    enum Action {
        Clear,
        Readd,
    }
    static SUCCESS_INTERMEDIATE: AtomicBool = AtomicBool::new(false);
    static SUCCESS_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static FAILURE_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static REPLACEMENT_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static REVISIT_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static CLEAR_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static TEARDOWN_NAVIGATION: AtomicU64 = AtomicU64::new(0);
    static REPLACEMENT_FIRST: AtomicU64 = AtomicU64::new(0);
    static REPLACEMENT_PHASE: AtomicU8 = AtomicU8::new(0);
    static REVISIT_COMPLETED: AtomicU64 = AtomicU64::new(0);
    static CLEAR_PHASE: AtomicU8 = AtomicU8::new(0);
    static TEARDOWN_PHASE: AtomicU8 = AtomicU8::new(0);
    static TIMER_PINGS: AtomicU64 = AtomicU64::new(0);
    static WORKER_PINGS: AtomicU64 = AtomicU64::new(0);
    static SUCCESS_FINISHED: AtomicBool = AtomicBool::new(false);
    static FAILURE_FAILED: AtomicBool = AtomicBool::new(false);
    static REPLACEMENT_DONE: AtomicBool = AtomicBool::new(false);
    static REVISIT_DONE: AtomicBool = AtomicBool::new(false);
    static CLEAR_DONE: AtomicBool = AtomicBool::new(false);
    static TEARDOWN_DONE: AtomicBool = AtomicBool::new(false);
    static BROKEN: AtomicBool = AtomicBool::new(false);
    static PASSED: AtomicBool = AtomicBool::new(false);

    fn failure_url() -> String {
        format!("https://127.0.0.1:{}/", FAILURE_PORT.load(Ordering::SeqCst))
    }

    fn hang_url() -> String {
        format!("https://127.0.0.1:{}/", HANG_PORT.load(Ordering::SeqCst))
    }

    fn ok_url() -> String {
        format!("http://127.0.0.1:{}/ok", HTTP_PORT.load(Ordering::SeqCst))
    }

    fn error_url() -> String {
        format!(
            "http://127.0.0.1:{}/error",
            HTTP_PORT.load(Ordering::SeqCst)
        )
    }

    fn clear_url() -> String {
        format!("http://127.0.0.1:{}/page", HTTP_PORT.load(Ordering::SeqCst))
    }

    fn measured_success_url() -> String {
        format!(
            "{SUCCESS_PAGE}%3Cimg%20src=%22https://127.0.0.1:{}/asset%22%3E",
            ASSET_PORT.load(Ordering::SeqCst)
        )
    }

    fn broken(message: impl std::fmt::Display) {
        if !BROKEN.swap(true, Ordering::SeqCst) {
            eprintln!("nsh-ext-apple acceptance: FAIL: {message}");
        }
        request_exit(1);
    }

    fn settle() {
        if SUCCESS_FINISHED.load(Ordering::SeqCst)
            && FAILURE_FAILED.load(Ordering::SeqCst)
            && REPLACEMENT_DONE.load(Ordering::SeqCst)
            && REVISIT_DONE.load(Ordering::SeqCst)
            && CLEAR_DONE.load(Ordering::SeqCst)
            && TEARDOWN_DONE.load(Ordering::SeqCst)
            && !BROKEN.load(Ordering::SeqCst)
            && !PASSED.swap(true, Ordering::SeqCst)
        {
            println!(
                "nsh-ext-apple acceptance: PASS (native estimate, finish, HTTP error document, \
                 native failure, replacement, revisit, clear, teardown)"
            );
            request_exit(0);
        }
    }

    /// macOS returns from `run_app` and the caller decides the status; iOS
    /// and Catalyst terminate the process here.
    #[cfg(target_os = "macos")]
    fn request_exit(_code: i32) {
        platform::quit_app();
    }

    #[cfg(target_os = "ios")]
    fn request_exit(code: i32) {
        // stdout is line buffered, but flush explicitly: the process ends
        // without running destructors here.
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::process::exit(code);
    }

    fn check_navigation(counter: &AtomicU64, label: &str, navigation: u64) -> bool {
        if navigation == 0 {
            broken(format!("{label}: navigation identity is zero"));
            return false;
        }
        let previous = counter.swap(navigation, Ordering::SeqCst);
        if navigation < previous {
            broken(format!(
                "{label}: navigation identity went backwards ({previous} then {navigation})"
            ));
            return false;
        }
        true
    }

    fn check_estimate(progress: &LoadProgress, label: &str) -> bool {
        let Some(value) = progress.progress else {
            broken(format!("{label}: Loading carried no native estimate"));
            return false;
        };
        let fraction = value.0;
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            broken(format!(
                "{label}: estimate {fraction} is outside zero to one"
            ));
            return false;
        }
        true
    }

    fn observe_success(progress: &LoadProgress) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&SUCCESS_NAVIGATION, SUCCESS, progress.navigation) {
            return;
        }
        match progress.state {
            LoadState::Started => {
                if progress.progress.is_some() || progress.message.is_some() {
                    broken("success: Started carried extra data");
                }
            }
            LoadState::Loading => {
                if check_estimate(progress, SUCCESS)
                    && progress
                        .progress
                        .is_some_and(|value| value.0 > 0.0 && value.0 < 1.0)
                    && !SUCCESS_INTERMEDIATE.swap(true, Ordering::SeqCst)
                {
                    let _ = ASSET_RELEASE.get().expect("asset fixture ready").send(());
                }
            }
            LoadState::Finished => {
                if !SUCCESS_INTERMEDIATE.load(Ordering::SeqCst) {
                    broken("success: no real intermediate estimate was delivered");
                    return;
                }
                if progress.progress.map(|value| value.0) != Some(1.0) || progress.message.is_some()
                {
                    broken("success: Finished carried the wrong final data");
                    return;
                }
                SUCCESS_FINISHED.store(true, Ordering::SeqCst);
                settle();
            }
            LoadState::Failed => broken(format!(
                "success: unexpected failure {:?}",
                progress.message
            )),
            LoadState::Cancelled => broken("success: unexpected cancellation"),
            _ => broken("success: unknown load state"),
        }
    }

    fn observe_failure(progress: &LoadProgress) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&FAILURE_NAVIGATION, FAILURE, progress.navigation) {
            return;
        }
        match progress.state {
            LoadState::Started => {}
            LoadState::Loading => {
                check_estimate(progress, FAILURE);
            }
            LoadState::Failed => {
                if progress.progress.is_some() {
                    broken("failure: Failed synthesized an estimate");
                    return;
                }
                if progress
                    .message
                    .as_deref()
                    .is_none_or(|message| message.is_empty())
                {
                    broken("failure: Failed carried no diagnostic");
                    return;
                }
                FAILURE_FAILED.store(true, Ordering::SeqCst);
                settle();
            }
            LoadState::Finished => {
                broken("failure: the refused connection unexpectedly finished");
            }
            LoadState::Cancelled => broken("failure: unexpected cancellation"),
            _ => broken("failure: unknown load state"),
        }
    }

    fn observe_replacement(progress: &LoadProgress, source: Signal<String>) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&REPLACEMENT_NAVIGATION, REPLACEMENT, progress.navigation) {
            return;
        }
        match progress.state {
            LoadState::Started => match REPLACEMENT_PHASE.load(Ordering::SeqCst) {
                0 => {
                    REPLACEMENT_FIRST.store(progress.navigation, Ordering::SeqCst);
                    REPLACEMENT_PHASE.store(1, Ordering::SeqCst);
                    // Replace while the hanging TLS connection is provisional.
                    source.set(SUCCESS_PAGE.to_owned());
                }
                1 => {
                    // A first estimate may have triggered the replacement;
                    // the same navigation's late start is not a new one.
                    if progress.navigation != REPLACEMENT_FIRST.load(Ordering::SeqCst) {
                        broken("replacement: a different navigation started");
                    }
                }
                2 => {
                    if progress.navigation <= REPLACEMENT_FIRST.load(Ordering::SeqCst) {
                        broken("replacement: the new navigation reused the old identity");
                    } else {
                        REPLACEMENT_PHASE.store(3, Ordering::SeqCst);
                    }
                }
                _ => broken("replacement: Started out of order"),
            },
            LoadState::Loading => {
                check_estimate(progress, REPLACEMENT);
                match REPLACEMENT_PHASE.load(Ordering::SeqCst) {
                    0 => {
                        // Tracking may have been enabled after didStart; the
                        // first native estimate is still the real identity.
                        REPLACEMENT_FIRST.store(progress.navigation, Ordering::SeqCst);
                        REPLACEMENT_PHASE.store(1, Ordering::SeqCst);
                        source.set(SUCCESS_PAGE.to_owned());
                    }
                    1 | 3 => {}
                    _ => broken("replacement: Loading out of order"),
                }
            }
            LoadState::Cancelled => {
                if REPLACEMENT_PHASE.load(Ordering::SeqCst) == 1
                    && progress.navigation == REPLACEMENT_FIRST.load(Ordering::SeqCst)
                {
                    REPLACEMENT_PHASE.store(2, Ordering::SeqCst);
                } else {
                    broken("replacement: cancelled the wrong navigation");
                }
            }
            LoadState::Finished => {
                if REPLACEMENT_PHASE.load(Ordering::SeqCst) == 3
                    && progress.progress.map(|value| value.0) == Some(1.0)
                {
                    REPLACEMENT_DONE.store(true, Ordering::SeqCst);
                    settle();
                } else {
                    broken("replacement: Finished out of order");
                }
            }
            LoadState::Failed => broken(format!(
                "replacement: unexpected failure {:?}",
                progress.message
            )),
            _ => broken("replacement: unknown load state"),
        }
    }

    /// A page, an HTTP 500 document, the page again, then a refused
    /// connection. Each `src` change destroys the browser and creates a new
    /// one, but the component's navigation identities continue: 1, 2, 3, 4.
    fn observe_revisit(progress: &LoadProgress, source: Signal<String>) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&REVISIT_NAVIGATION, REVISIT, progress.navigation) {
            return;
        }
        let completed = REVISIT_COMPLETED.load(Ordering::SeqCst);
        let expected = completed + 1;
        match progress.state {
            LoadState::Started => {
                if progress.navigation != expected {
                    broken(format!(
                        "revisit: expected navigation {expected} but got {}",
                        progress.navigation
                    ));
                }
            }
            LoadState::Loading => {
                check_estimate(progress, REVISIT);
            }
            LoadState::Finished => {
                if progress.navigation != expected {
                    broken(format!(
                        "revisit: expected navigation {expected} to finish but got {}",
                        progress.navigation
                    ));
                    return;
                }
                if progress.progress.map(|value| value.0) != Some(1.0) || progress.message.is_some()
                {
                    broken("revisit: Finished carried the wrong data");
                    return;
                }
                REVISIT_COMPLETED.store(progress.navigation, Ordering::SeqCst);
                match progress.navigation {
                    // An HTTP error document still finishes.
                    1 => source.set(error_url()),
                    2 => source.set(ok_url()),
                    3 => source.set(failure_url()),
                    _ => broken("revisit: too many successful navigations"),
                }
            }
            LoadState::Failed => {
                if completed != 3 || progress.navigation != 4 {
                    broken("revisit: failure out of order");
                    return;
                }
                if progress.progress.is_some() {
                    broken("revisit: Failed synthesized an estimate");
                    return;
                }
                if progress
                    .message
                    .as_deref()
                    .is_none_or(|message| message.is_empty())
                {
                    broken("revisit: Failed carried no diagnostic");
                    return;
                }
                REVISIT_DONE.store(true, Ordering::SeqCst);
                settle();
            }
            LoadState::Cancelled => broken("revisit: unexpected cancellation"),
            _ => broken("revisit: unknown load state"),
        }
    }

    /// The page's timer and worker must be running, the empty source must
    /// cancel the active load, and both counters must stop changing once the
    /// page is destroyed. No blank navigation may be reported.
    fn observe_clear(progress: &LoadProgress) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&CLEAR_NAVIGATION, CLEAR, progress.navigation) {
            return;
        }
        let phase = CLEAR_PHASE.load(Ordering::SeqCst);
        match progress.state {
            LoadState::Started => {
                if phase != 0 {
                    broken("clear: a navigation started after the page was loaded");
                    return;
                }
                CLEAR_PHASE.store(1, Ordering::SeqCst);
                let _ = std::thread::spawn(move || {
                    // Let the document's timer and worker run before the
                    // source is cleared. The generous wait also covers
                    // WebKit's timer throttling in a background window.
                    std::thread::sleep(Duration::from_millis(2500));
                    if BROKEN.load(Ordering::SeqCst) {
                        return;
                    }
                    let timer = TIMER_PINGS.load(Ordering::SeqCst);
                    let worker = WORKER_PINGS.load(Ordering::SeqCst);
                    if timer == 0 {
                        broken("clear: the page's timer did not run before clearing");
                        return;
                    }
                    if worker == 0 {
                        broken("clear: the page's worker did not run before clearing");
                        return;
                    }
                    // From here the only permitted event is the active
                    // navigation's cancellation.
                    CLEAR_PHASE.store(2, Ordering::SeqCst);
                    ACTIONS
                        .get()
                        .expect("the app is running")
                        .send(Action::Clear)
                        .expect("the engine owns the fixture commands");
                    // Give the destroyed page time to be torn down, then
                    // require that the counters no longer move.
                    std::thread::sleep(Duration::from_millis(900));
                    let first_timer = TIMER_PINGS.load(Ordering::SeqCst);
                    let first_worker = WORKER_PINGS.load(Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(900));
                    let second_timer = TIMER_PINGS.load(Ordering::SeqCst);
                    let second_worker = WORKER_PINGS.load(Ordering::SeqCst);
                    if first_timer != second_timer || first_worker != second_worker {
                        broken(
                            "clear: the page's timer or worker kept running after an empty source",
                        );
                        return;
                    }
                    CLEAR_PHASE.store(3, Ordering::SeqCst);
                    CLEAR_DONE.store(true, Ordering::SeqCst);
                    settle();
                });
            }
            LoadState::Loading => {
                // An estimate from the doomed page can still arrive between
                // the source write and the render that destroys it; it is a
                // native measurement, not a blank navigation.
                check_estimate(progress, CLEAR);
            }
            LoadState::Cancelled => {
                // The active load's cancellation is the only event the
                // clear may produce.
                if phase != 2 {
                    broken("clear: an unexpected cancellation");
                }
            }
            LoadState::Finished => {
                // The fixture's active subresource usually keeps the document
                // loading, but if the document finished just before the empty
                // source was applied, that finish is the old page's, not a
                // blank navigation. A finish after verification would mean
                // the destroyed page is still alive.
                if phase >= 3 {
                    broken("clear: the destroyed page reported a finish");
                    return;
                }
                if progress.progress.map(|value| value.0) != Some(1.0) || progress.message.is_some()
                {
                    broken("clear: Finished carried the wrong data");
                }
            }
            LoadState::Failed => {
                broken("clear: the fixture page failed instead of being destroyed");
            }
            _ => broken("clear: unknown load state"),
        }
    }

    /// The removed component must be silent, and the component added again
    /// must be a fresh one whose identity restarts at one.
    fn observe_teardown(progress: &LoadProgress, show: Signal<bool>) {
        if BROKEN.load(Ordering::SeqCst) || PASSED.load(Ordering::SeqCst) {
            return;
        }
        if !check_navigation(&TEARDOWN_NAVIGATION, TEARDOWN, progress.navigation) {
            return;
        }
        let phase = TEARDOWN_PHASE.load(Ordering::SeqCst);
        match progress.state {
            LoadState::Started | LoadState::Loading => {
                if phase == 0 {
                    if progress.state == LoadState::Loading {
                        check_estimate(progress, TEARDOWN);
                    }
                    // Drop the component while it is still loading. A
                    // detached thread owns the wait so the check is not
                    // owned by the component being dropped.
                    TEARDOWN_PHASE.store(1, Ordering::SeqCst);
                    show.set(false);
                    let _ = std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(1200));
                        if BROKEN.load(Ordering::SeqCst) {
                            return;
                        }
                        ACTIONS
                            .get()
                            .expect("the app is running")
                            .send(Action::Readd)
                            .expect("the engine owns the fixture commands");
                    });
                    return;
                }
                if progress.state == LoadState::Loading {
                    // The dropped view may still be alive for the turn that
                    // processes the removal; its measurements are legitimate
                    // until the component is gone. Termination is what must
                    // stay silent.
                    check_estimate(progress, TEARDOWN);
                    return;
                }
                if phase == 2 && progress.navigation == 1 {
                    TEARDOWN_PHASE.store(3, Ordering::SeqCst);
                } else {
                    broken("teardown: the added component did not restart its identity");
                }
            }
            LoadState::Finished => {
                if phase == 3
                    && progress.navigation == 1
                    && progress.progress.map(|value| value.0) == Some(1.0)
                    && progress.message.is_none()
                {
                    TEARDOWN_DONE.store(true, Ordering::SeqCst);
                    settle();
                } else {
                    broken("teardown: the added component did not finish cleanly");
                }
            }
            LoadState::Cancelled | LoadState::Failed => {
                broken("teardown: a terminal event escaped a dropped view");
            }
            _ => broken("teardown: unknown load state"),
        }
    }

    fn watcher(label: &'static str) -> impl Fn(&LoadProgress) + 'static {
        move |progress| {
            if label == SUCCESS {
                observe_success(progress);
            } else if label == FAILURE {
                observe_failure(progress);
            }
        }
    }

    #[component]
    pub fn App() -> impl IntoElement {
        let replacement_source = signal(hang_url());
        let revisit_source = signal(ok_url());
        let clear_source = signal(clear_url());
        let show_teardown = signal(true);
        let teardown_source = signal(hang_url());
        let (sender, receiver) = mpsc::channel();
        ACTIONS.set(sender).expect("one fixture app per process");
        // Worker threads observe HTTP counters; only the engine mutates signals.
        // This root-owned driver also survives removal of the child under test.
        set_interval(Duration::from_millis(10), move || {
            for action in receiver.try_iter() {
                match action {
                    Action::Clear => clear_source.set(String::new()),
                    Action::Readd => {
                        TEARDOWN_PHASE.store(2, Ordering::SeqCst);
                        teardown_source.set(ok_url());
                        show_teardown.set(true);
                    }
                }
            }
        });
        view()
            .flex_1()
            .child(
                nsh_ext::web_view()
                    .src(measured_success_url())
                    .on_load_progress(watcher(SUCCESS))
                    .flex_1(),
            )
            .child(
                nsh_ext::web_view()
                    .src(failure_url())
                    .on_load_progress(watcher(FAILURE))
                    .flex_1(),
            )
            .child(
                nsh_ext::web_view()
                    .src(replacement_source)
                    .on_load_progress(move |progress| {
                        observe_replacement(progress, replacement_source)
                    })
                    .flex_1(),
            )
            .child(
                nsh_ext::web_view()
                    .src(revisit_source)
                    .on_load_progress(move |progress| observe_revisit(progress, revisit_source))
                    .flex_1(),
            )
            .child(
                nsh_ext::web_view()
                    .src(clear_source)
                    .on_load_progress(observe_clear)
                    .flex_1(),
            )
            .child(view().flex_1().child(when(show_teardown, move || {
                nsh_ext::web_view()
                    .src(teardown_source)
                    .on_load_progress(move |progress| observe_teardown(progress, show_teardown))
                    .flex_1()
            })))
    }

    /// Serves the clear/revisit fixtures on loopback. Only this harness uses
    /// it; each request runs on its own thread so the page's timer, worker and
    /// pending image can be in flight at once.
    fn spawn_http_fixture() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the HTTP fixture");
        let port = listener.local_addr().expect("the fixture's address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let _ = std::thread::spawn(move || serve_http(stream));
            }
        });
        port
    }

    fn serve_http(mut stream: TcpStream) {
        let Ok(reader_stream) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader_stream);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) => break,
                Ok(_) if header == "\r\n" || header == "\n" => break,
                Ok(_) => {}
                Err(_) => return,
            }
        }
        let target = request.split_whitespace().nth(1).unwrap_or("/");
        let (route, query) = target.split_once('?').unwrap_or((target, ""));
        match route {
            "/ping" => {
                match query.split('&').find_map(|pair| pair.strip_prefix("kind=")) {
                    Some("timer") => {
                        let _ = TIMER_PINGS.fetch_add(1, Ordering::SeqCst);
                    }
                    Some("worker") => {
                        let _ = WORKER_PINGS.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
                write_response(&mut stream, "204 No Content", "text/plain", "");
            }
            "/page" => write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                CLEAR_PAGE,
            ),
            "/ok" => write_response(&mut stream, "200 OK", "text/html; charset=utf-8", OK_PAGE),
            "/error" => write_response(
                &mut stream,
                "500 Internal Server Error",
                "text/html; charset=utf-8",
                ERROR_PAGE,
            ),
            "/worker.js" => write_response(
                &mut stream,
                "200 OK",
                "application/javascript",
                WORKER_SCRIPT,
            ),
            "/slow" => {
                // Accept the image request and never answer it: the document
                // stays loading while its timer and worker keep running.
                std::thread::sleep(Duration::from_secs(120));
            }
            _ => write_response(&mut stream, "404 Not Found", "text/plain", "not found"),
        }
    }

    fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    pub fn run() {
        // Bind every loopback fixture before releasing the refused port, so
        // no later fixture can take it. The hanging listener then makes
        // WebKit's TLS connection fail natively, without any HTTP status
        // involved.
        let failure = TcpListener::bind(("127.0.0.1", 0)).expect("bind a refused port");
        let failure_port = failure.local_addr().expect("the bound address").port();
        let hang = TcpListener::bind(("127.0.0.1", 0)).expect("bind a hanging port");
        let hang_port = hang.local_addr().expect("the bound address").port();
        // The page's subresource stays pending until a real native estimate
        // reaches the app. Closing its TLS connection then lets the document
        // finish; no synthetic progress or arbitrary release timer is used.
        let asset = TcpListener::bind(("127.0.0.1", 0)).expect("bind asset fixture");
        let asset_port = asset.local_addr().expect("the bound address").port();
        let http_port = spawn_http_fixture();
        let ports = [failure_port, hang_port, asset_port, http_port];
        assert!(
            ports
                .iter()
                .enumerate()
                .all(|(index, port)| !ports[..index].contains(port)),
            "the fixture ports must be distinct"
        );
        FAILURE_PORT.store(failure_port, Ordering::SeqCst);
        HANG_PORT.store(hang_port, Ordering::SeqCst);
        ASSET_PORT.store(asset_port, Ordering::SeqCst);
        HTTP_PORT.store(http_port, Ordering::SeqCst);
        drop(failure);
        let (release, wait) = mpsc::channel();
        ASSET_RELEASE.set(release).expect("one fixture per process");
        std::thread::spawn(move || {
            if let Ok((connection, _)) = asset.accept() {
                let _ = wait.recv_timeout(TIMEOUT);
                drop(connection);
            }
        });
        let _ = std::thread::spawn(move || {
            for stream in hang.incoming().flatten() {
                std::mem::forget(stream);
            }
        });

        let _ = std::thread::spawn(|| {
            std::thread::sleep(TIMEOUT);
            if !PASSED.load(Ordering::SeqCst) {
                BROKEN.store(true, Ordering::SeqCst);
                eprintln!(
                    "nsh-ext-apple acceptance: FAIL: no pass within {} seconds",
                    TIMEOUT.as_secs()
                );
                request_exit(1);
            }
        });

        set_components(nsh_ext_apple::register_components);
        #[cfg(target_os = "macos")]
        let options = AppOptions {
            title: "nsh-ext-apple acceptance".to_owned(),
            app_id: "dev.nsh.ext.apple.acceptance".to_owned(),
            size: (900, 900),
            ..Default::default()
        };
        #[cfg(target_os = "ios")]
        let options = AppOptions::default();

        #[cfg(target_os = "macos")]
        let _ = run_app(AppSource::Rust(Root::of::<App>()), options);
        #[cfg(target_os = "ios")]
        run_app(AppSource::Rust(Root::of::<App>()), options);

        #[cfg(target_os = "macos")]
        if !(PASSED.load(Ordering::SeqCst) && !BROKEN.load(Ordering::SeqCst)) {
            eprintln!("nsh-ext-apple acceptance: FAIL (closed or stopped early)");
            std::process::exit(1);
        }
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn main() {
    apple::run();
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn main() {
    eprintln!("This acceptance harness runs on iOS, Mac Catalyst or macOS");
}
