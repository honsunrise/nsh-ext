//! Native acceptance harness for the Linux adapter.
//!
//! Runs the adapter in a real NSH app with real GTK 4 / WebKitGTK widgets
//! against a local HTTP server, and checks the lifecycle the adapter emits.
//!
//! ```console
//! cargo run --example acceptance             # every scenario, each in its own process
//! cargo run --example acceptance -- success  # one scenario
//! cargo run --example acceptance -- slow
//! cargo run --example acceptance -- failure
//! cargo run --example acceptance -- replacement
//! cargo run --example acceptance -- replacement-failure
//! cargo run --example acceptance -- hidden-clear
//! cargo run --example acceptance -- disposal
//! ```
//!
//! It needs a display. Without a session, wrap it:
//!
//! ```console
//! xvfb-run -a cargo run --example acceptance
//! ```
//!
//! The exit status is `0` only when every selected scenario met its
//! assertions; a failure exits `1`, and a setup failure (no display, bad
//! arguments) exits `2`.
//!
//! What it checks, through the real `on_load_progress` callbacks:
//!
//! - `started` and `cancelled` carry no numeric estimate (`progress: null`),
//!   never a fabricated zero; `finished` is a confirmed success with `1.0`.
//! - `slow` requires WebKit's own intermediate `estimated-load-progress`.
//! - `failure` loads a refused connection (not an HTTP error status) and
//!   requires `failed` with WebKit's message and no `finished`.
//! - `replacement` changes `src` mid-load: the replaced navigation is
//!   `cancelled`, and the new one finishes.
//! - `replacement-failure` replaces a load with a URL that refuses the
//!   connection: the replaced navigation is `cancelled`, and the genuine
//!   failure of the replacement is reported as `failed`, never suppressed.
//! - `hidden-clear` unmaps the real WebKit widget mid-load, clears `src`
//!   while it is unmapped, and later gives it a new source: the hidden load
//!   is `cancelled` and cleared, then the new navigation finishes.
//! - `disposal` removes the element mid-load and requires no terminal event.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nsh_core::prelude::*;
use nsh_ext::{LoadProgress, LoadState, web_view};
use nsh_platform_linux::gtk4 as gtk;
use nsh_platform_linux::gtk4::glib::{self, prelude::*};
use nsh_platform_linux::gtk4::prelude::*;
use nsh_platform_linux::{App, AppOptions, AppView, Root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Success,
    Slow,
    Failure,
    Replacement,
    ReplacementFailure,
    HiddenClear,
    Disposal,
}

impl Scenario {
    const ALL: [Self; 7] = [
        Self::Success,
        Self::Slow,
        Self::Failure,
        Self::Replacement,
        Self::ReplacementFailure,
        Self::HiddenClear,
        Self::Disposal,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "slow" => Some(Self::Slow),
            "failure" => Some(Self::Failure),
            "replacement" => Some(Self::Replacement),
            "replacement-failure" => Some(Self::ReplacementFailure),
            "hidden-clear" => Some(Self::HiddenClear),
            "disposal" => Some(Self::Disposal),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Slow => "slow",
            Self::Failure => "failure",
            Self::Replacement => "replacement",
            Self::ReplacementFailure => "replacement-failure",
            Self::HiddenClear => "hidden-clear",
            Self::Disposal => "disposal",
        }
    }
}

#[derive(Debug)]
struct Config {
    scenario: Scenario,
    base: String,
    first_url: String,
    second_url: String,
    failure_url: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

// GTK's scenario driver sends commands; only the engine mutates its signal.
static SOURCE: LazyLock<(mpsc::Sender<String>, Mutex<mpsc::Receiver<String>>)> =
    LazyLock::new(|| {
        let (send, receive) = mpsc::channel();
        (send, Mutex::new(receive))
    });

fn config() -> &'static Config {
    CONFIG
        .get()
        .expect("the acceptance config is set before the app loads")
}

fn set_source(source: &str) {
    SOURCE
        .0
        .send(source.to_owned())
        .expect("engine source channel");
}

