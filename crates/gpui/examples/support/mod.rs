//! The examples exercise the adapter as an embedded NSH view in a GPUI window.

use std::time::Duration;

use nsh_platform_gpui::{
    App, AppOptions, AppView, Root,
    gpui::{
        AnyWindowHandle, AppContext, Bounds, Pixels, Size, TitlebarOptions, WindowBounds,
        WindowOptions,
    },
};

pub(crate) fn run(root: Root, title: &'static str, size: Size<Pixels>, scripted: bool) {
    gpui_platform::application().run(move |cx| {
        let bounds = Bounds::centered(None, size, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            inactive_frame_interval: None,
            ..Default::default()
        };
        let handle: AnyWindowHandle = cx
            .open_window(options, move |window, cx| {
                cx.new(|cx| {
                    let options = nsh_ext_gpui::install(AppOptions::default());
                    let view = AppView::new(options, window, cx);
                    view.load(App::Rust(root));
                    view
                })
            })
            .expect("the GPUI example window opens")
            .into();
        if scripted && cfg!(any(target_os = "macos", windows)) {
            // Desktop runners may not have a display link for this window.
            // Exercise GPUI's real prepaint, draw and native placement without
            // depending on screen presentation or changing the adapter.
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    if handle
                        .update(cx, |_, window, cx| window.draw(cx).clear(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
