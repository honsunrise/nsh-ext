//! Native acceptance harness for the GPUI adapter: run it on each desktop and
//! judge the extension's actual native callbacks.
//!
//! ```sh
//! cargo run --example acceptance          # exits 0 when the contract holds
//! RUST_LOG=debug cargo run --example acceptance
//! ```
//!
//! It is a real Native Shell app (GPUI backend) whose page source is driven
//! through the public wire (`src`, `trackProgress`, `loadProgress`) on a script
//! that advances on the events themselves, so it does not race a wall clock:
//!
//! 1. a local page streamed slowly; once the engine reports an estimate (on
//!    Windows, which has no fraction, as soon as it starts) the source is
//!    replaced with the fast page, so a navigation is replaced while it is
//!    loading (a cancellation, then a real success);
//! 2. the slow page to completion, with native estimates;
//! 3. then the fast page again (a revisit of a URL that already finished);
//! 4. then the source is cleared (the browser is retired, with no blank navigation);
//! 5. then a refused connection (a real native failure: `127.0.0.1` with nobody
//!    listening; never an HTTP error status, which engines may render as a
//!    page);
//! 6. then the fast page again (a success after a failure); when it finishes
//!    the web view is removed from the tree and added again with a new source
//!    (teardown and a fresh native view, whose navigation identity starts at
//!    one again).
//!
//! It asserts, from the events themselves:
//!
//! - navigation identities are positive and JavaScript-safe, begin at one for
//!   each view, never go backwards within one view, and a navigation that
//!   ended never continues;
//! - every started navigation ends exactly once, and a terminal event names a
//!   navigation that started;
//! - there is exactly one failure in the whole run: a successful or replaced
//!   load reported as failed is a violation;
//! - on macOS the failure message carries the `NSError` domain and code of the
//!   `WKNavigationDelegate` failure callback, proving the failure is native and
//!   not inferred from an estimate or a frame;
//! - `started` carries no estimate and no message, `loading` always carries a
//!   native estimate, `finished` carries exactly one and no message, `failed`
//!   carries the engine's message and no estimate;
//! - Windows reports no loading estimates; confirmed success carries one.
//!   Failure is reported only by reading `NavigationCompleted`'s `IsSuccess`:
//!   wry's generic `PageLoadEvent::Finished` cannot tell success from failure,
//!   so the refused connection failing proves that the native verdict was read;
//! - macOS and Linux report at least one real estimate;
//! - at least one navigation finishes, the refused connection fails, and a
//!   replaced load is cancelled.
//!
//! A failure prints the violations and exits 1. The process exits directly (the
//! script's last step) so the exit code is the verdict.
//!
//! The harness mounts the app in an `AppView` in a GPUI window, just as an
//! embedding host does. It judges the native progress wire.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::TcpListener,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use nsh_core::prelude::*;
use nsh_ext::{LoadProgress, LoadState};
use nsh_platform_gpui::{Root, gpui};

mod support;

/// How long the script may take before the harness fails it.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the web view stays removed before it is added again.
const REMOVAL_GAP: Duration = Duration::from_millis(250);

/// The page the local server streams: large enough for the engine to report
/// intermediate progress while it arrives in chunks.
const PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>nsh-ext acceptance</title></head>\
<body style=\"margin:0;background:#0b0f14;color:#e6edf3;font-family:system-ui\">\
<main style=\"padding:2rem\"><h1>nsh-ext acceptance</h1>\
<p>This page is produced by the harness's own loopback server.</p>\
<p>It is written in chunks so WebKit's native estimate has values to report.</p>\
<p>Progress is the engine's, never a page script's.</p>\
</main></body></html>";

/// The replacement target cannot finish while its image response is pending.
const REPLACEMENT_PAGE: &str = "<!doctype html><html><body><h1>Pending load</h1><img src=\"/pending\" alt=\"Waiting\"></body></html>";

/// The harness's local server, and the URLs its script uses.
struct Server {
    replacement: String,
    slow: String,
    fast: String,
    refused: String,
}