/// The real WebKit widget the adapter created, found under the app view.
fn find_web_view(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.is::<webkit6::WebView>() {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_web_view(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

#[derive(Debug)]
struct Event {
    at: Instant,
    progress: LoadProgress,
}

static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());

fn record(progress: &LoadProgress) {
    // Printed as it happens: a failing run shows where it stopped.
    eprintln!(
        "event navigation={} state={:?} progress={:?} message={:?}",
        progress.navigation, progress.state, progress.progress, progress.message
    );
    EVENTS
        .lock()
        .expect("no other thread panicked while recording")
        .push(Event {
            at: Instant::now(),
            progress: progress.clone(),
        });
}

fn snapshot() -> Vec<Event> {
    EVENTS
        .lock()
        .expect("no other thread panicked while recording")
        .iter()
        .map(|event| Event {
            at: event.at,
            progress: event.progress.clone(),
        })
        .collect()
}

/// A real HTTP server: fast pages, a body delivered in chunks, and no
/// response at all on the failure URL (a closed port).
struct Server {
    base: String,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the acceptance server");
        let base = format!(
            "http://{}/",
            listener.local_addr().expect("the server's address")
        );
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || serve(stream));
            }
        });
        Self { base }
    }
}

/// A URL on a port that was just free: connecting to it is a real network
/// failure, not a restricted-port policy decision and not an HTTP error
/// status.
fn closed_port_url() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a port to close");
    let address = listener.local_addr().expect("the closed port's address");
    drop(listener);
    format!("http://{address}/")
}

