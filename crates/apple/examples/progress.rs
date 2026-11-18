//! Interactive WebView demo for iOS, Mac Catalyst and macOS.
//!
//! It registers the adapter through the platform's component hook and loads
//! a self-contained page. Use it to watch `loadProgress` in Xcode's console
//! (or `simctl launch --console`); the macOS acceptance harness is the
//! pass/fail check.
//!
//! ```sh
//! cargo run --manifest-path crates/apple/Cargo.toml --example progress
//! crates/apple/examples/run-ios-simulator.sh
//! crates/apple/examples/run-catalyst.sh
//! ```

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod demo {
    use nsh_core::prelude::*;
    use nsh_ext::LoadState;

    #[cfg(target_os = "ios")]
    use nsh_platform_ios as platform;
    #[cfg(target_os = "macos")]
    use nsh_platform_macos as platform;

    const PAGE: &str = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Cmeta%20name=viewport%20content=%22width=device-width,%20initial-scale=1%22%3E%3Cstyle%3Ebody%7Bfont:16px%20-apple-system,sans-serif;margin:2rem;%7D%3C/style%3E%3Ch1%3Ensh-ext%20WKWebView%3C/h1%3E%3Cp%3EloadProgress%20comes%20from%20WKWebView.estimatedProgress.%3C/p%3E";

    fn observe(progress: &nsh_ext::LoadProgress) {
        match progress.state {
            LoadState::Loading => println!(
                "loadProgress: navigation={} state=loading progress={:?}",
                progress.navigation,
                progress.progress.map(|value| value.0)
            ),
            LoadState::Failed => println!(
                "loadProgress: navigation={} state=failed message={:?}",
                progress.navigation, progress.message
            ),
            state => println!(
                "loadProgress: navigation={} state={state:?}",
                progress.navigation
            ),
        }
    }

    #[component]
    pub fn App() -> impl IntoElement {
        view().flex_1().child(
            nsh_ext::web_view()
                .src(PAGE.to_owned())
                .on_load_progress(observe)
                .flex_1(),
        )
    }

    pub fn run() {
        platform::set_components(nsh_ext_apple::register_components);
        #[cfg(target_os = "macos")]
        let options = platform::AppOptions {
            title: "nsh-ext-apple progress".to_owned(),
            app_id: "dev.nsh.ext.apple.progress".to_owned(),
            size: (900, 640),
            ..Default::default()
        };
        #[cfg(target_os = "ios")]
        let options = platform::AppOptions::default();

        #[cfg(target_os = "macos")]
        let _ = platform::run_app(
            platform::AppSource::Rust(platform::Root::of::<App>()),
            options,
        );
        #[cfg(target_os = "ios")]
        platform::run_app(
            platform::AppSource::Rust(platform::Root::of::<App>()),
            options,
        );
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn main() {
    demo::run();
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn main() {
    eprintln!("The interactive demo runs on iOS, Mac Catalyst or macOS");
}