/// Starts a loopback server: `/slow` streams the page over ~1.6 s and `/fast`
/// returns it at once; `/favicon.ico` is empty. The refusal URL is a port that
/// was bound and released, so connecting fails in the network stack before any
/// HTTP status exists.
fn server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the harness server binds a port");
    let port = listener
        .local_addr()
        .expect("the harness server has an address")
        .port();
    let refused = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("a refused port can be reserved");
        let port = probe.local_addr().expect("the probe has an address").port();
        drop(probe);
        port
    };
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut request = [0u8; 2048];
                let read = stream.read(&mut request).unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]);
                let path = head.split_whitespace().nth(1).unwrap_or("/");
                if path.starts_with("/favicon.ico") {
                    let _ =
                        stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                    return;
                }
                if path == "/pending" {
                    // Wait for cancellation rather than racing a fixed sleep.
                    let _ = stream.set_read_timeout(Some(SCRIPT_TIMEOUT));
                    let _ = stream.read(&mut [0u8; 1]);
                    return;
                }
                let body = if path == "/replace" {
                    REPLACEMENT_PAGE.as_bytes()
                } else {
                    PAGE.as_bytes()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).is_err() {
                    return;
                }
                if path.starts_with("/fast") || path == "/replace" {
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                    return;
                }
                for chunk in body.chunks(20) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(80));
                }
                let _ = stream.flush();
            });
        }
    });
    Server {
        replacement: format!("http://127.0.0.1:{port}/replace"),
        slow: format!("http://127.0.0.1:{port}/slow"),
        fast: format!("http://127.0.0.1:{port}/fast"),
        refused: format!("http://127.0.0.1:{refused}/"),
    }
}

/// What the script waits for next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// The slow page is loading: replace it once the engine has an estimate
    /// (Windows has none, so there the start itself is the signal).
    ReplaceSlow,
    /// The fast page is loading after replacing the slow one.
    AwaitFastAfterReplacement,
    /// The fast page finished: load the slow page to completion.
    AwaitSlowSuccess,
    /// The slow page finished: revisit the fast URL.
    AwaitRevisit,
    /// The revisited fast page finished: clear `src`.
    AwaitClear,
    /// The cleared source stayed silent: try the refused connection.
    AwaitRefusedFailure,
    /// The refused connection failed: load the fast page again.
    AwaitFastAfterFailure,
    /// The component is absent; callbacks must remain silent.
    AwaitRemoved,
    /// The web view was added again: wait for its page.
    VerdictAfterReadd,
    /// The script is over.
    Done,
}

/// What the harness does after recording an event.
enum Next {
    /// Replace `src`.
    Source(String),
    /// Remove the web view from the tree, then add it again.
    Remove,
    /// Print the verdict and exit with it.
    Verdict,
}

/// The contract checks, fed by the app's listener.
#[derive(Default)]
struct Report {
    events: usize,
    started: usize,
    loading: usize,
    finished: usize,
    failed: usize,
    cancelled: usize,
    /// Which native view the events belong to: a removed and re-added web view
    /// is a new one, whose identities begin at one again.
    view: u64,
    last_navigation: u64,
    last_started: u64,
    /// How each navigation ended, in each view.
    ended: BTreeMap<(u64, u64), &'static str>,
    /// Navigations that started and have not ended, in each view.
    pending: BTreeSet<(u64, u64)>,
    violations: Vec<String>,
}

