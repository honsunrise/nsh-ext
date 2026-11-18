//! Runs a real NSH window with the extension's WebView2 component.
//!
//! `cargo run --example progress`
//!
//! The buttons change the reactive source, including clearing it.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
mod app {
    use nsh_core::{Color, prelude::*};
    use nsh_ext::LoadState;

    const HOME: &str = "https://example.com";

    fn action(label: &'static str, press: impl Fn() + 'static) -> impl IntoElement {
        view()
            .px_3()
            .py_1()
            .rounded(px(6.0))
            .bg(Color(0xeeee_eeff))
            .active(|state| state.opacity(0.6))
            .on_press(press)
            .child(text(label))
    }

    #[component]
    pub fn Demo() -> impl IntoElement {
        let src = signal(HOME.to_owned());
        let status = signal("no navigation yet".to_owned());
        let shown = status;
        let page = nsh_ext::web_view()
            .src(src)
            .on_load_progress(move |load| {
                let detail = match load.state {
                    LoadState::Failed => {
                        load.message.clone().unwrap_or_else(|| "failed".to_owned())
                    }
                    state => format!("{state:?}"),
                };
                let fraction = load
                    .progress
                    .map(|value| format!(" · {:.0}%", value.0 * 100.0))
                    .unwrap_or_default();
                shown.set(format!(
                    "navigation {} · {detail}{fraction}",
                    load.navigation
                ));
            })
            .flex_1();
        let home = move || src.set(HOME.to_owned());
        let rust = move || src.set("https://www.rust-lang.org".to_owned());
        let clear = move || src.set(String::new());
        view()
            .flex_1()
            .p_4()
            .gap_3()
            .child(
                view()
                    .flex_row()
                    .gap_2()
                    .child(action("example.com", home))
                    .child(action("rust-lang.org", rust))
                    .child(action("Clear", clear))
                    .child(text(move || status.get())),
            )
            .child(page)
    }
}

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    use nsh_platform_windows::{AppOptions, AppSource, Root, run_app};

    let options = AppOptions {
        title: "nsh-ext WebView2".to_owned(),
        app_id: "dev.nsh.ext.windows.example".to_owned(),
        size: (960, 720),
        ..Default::default()
    };
    nsh_ext_windows::register(&options.components);
    run_app(AppSource::Rust(Root::of::<app::Demo>()), options)
}

#[cfg(not(target_os = "windows"))]
fn main() {}
