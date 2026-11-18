//! The extension's web view in a running Native Shell (GPUI) app, with its
//! native loading progress on screen.
//!
//! ```sh
//! cargo run --example progress -- https://example.com/
//! RUST_LOG=info cargo run --example progress      # logs every loadProgress
//! ```
//!
//! The header reports the native load lifecycle the adapter emits: the
//! navigation identity, its state, the engine's estimate when it has one, and
//! the engine's message for a failure. Without an argument the demo serves its
//! own page from a loopback HTTP server (no network) in small chunks, so the
//! engines' own estimates have something to report while it arrives.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

use nsh_core::prelude::*;
use nsh_ext::{LoadProgress, LoadState};
use nsh_platform_gpui::{AppOptions, AppSource, Root, run_app};

/// What the demo's loopback server streams: large enough, and written slowly
/// enough, that the engine reports intermediate progress.
const PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>nsh-ext GPUI web view</title></head>\
<body style=\"margin:0;background:#101820;color:#e6edf3;font-family:system-ui,sans-serif\">\
<main style=\"padding:2rem\"><h1>nsh-ext</h1>\
<p>A native web view owned by this app, filling its Native Shell container.</p>\
<p>The header line reports the engine's own <code>loadProgress</code>.</p>\
</main></body></html>";

/// Serves [`PAGE`] from a loopback port, over about a second, and returns its
/// URL. The server lives as long as the app.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the demo server binds a loopback port");
    let port = listener
        .local_addr()
        .expect("the demo server has an address")
        .port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut request = [0u8; 1024];
                let read = stream.read(&mut request).unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]);
                let path = head.split_whitespace().nth(1).unwrap_or("/");
                if path.starts_with("/favicon.ico") {
                    let _ =
                        stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                    return;
                }
                let body = PAGE.as_bytes();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).is_err() {
                    return;
                }
                // Chunks, so a fraction of the load is there to report.
                for chunk in body.chunks(32) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(40));
                }
                let _ = stream.flush();
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// One `loadProgress` event, as the header shows it.
fn describe(progress: &LoadProgress) -> String {
    let estimate = progress
        .progress
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    match progress.state {
        LoadState::Started => format!("navigation {}: started", progress.navigation),
        LoadState::Loading => format!("navigation {}: loading ({estimate})", progress.navigation),
        LoadState::Finished => format!("navigation {}: finished", progress.navigation),
        LoadState::Failed => format!(
            "navigation {}: failed — {}",
            progress.navigation,
            progress.message.as_deref().unwrap_or("no message")
        ),
        LoadState::Cancelled => format!("navigation {}: cancelled", progress.navigation),
        _ => format!("navigation {}: {:?}", progress.navigation, progress.state),
    }
}

/// The app's root component.
struct Demo;

impl Component for Demo {
    type Props = ();
    type Event = NoEvent;

    fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
        let source = std::env::args()
            .nth(1)
            .filter(|url| !url.is_empty())
            .unwrap_or_else(serve);
        let status = signal(format!("waiting for {source}"));
        let header = status;
        view()
            .flex_1()
            .flex_col()
            .bg(rgb(0x101820))
            .child(
                text(move || header.get())
                    .p_2()
                    .flex_shrink_0()
                    .text_color(rgb(0xe6edf3)),
            )
            .child(
                nsh_ext::web_view()
                    .src(source)
                    .on_load_progress(move |progress| {
                        log::info!("loadProgress {progress:?}");
                        status.set(describe(progress));
                    })
                    .flex_1(),
            )
    }
}

fn main() -> std::process::ExitCode {
    env_logger::init();
    let root = Root::of::<Demo>().app_id("dev.nsh.ext.gpui.progress");
    let options = nsh_ext_gpui::install(AppOptions {
        title: "nsh-ext · GPUI web view".to_owned(),
        app_id: "dev.nsh.ext.gpui.progress".to_owned(),
        size: (1000, 700),
        ..Default::default()
    });
    run_app(AppSource::Rust(root), options)
}