impl Report {
    fn record(&mut self, event: &LoadProgress) {
        self.events += 1;
        println!(
            "[acceptance] view={} navigation={} state={:?} progress={:?} message={:?}",
            self.view, event.navigation, event.state, event.progress, event.message
        );
        if event.navigation == 0 || event.navigation > 9_007_199_254_740_991 {
            self.violate(format!(
                "navigation {} is not a positive JavaScript-safe integer",
                event.navigation
            ));
        }
        if event.navigation < self.last_navigation {
            self.violate(format!(
                "navigation {} went backwards after {} within one view",
                event.navigation, self.last_navigation
            ));
        }
        self.last_navigation = self.last_navigation.max(event.navigation);
        let ended = self.ended.get(&(self.view, event.navigation)).copied();
        match event.state {
            LoadState::Started => {
                self.started += 1;
                if self.last_started == 0 && event.navigation != 1 {
                    self.violate(format!(
                        "view {} began at navigation {} instead of one",
                        self.view, event.navigation
                    ));
                }
                if event.navigation <= self.last_started {
                    self.violate(format!(
                        "navigation {} started after {} (identities must increase)",
                        event.navigation, self.last_started
                    ));
                }
                self.last_started = self.last_started.max(event.navigation);
                if event.progress.is_some() {
                    self.violate(format!(
                        "navigation {} started with an estimate {:?}",
                        event.navigation, event.progress
                    ));
                }
                if event.message.is_some() {
                    self.violate(format!(
                        "navigation {} started with a message",
                        event.navigation
                    ));
                }
                self.pending.insert((self.view, event.navigation));
            }
            LoadState::Loading => {
                self.loading += 1;
                match event.progress {
                    Some(value) if value.0.is_finite() && (0.0..=1.0).contains(&value.0) => {}
                    other => self.violate(format!(
                        "navigation {} loading reported an invalid estimate {other:?}",
                        event.navigation
                    )),
                }
                if event.message.is_some() {
                    self.violate(format!(
                        "navigation {} loading reported a message",
                        event.navigation
                    ));
                }
                if ended.is_some() {
                    self.violate(format!(
                        "navigation {} continued after it ended",
                        event.navigation
                    ));
                }
            }
            LoadState::Finished => {
                self.finished += 1;
                if event.progress != Some(Percentage(1.0)) {
                    self.violate(format!(
                        "navigation {} finished with {:?}, not a confirmed 1",
                        event.navigation, event.progress
                    ));
                }
                if event.message.is_some() {
                    self.violate(format!(
                        "navigation {} finished with a message",
                        event.navigation
                    ));
                }
                self.terminate(event, "finished");
            }
            LoadState::Failed => {
                self.failed += 1;
                if event.progress.is_some() {
                    self.violate(format!(
                        "navigation {} failed with an estimate {:?}",
                        event.navigation, event.progress
                    ));
                }
                if event.message.as_deref().is_none_or(str::is_empty) {
                    self.violate(format!(
                        "navigation {} failed without the engine's message",
                        event.navigation
                    ));
                }
                if cfg!(target_os = "macos")
                    && let Some(message) = event.message.as_deref()
                    && !message.contains("NSURLErrorDomain")
                {
                    // Only the WKNavigationDelegate's error callback carries
                    // the engine's domain; an inferred stop carries a guess.
                    self.violate(format!(
                        "macOS navigation {} failed with {message:?}, which is not \
                         a WKNavigationDelegate NSError",
                        event.navigation
                    ));
                }
                self.terminate(event, "failed");
            }
            LoadState::Cancelled => {
                self.cancelled += 1;
                if event.progress.is_some() {
                    self.violate(format!(
                        "navigation {} was cancelled with an estimate",
                        event.navigation
                    ));
                }
                self.terminate(event, "cancelled");
            }
            _ => self.violate(format!(
                "navigation {} reported an unknown state",
                event.navigation
            )),
        }
    }

    fn terminate(&mut self, event: &LoadProgress, how: &'static str) {
        if let Some(previous) = self.ended.insert((self.view, event.navigation), how) {
            self.violate(format!(
                "navigation {} ended twice ({previous}, then {how})",
                event.navigation
            ));
        }
        if !self.pending.remove(&(self.view, event.navigation)) {
            self.violate(format!(
                "navigation {} ended ({how}) without starting",
                event.navigation
            ));
        }
    }

