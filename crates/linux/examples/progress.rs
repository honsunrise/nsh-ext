//! A runnable NSH app showing the extension's WebView on Linux.
//!
//! ```console
//! cargo run --example progress -- https://example.com
//! ```
//!
//! The header reports the native load lifecycle the adapter emits: the
//! navigation identity, its state and WebKit's estimate when it has one.
//! Leaving the URL out shows `https://example.com`.

use nsh_core::prelude::*;
use nsh_ext::web_view;
use nsh_platform_linux::{AppOptions, AppSource, Root, run_app};

/// The app's root component.
struct App;

impl Component for App {
    type Props = ();
    type Event = NoEvent;

    fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
        let source = std::env::args()
            .nth(1)
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| "https://example.com".to_owned());
        let status = signal(format!("Waiting for {source}"));
        let header = status;

        view()
            .flex_1()
            .flex_col()
            .child(text(move || header.get()).p_2().flex_shrink_0())
            .child(
                web_view()
                    .src(source)
                    .on_load_progress(move |progress| {
                        let estimate = progress
                            .progress
                            .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
                        status.set(format!(
                            "navigation {}: {:?} ({estimate}){}",
                            progress.navigation,
                            progress.state,
                            progress
                                .message
                                .as_deref()
                                .map_or_else(String::new, |message| format!(" — {message}")),
                        ));
                    })
                    .flex_1(),
            )
    }
}

fn main() -> nsh_platform_linux::gtk4::glib::ExitCode {
    let options = AppOptions {
        title: "nsh-ext WebView".to_owned(),
        app_id: "com.example.nshext.webview".to_owned(),
        size: (960, 720),
        ..Default::default()
    };
    nsh_ext_linux::register(&options.components);
    run_app(AppSource::Rust(Root::of::<App>()), options)
}
