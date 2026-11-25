//! A tiny local HTTP server shared by the acceptance example.
//!
//! It serves a successful page, a 404 page, a slow page and a marker page
//! so the acceptance can exercise completed HTTP errors, a navigation
//! replaced while it is loading, and page destruction. The successful pages
//! report their viewport size and `devicePixelRatio` back to the server:
//! that is the proof the WebView2 controller was actually sized by NSH and
//! rasterized at the window's scale.
//!
//! The marker page starts a timer, a worker and a request that stays open.
//! All three report to this server, so the acceptance can prove they stop
//! when the source is cleared: a real navigation destroys the document and
//! its timers, workers and requests, whereas rewriting the document in
//! place leaves them running.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const PAGE_OK: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <style>html,body{margin:0;height:100%;overflow:hidden;font-family:sans-serif}\
    </style></head><body>\
    <h1>ok</h1><script>\
    function report(){\
    fetch('/size?page='+encodeURIComponent(location.pathname)+\
    '&w='+encodeURIComponent(innerWidth)+'&h='+encodeURIComponent(innerHeight)+\
    '&dpr='+encodeURIComponent(devicePixelRatio)).catch(function(){});\
    }\
    report();addEventListener('resize',report);\
    </script></body></html>";

const PAGE_MISSING: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <title>missing</title></head><body><h1>missing</h1></body></html>";

/// A page whose timer, worker and open request never stop on their own.
///
/// They only stop when the document that owns them is destroyed, which is
/// what the server observes after the source is cleared. The worker script
/// is served from the page's own origin.
const PAGE_MARKER: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <title>marker</title></head><body>\
    <h1>marker</h1><script>\
    setInterval(function(){navigator.sendBeacon('/beat?kind=timer');},100);\
    const markerWorker=new Worker('/marker-worker.js');\
    fetch('/stream').catch(function(){});\
    </script></body></html>";

const MARKER_WORKER: &str = "function beat(){fetch('/beat?kind=worker').catch(function(){});}\
    beat();setInterval(beat,120);";

/// The URLs the acceptance navigates to.
#[derive(Clone, Debug)]
pub struct Urls {
    pub ok: String,
    pub missing: String,
    pub ok2: String,
    pub slow: String,
    pub next: String,
    pub refused: String,
    pub marker: String,
}

/// What a successful page reported about its viewport.
#[derive(Clone, Debug)]
pub struct Viewport {
    /// The page's path (`/ok`, `/ok2`, ...).
    pub page: String,
    /// `innerWidth` in CSS pixels.
    pub width: u32,
    /// `innerHeight` in CSS pixels.
    pub height: u32,
    /// `devicePixelRatio`: the controller's rasterization scale.
    pub dpr: f64,
}

/// A beat from the marker page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beat {
    /// The page's `setInterval`.
    Timer,
    /// The page's `Worker`.
    Worker,
}

/// The request the marker page keeps open while its document lives.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stream {
    /// How many times the marker page opened it.
    pub opened: u32,
    /// When the client aborted it, if it did.
    pub closed: Option<Instant>,
}

static URLS: OnceLock<Urls> = OnceLock::new();
static VIEWPORTS: Mutex<Vec<Viewport>> = Mutex::new(Vec::new());
static BEATS: Mutex<Vec<(Beat, Instant)>> = Mutex::new(Vec::new());
static STREAM: Mutex<Stream> = Mutex::new(Stream {
    opened: 0,
    closed: None,
});

pub fn set_urls(urls: Urls) {
    let _ = URLS.set(urls);
}

pub fn urls() -> Urls {
    URLS.get().expect("acceptance URLs are set").clone()
}