    /// A web view was removed and another added: the next events are another
    /// view's, whose identities begin at one.
    fn begin_view(&mut self) {
        self.view += 1;
        self.last_navigation = 0;
        self.last_started = 0;
    }

    fn violate(&mut self, message: String) {
        self.violations.push(message);
    }

    /// Adds what the script expects of a whole run, prints, and returns whether
    /// the contract held.
    fn verdict(&mut self) -> bool {
        if self.events == 0 {
            self.violate("no loadProgress event reached the app".to_owned());
        }
        if self.finished == 0 {
            self.violate("no navigation reached native completion".to_owned());
        }
        if self.failed != 1 {
            self.violate(format!(
                "{} failure(s) reached the app; only the refused connection may fail",
                self.failed
            ));
        }
        if self.cancelled == 0 {
            self.violate("the replaced load was not reported as cancelled".to_owned());
        }
        if let Some((view, navigation)) = self.pending.iter().next() {
            self.violate(format!(
                "view {view} navigation {navigation} started but never ended"
            ));
        }
        if self.view == 0 {
            self.violate("the web view was not removed and added again".to_owned());
        }
        if cfg!(windows) {
            // WebView2 has no fraction: an absent estimate must stay absent,
            // not become zero.
            if self.loading != 0 {
                self.violate(format!(
                    "WebView2 reported {} numeric estimate event(s); it has none",
                    self.loading
                ));
            }
        } else if self.loading == 0 {
            self.violate("the engine reported no native estimate at all".to_owned());
        }
        println!(
            "[acceptance] events={} started={} loading={} finished={} failed={} cancelled={} views={}",
            self.events,
            self.started,
            self.loading,
            self.finished,
            self.failed,
            self.cancelled,
            self.view + 1,
        );
        if self.violations.is_empty() {
            println!("[acceptance] PASS");
            true
        } else {
            for violation in &self.violations {
                println!("[acceptance] FAIL {violation}");
            }
            println!(
                "[acceptance] FAILED with {} violation(s)",
                self.violations.len()
            );
            false
        }
    }
}

/// The app's root component.
struct Acceptance;

impl Component for Acceptance {
    type Props = ();
    type Event = NoEvent;

    fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
        let server = server();
        let source = signal(server.replacement.clone());
        let visible = signal(true);
        let step = Rc::new(Cell::new(Step::ReplaceSlow));
        let report = Rc::new(RefCell::new(Report::default()));
        let resume_at = Rc::new(Cell::new(None::<Instant>));

        // A script that has not finished by then failed.
        {
            let report = report.clone();
            let step = step.clone();
            set_timeout(SCRIPT_TIMEOUT, move || {
                let mut report = report.borrow_mut();
                report.violate(format!("script timed out in {:?}", step.get()));
                report.verdict();
                std::process::exit(1);
            });
        }

        // The root owns this clock, so removing a child cannot cancel re-add.
        {
            let step = step.clone();
            let report = report.clone();
            let resume_at = resume_at.clone();
            let refused = server.refused.clone();
            let again = format!("{}?again", server.fast);
            set_interval(Duration::from_millis(20), move || {
                if !resume_at.get().is_some_and(|at| Instant::now() >= at) {
                    return;
                }
                resume_at.set(None);
                match step.get() {
                    Step::AwaitClear => {
                        step.set(Step::AwaitRefusedFailure);
                        source.set(refused.clone());
                    }
                    Step::AwaitRemoved => {
                        report.borrow_mut().begin_view();
                        step.set(Step::VerdictAfterReadd);
                        source.set(again.clone());
                        visible.set(true);
                    }
                    _ => report
                        .borrow_mut()
                        .violate("unexpected delayed step".to_owned()),
                }
            });
        }