fn serve(mut stream: TcpStream) {
    let mut request = [0u8; 4096];
    let read = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");
    match path {
        "/ok" => respond(
            &mut stream,
            200,
            b"<!doctype html><title>ok</title><h1>ok</h1>",
        ),
        "/second" => respond(
            &mut stream,
            200,
            b"<!doctype html><title>second</title><h1>second</h1>",
        ),
        "/slow" => slow_response(&mut stream),
        _ => respond(&mut stream, 404, b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// A page whose known-length body arrives in pieces over about a second, so
/// WebKit reports native estimates while it loads.
fn slow_response(stream: &mut TcpStream) {
    const CHUNKS: usize = 16;
    const CHUNK: usize = 64 * 1024;
    let mut body = b"<!doctype html><title>slow</title><h1>slow</h1><!--".to_vec();
    body.resize(CHUNK * CHUNKS - 16, b'.');
    body.extend_from_slice(b"--></body></html>");
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    for chunk in body.chunks(CHUNK) {
        if stream.write_all(chunk).is_err() || stream.flush().is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(70));
    }
}

struct HarnessApp;

impl Component for HarnessApp {
    type Props = ();
    type Event = NoEvent;

    fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
        let config = config();
        let shown = signal(true);
        let source = signal(match config.scenario {
            // Replaced, failure-replaced, hidden-cleared and disposed loads
            // must still be in flight when the harness acts.
            Scenario::Slow
            | Scenario::Replacement
            | Scenario::ReplacementFailure
            | Scenario::HiddenClear
            | Scenario::Disposal => format!("{}slow", config.base),
            Scenario::Failure => config.failure_url.clone(),
            Scenario::Success => config.first_url.clone(),
        });
        set_interval(Duration::from_millis(10), move || {
            loop {
                let next = SOURCE.1.lock().unwrap().try_recv();
                match next {
                    Ok(next) => source.set(next),
                    Err(_) => break,
                }
            }
        });

        let listener: Rc<dyn Fn(&LoadProgress)> = Rc::new(move |progress: &LoadProgress| {
            record(progress);
            if progress.navigation != 1 || progress.state != LoadState::Started {
                return;
            }
            match config.scenario {
                Scenario::Replacement => {
                    let second = config.second_url.clone();
                    set_timeout(Duration::from_millis(200), move || source.set(second));
                }
                Scenario::ReplacementFailure => {
                    let failure = config.failure_url.clone();
                    set_timeout(Duration::from_millis(200), move || source.set(failure));
                }
                Scenario::Disposal => {
                    // The slow response is still in flight when the
                    // element goes away.
                    set_timeout(Duration::from_millis(250), move || shown.set(false));
                }
                // `hidden-clear` is driven from `run`, which unmaps the
                // real GTK widget itself.
                _ => {}
            }
        });

        view().flex_1().child(when(
            move || shown.get(),
            move || {
                let listener = listener.clone();
                web_view()
                    .src(move || source.get())
                    .on_load_progress(move |progress| listener(progress))
                    .flex_1()
            },
        ))
    }
}

fn main() {
    match std::env::args().nth(1) {
        None => run_all(),
        Some(name) => {
            let Some(scenario) = Scenario::parse(&name) else {
                eprintln!(
                    "usage: acceptance <success|slow|failure|replacement|replacement-failure|hidden-clear|disposal>"
                );
                std::process::exit(2);
            };
            run(scenario);
        }
    }
}

/// Runs every scenario in a child process of its own: each has one app, one
/// GTK loop and one WebKit view.
fn run_all() {
    let executable = std::env::current_exe().expect("the acceptance executable's path");
    let mut failed = Vec::new();
    for scenario in Scenario::ALL {
        eprintln!("acceptance: running {}...", scenario.name());
        match Command::new(&executable).arg(scenario.name()).status() {
            Ok(status) if status.success() => {}
            _ => failed.push(scenario),
        }
    }
    if failed.is_empty() {
        println!("acceptance: all scenarios passed");
    } else {
        eprintln!(
            "acceptance: failed scenarios: {}",
            failed
                .iter()
                .map(|scenario| scenario.name())
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::process::exit(1);
    }
}

fn run(scenario: Scenario) {
    let server = Server::start();
    CONFIG
        .set(Config {
            scenario,
            base: server.base.clone(),
            first_url: format!("{}ok", server.base),
            second_url: format!("{}second", server.base),
            failure_url: closed_port_url(),
        })
        .unwrap_or_else(|_| panic!("the acceptance config is set once"));

    if gtk::init().is_err() {
        eprintln!(
            "acceptance: GTK could not be initialized (is a display available? try xvfb-run -a)"
        );
        std::process::exit(2);
    }

    let options = AppOptions {
        title: "nsh-ext acceptance".to_owned(),
        app_id: "com.example.nshext.acceptance".to_owned(),
        size: (900, 600),
        ..Default::default()
    };
    nsh_ext_linux::register(&options.components);
    let view = AppView::new(options);
    let window = gtk::Window::new();
    window.set_title(Some("nsh-ext acceptance"));
    window.set_default_size(900, 600);
    window.set_child(Some(&view.widget()));
    window.present();
    view.load(App::Rust(Root::of::<HarnessApp>()));

    let main_loop = Rc::new(glib::MainLoop::new(None, false));
    let started = Instant::now();
    let root_widget = view.widget();
    let deadline = if scenario == Scenario::Disposal {
        Duration::from_secs(4)
    } else {
        Duration::from_secs(20)
    };
    {
        let main_loop = main_loop.clone();
        let root_widget = root_widget.clone();
        let mut settled_at = None;
        let mut hidden_step = 0u8;
        let mut hidden_at = Instant::now();
        let mut hidden_root: Option<glib::WeakRef<gtk::Widget>> = None;
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let events = snapshot();
            if scenario == Scenario::HiddenClear {
                // The widget is unmapped by GTK itself: this is a real
                // hidden view, not a removed element. Clearing while it is
                // unmapped must still stop the load and clear the page.
                match hidden_step {
                    0 if events.iter().any(|event| {
                        event.progress.navigation == 1 && event.progress.state == LoadState::Started
                    }) =>
                    {
                        if let Some(web) = find_web_view(&root_widget) {
                            let component_root = web.parent().expect("extension-owned root");
                            hidden_root = Some(component_root.downgrade());
                            component_root.set_visible(false);
                            set_source("");
                            hidden_step = 1;
                            hidden_at = Instant::now();
                        }
                    }
                    1 if hidden_at.elapsed() >= Duration::from_millis(300) => {
                        // A new source while still unmapped waits for the map.
                        set_source(&config().second_url);
                        hidden_step = 2;
                        hidden_at = Instant::now();
                    }
                    2 if hidden_at.elapsed() >= Duration::from_millis(300) => {
                        if let Some(root) = hidden_root.as_ref().and_then(glib::WeakRef::upgrade) {
                            root.set_visible(true);
                        }
                        hidden_step = 3;
                    }
                    _ => {}
                }
            }
            // A grace after the expected terminal event catches a late,
            // contradictory one (a `finished` after `failed`, say).
            if settled_at.is_none() && verdict(&events, scenario).is_ok() {
                settled_at = Some(Instant::now());
            }
            let timed_out = started.elapsed() > deadline;
            let settled = settled_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(600));
            // Disposal has no terminal event to wait for: its fixed wait is
            // the point of the scenario.
            if (settled && scenario != Scenario::Disposal) || timed_out {
                main_loop.quit();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
    main_loop.run();

    let events = snapshot();
    match verdict(&events, scenario) {
        Ok(()) => {
            println!(
                "acceptance: {} passed ({} events)",
                scenario.name(),
                events.len()
            );
        }
        Err(reason) => {
            eprintln!("acceptance: {} FAILED: {reason}", scenario.name());
            for event in &events {
                eprintln!("  after {:?}: {:?}", event.at - started, event.progress);
            }
            std::process::exit(1);
        }
    }
}

/// What each scenario must observe, from the adapter's emitted events only.
fn verdict(events: &[Event], scenario: Scenario) -> Result<(), String> {
    for event in events {
        require(
            event.progress.navigation > 0,
            "navigation identities begin at one",
        )?;
        if event.progress.state == LoadState::Loading {
            require(
                event
                    .progress
                    .progress
                    .is_some_and(|value| (0.0..=1.0).contains(&value.0)),
                "`loading` carries the native estimate as a fraction",
            )?;
        }
    }

    match scenario {
        Scenario::Success => {
            expect_no_estimate(started_event(events, 1)?)?;
            let states = states_of(events, 1);
            require(
                !states.contains(&LoadState::Failed) && !states.contains(&LoadState::Cancelled),
                "a successful load must not report failure or cancellation",
            )?;
            expect_success(events, 1)?;
        }
        Scenario::Slow => {
            expect_no_estimate(started_event(events, 1)?)?;
            let intermediate = events_of(events, 1).any(|event| {
                event.progress.state == LoadState::Loading
                    && event
                        .progress
                        .progress
                        .is_some_and(|value| value.0 > 0.0 && value.0 < 1.0)
            });
            require(
                intermediate,
                "the slow response must report a native estimate between 0 and 1 while loading",
            )?;
            let states = states_of(events, 1);
            require(
                !states.contains(&LoadState::Failed) && !states.contains(&LoadState::Cancelled),
                "the slow page must not report failure or cancellation",
            )?;
            expect_success(events, 1)?;
        }
        Scenario::Failure => {
            expect_no_estimate(started_event(events, 1)?)?;
            let failed = events_of(events, 1)
                .find(|event| event.progress.state == LoadState::Failed)
                .ok_or("the refused connection must report `failed`")?;
            require(
                failed
                    .progress
                    .message
                    .as_deref()
                    .is_some_and(|message| !message.is_empty()),
                "a failure must carry WebKit's message",
            )?;
            require(
                failed.progress.progress.is_none(),
                "a failure carries no estimate",
            )?;
            require(
                !states_of(events, 1).contains(&LoadState::Finished),
                "a failed navigation must not report `finished`",
            )?;
        }
        Scenario::Replacement => {
            expect_no_estimate(started_event(events, 1)?)?;
            let cancelled = events_of(events, 1)
                .find(|event| event.progress.state == LoadState::Cancelled)
                .ok_or("replacing the source must report the old navigation as `cancelled`")?;
            expect_no_estimate(cancelled)?;
            require(
                states_of(events, 1).last() == Some(&LoadState::Cancelled),
                "the replaced navigation must end in `cancelled`",
            )?;
            require(
                !states_of(events, 1).contains(&LoadState::Failed),
                "the replaced navigation must not report a failure",
            )?;
            expect_no_estimate(started_event(events, 2)?)?;
            require(
                !states_of(events, 2).contains(&LoadState::Failed),
                "the replacing navigation must not report a failure",
            )?;
            expect_success(events, 2)?;
        }
        Scenario::ReplacementFailure => {
            expect_no_estimate(started_event(events, 1)?)?;
            require(
                states_of(events, 1).last() == Some(&LoadState::Cancelled),
                "replacing the source must end the old navigation in `cancelled`",
            )?;
            require(
                !states_of(events, 1).contains(&LoadState::Failed),
                "the replaced navigation must not report a failure",
            )?;
            expect_no_estimate(started_event(events, 2)?)?;
            let failed = events_of(events, 2)
                .find(|event| event.progress.state == LoadState::Failed)
                .ok_or("the replacement's refused connection must report `failed`")?;
            require(
                failed
                    .progress
                    .message
                    .as_deref()
                    .is_some_and(|message| !message.is_empty()),
                "the replacement's failure must carry WebKit's message",
            )?;
            require(
                failed.progress.progress.is_none(),
                "a failure carries no estimate",
            )?;
            require(
                !states_of(events, 2).contains(&LoadState::Finished),
                "a failed replacement must not report `finished`",
            )?;
        }
        Scenario::HiddenClear => {
            expect_no_estimate(started_event(events, 1)?)?;
            require(
                states_of(events, 1).last() == Some(&LoadState::Cancelled),
                "clearing `src` on an unmapped view must cancel its navigation",
            )?;
            require(
                !states_of(events, 1)
                    .iter()
                    .any(|state| matches!(state, LoadState::Failed | LoadState::Finished)),
                "the cleared navigation must not report a terminal success or failure",
            )?;
            expect_no_estimate(started_event(events, 2)?)?;
            require(
                !states_of(events, 2).contains(&LoadState::Failed),
                "the source assigned while hidden must not fail",
            )?;
            expect_success(events, 2)?;
        }
        Scenario::Disposal => {
            expect_no_estimate(started_event(events, 1)?)?;
            let states = states_of(events, 1);
            require(
                states.last() == Some(&LoadState::Started)
                    || states.last() == Some(&LoadState::Loading),
                "a disposed view must not report a terminal event",
            )?;
        }
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn events_of<'a>(
    events: &'a [Event],
    navigation: u64,
) -> impl DoubleEndedIterator<Item = &'a Event> + 'a {
    events
        .iter()
        .filter(move |event| event.progress.navigation == navigation)
}

fn states_of(events: &[Event], navigation: u64) -> Vec<LoadState> {
    events_of(events, navigation)
        .map(|event| event.progress.state)
        .collect()
}

fn started_event(events: &[Event], navigation: u64) -> Result<&Event, String> {
    events_of(events, navigation)
        .find(|event| event.progress.state == LoadState::Started)
        .ok_or_else(|| format!("navigation {navigation} must report `started`"))
}

/// An unavailable estimate is `None`, never zero and never one.
fn expect_no_estimate(event: &Event) -> Result<(), String> {
    require(
        event.progress.progress.is_none(),
        "`started` and `cancelled` must carry no numeric estimate",
    )
}

fn expect_success(events: &[Event], navigation: u64) -> Result<(), String> {
    require(
        states_of(events, navigation).last() == Some(&LoadState::Finished),
        &format!("navigation {navigation} must finish with `finished`"),
    )?;
    let finished = events_of(events, navigation)
        .rev()
        .find(|event| event.progress.state == LoadState::Finished)
        .ok_or("the `finished` event is missing")?;
    require(
        finished
            .progress
            .progress
            .is_some_and(|value| (value.0 - 1.0).abs() <= f32::EPSILON),
        "`finished` is confirmed success and reports 1.0",
    )
}