/// What the successful pages reported about their viewport.
pub fn viewports() -> Vec<Viewport> {
    VIEWPORTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Every beat the marker page's timer and worker sent, with its arrival.
pub fn beats() -> Vec<(Beat, Instant)> {
    BEATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The marker page's long-lived request.
pub fn stream() -> Stream {
    *STREAM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Both execution contexts have reported repeated activity and the marker's
/// long-lived request is open. Clearing before this would prove nothing.
pub fn marker_is_live() -> bool {
    let beats = BEATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let timers = beats
        .iter()
        .filter(|(kind, _)| *kind == Beat::Timer)
        .take(2)
        .count();
    let workers = beats
        .iter()
        .filter(|(kind, _)| *kind == Beat::Worker)
        .take(2)
        .count();
    let stream = stream();
    timers == 2 && workers == 2 && stream.opened > 0 && stream.closed.is_none()
}

/// The local server. Its thread lives for the process.
pub struct Server {
    pub urls: Urls,
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
        let port = listener.local_addr().expect("the local address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || handle(stream));
            }
        });
        let base = format!("http://127.0.0.1:{port}");
        Self {
            urls: Urls {
                ok: format!("{base}/ok"),
                missing: format!("{base}/missing"),
                ok2: format!("{base}/ok2"),
                slow: format!("{base}/slow"),
                next: format!("{base}/next"),
                refused: refused_url(),
                marker: format!("{base}/marker"),
            },
        }
    }
}

/// A loopback URL whose port was just released: connecting to it is refused
/// by the system, not answered with an HTTP error status.
fn refused_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().expect("the local address").port();
    drop(listener);
    format!("http://127.0.0.1:{port}/refused")
}

fn handle(mut stream: TcpStream) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    // Consume the headers.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    if path.starts_with("/stream") {
        hold_stream(&mut stream);
        return;
    }
    let content_type = if path == "/marker-worker.js" {
        "text/javascript"
    } else {
        "text/html"
    };
    let (status, body, delay) = match path {
        "/missing" => ("404 Not Found", PAGE_MISSING, None),
        "/slow" => ("200 OK", PAGE_OK, Some(Duration::from_millis(1500))),
        "/marker" => ("200 OK", PAGE_MARKER, None),
        "/marker-worker.js" => ("200 OK", MARKER_WORKER, None),
        "/ok" | "/ok2" | "/next" => ("200 OK", PAGE_OK, None),
        path if path.starts_with("/size?") => {
            record_size(path);
            ("204 No Content", "", None)
        }
        path if path.starts_with("/beat?") => {
            record_beat(path);
            ("204 No Content", "", None)
        }
        _ => ("404 Not Found", PAGE_MISSING, None),
    };
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Answers the marker page's request and keeps it open until the client
/// aborts it, which happens when the document holding it is destroyed.
fn hold_stream(stream: &mut TcpStream) {
    {
        let mut state = STREAM
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.opened += 1;
        state.closed = None;
    }
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
          Cache-Control: no-store\r\n\r\n",
    );
    let _ = stream.flush();
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let mut state = STREAM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.closed = Some(Instant::now());
}

fn record_size(path: &str) {
    let query = path.split_once('?').map_or("", |(_, query)| query);
    let mut page = String::new();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut dpr = 0.0f64;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match key {
            "page" => page = percent_decode(value),
            "w" => width = value.parse().unwrap_or_default(),
            "h" => height = value.parse().unwrap_or_default(),
            "dpr" => dpr = value.parse().unwrap_or_default(),
            _ => {}
        }
    }
    if width > 0 && height > 0 && dpr > 0.0 {
        VIEWPORTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Viewport {
                page,
                width,
                height,
                dpr,
            });
    }
}

fn record_beat(path: &str) {
    let kind = path
        .split_once("kind=")
        .map(|(_, kind)| kind)
        .unwrap_or_default();
    let beat = match kind {
        "timer" => Beat::Timer,
        "worker" => Beat::Worker,
        _ => return,
    };
    BEATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((beat, Instant::now()));
}

/// `%XX` escapes in a query value, as `encodeURIComponent` writes them.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}