        // The script: record the event, then take the step it calls for.
        let script: Rc<dyn Fn(&LoadProgress)> = {
            let step = step.clone();
            let report = report.clone();
            let slow = server.slow.clone();
            let fast = server.fast.clone();
            Rc::new(move |event: &LoadProgress| {
                println!("[acceptance] step={:?}", step.get());
                report.borrow_mut().record(event);
                let next = match (event.state, step.get()) {
                    // Replace the slow navigation while it is loading: with a
                    // native estimate where the engine has one, at the native
                    // start on Windows (which has no fraction at all).
                    (LoadState::Loading, Step::ReplaceSlow) if !cfg!(windows) => {
                        step.set(Step::AwaitFastAfterReplacement);
                        Some(Next::Source(fast.clone()))
                    }
                    (LoadState::Started, Step::ReplaceSlow) if cfg!(windows) => {
                        step.set(Step::AwaitFastAfterReplacement);
                        Some(Next::Source(fast.clone()))
                    }
                    // The slow page finished before there was a chance to
                    // replace it; the verdict's cancelled check reports it.
                    (LoadState::Finished, Step::ReplaceSlow) => {
                        report.borrow_mut().violate(
                            "the slow page finished before the harness replaced it".to_owned(),
                        );
                        step.set(Step::AwaitFastAfterReplacement);
                        Some(Next::Source(fast.clone()))
                    }
                    (LoadState::Finished, Step::AwaitFastAfterReplacement) => {
                        step.set(Step::AwaitSlowSuccess);
                        Some(Next::Source(slow.clone()))
                    }
                    (LoadState::Finished, Step::AwaitSlowSuccess) => {
                        step.set(Step::AwaitRevisit);
                        Some(Next::Source(fast.clone()))
                    }
                    (LoadState::Finished, Step::AwaitRevisit) => {
                        step.set(Step::AwaitClear);
                        resume_at.set(Some(Instant::now() + REMOVAL_GAP));
                        Some(Next::Source(String::new()))
                    }
                    (_, Step::AwaitClear | Step::AwaitRemoved) => {
                        report
                            .borrow_mut()
                            .violate("a cleared or removed view emitted a native event".to_owned());
                        None
                    }
                    (LoadState::Failed, Step::AwaitRefusedFailure) => {
                        step.set(Step::AwaitFastAfterFailure);
                        Some(Next::Source(fast.clone()))
                    }
                    (LoadState::Finished, Step::AwaitFastAfterFailure) => Some(Next::Remove),
                    (LoadState::Finished, Step::VerdictAfterReadd) => Some(Next::Verdict),
                    _ => None,
                };
                match next {
                    Some(Next::Source(url)) => source.set(url),
                    Some(Next::Remove) => {
                        // The tree drops the component (and with it the native
                        // view) while it is not drawn; a fresh one is added
                        // shortly, as another view.
                        step.set(Step::AwaitRemoved);
                        visible.set(false);
                        resume_at.set(Some(Instant::now() + REMOVAL_GAP));
                    }
                    Some(Next::Verdict) => {
                        step.set(Step::Done);
                        let passed = report.borrow_mut().verdict();
                        std::process::exit(if passed { 0 } else { 1 });
                    }
                    None => {}
                }
            })
        };

        view()
            .flex_1()
            .bg(rgb(0x0b0f14))
            .child(when(visible, move || {
                let script = script.clone();
                nsh_ext::web_view()
                    .size_full()
                    .src(source)
                    .on_load_progress(move |event| script(event))
            }))
    }
}

fn main() -> std::process::ExitCode {
    env_logger::init();
    assert_eq!(
        nsh_ext_gpui::COMPONENT_NAME,
        nsh_ext::COMPONENT_NAME,
        "the adapter registers the component name the shared wrapper sends"
    );
    println!(
        "[acceptance] {} on {}",
        env!("CARGO_PKG_NAME"),
        std::env::consts::OS
    );
    let root = Root::of::<Acceptance>().app_id("dev.nsh.ext.gpui.acceptance");
    support::run(
        root,
        "nsh-ext acceptance",
        gpui::size(gpui::px(900.0), gpui::px(640.0)),
        true,
    );
    eprintln!("[acceptance] FAIL window closed before the script had a verdict");
    std::process::ExitCode::FAILURE
}
