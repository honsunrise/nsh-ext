# nsh-ext-windows

The Windows adapter for [`nsh-ext`](../..): an extension-owned WebView2
rendered as the native component `nsh-ext.webview`.

It uses NSH's public Windows `NativeComponent`/`window()` hosting and owns
its own WebView2 controller. It does not register over NSH's built-in web
view, reuse its private clients or delegates, or rely on NSH's URL grants:
native components are trusted host code, and the embedding application
decides which sources it passes.

## Requirements

- Windows 10/11 with the WebView2 Evergreen Runtime installed.
- Rust 1.97 or later, edition 2024.
- NSH `nsh-platform-windows` `=0.7.0` (a published release, not a fork);
  the application, its host and this adapter share that NSH version.
- `webview2-com-sys` links `WebView2LoaderStatic` on MSVC; GNU targets
  need `WebView2Loader.dll` beside the executable.

This package has its own `Cargo.toml`/lock graph, separate from the root
`nsh-ext` package, so Windows' WebView2 dependencies do not leak into the
root crate or the other adapters.

## Use

[Install both nshm](../../README.md#install-as-an-nshm) for automatic registration
in an NSH-generated Windows host. To register directly in an embedding host:

```rust
use nsh_platform_windows::{AppOptions, AppSource, Root, run_app};

let options = AppOptions {
    title: "WebView2".to_owned(),
    app_id: "com.example.webview".to_owned(),
    size: (960, 720),
    ..Default::default()
};
// Before the app loads; on the UI thread.
nsh_ext_windows::register(&options.components);
run_app(AppSource::Rust(Root::of::<MyApp>()), options)
```

Then build the element with the shared Rust API:

```rust
nsh_ext::web_view()
    .src("https://example.com")
    .on_load_progress(|progress| {
        // Started / Finished / Failed / Cancelled; no invented fractions.
        println!("{:?}", progress.state);
    })
```

TSX uses the same wire through `nativeComponent("nsh-ext.webview", {
src, trackProgress })`. Register the component factory on an existing
`AppView` with `view.register_component(nsh_ext::COMPONENT_NAME,
nsh_ext_windows::component_factory())`.

### Props

| Prop | Type | Default | Meaning |
| --- | --- | --- | --- |
| `src` | string | empty | URL to load. A changed source retires the current browser and loads in a new one. An empty string or a removed prop retires it and clears the view. |
| `trackProgress` | bool | `false` | Emits `loadProgress` events. Payload allocation and emission are skipped while false. |

### `loadProgress` events

The payload is `nsh_ext::LoadProgress`:

```text
{ navigation: positive integer, state: "started" | "loading" | "finished" | "failed" | "cancelled",
  progress: null | 0..1, message: string | null }
```

- `started` is WebView2's `NavigationStarting`; `progress` is `null`.
- `finished` is `NavigationCompleted` with `IsSuccess == true`; `progress`
  is WebView2's confirmed completion (`1.0`).
- `failed` is `IsSuccess == false` (with the WebView2 error status in
  `message`), or a controller/environment failure. An HTTP error page is
  still a completed navigation: the adapter does not treat an HTTP status
  alone as a native navigation failure.
- `cancelled` is the host retiring a browser whose navigation was still
  loading (a source change or a clear).
- `loading` is not emitted: WebView2 reports no completion percentage, so
  the adapter never invents one. A fractional estimate is not success;
  only `finished` is.

Navigation identities are per component and never reused; a component
removal and re-add starts a new component with identity 1. The native
`NavigationStarting`/`NavigationCompleted` ids are correlated within one
browser, so a late completion for a navigation that a newer one replaced
is ignored. A page-originated navigation stays in its browser and is
counted as a new navigation; it is never replayed into a replacement.

## Browser lifetime and its tradeoff

The child window is the component's stable native container, and the
WebView2 environment is shared per profile folder. The *browser* — a
controller and its core — is not stable: an explicit `src` change retires
the current one and makes a new one, and an empty source retires it without
making one. `Close` is synchronous and does not run `beforeunload`, so the
old page's document, timers, workers and open requests end with the
controller instead of a navigation whose cancellation cannot be verified.
The generation of the browser fences every asynchronous controller
creation and every native callback: a controller that finishes after its
source was replaced is closed, and an event from a retired browser changes
nothing. Only one controller creation is outstanding at a time, so two
controllers never race for the same parent window.

The cost is real: every source change is a cold browser start and the new
browser has no back/forward history, session state or warm cache from the
previous one. Re-setting the *same* source is not a change and does not
reload; a revisit after a clear (or of a different URL) is a new browser.
The alternative — keeping one browser and proving that a blank navigation
cancelled the old document — cannot be verified through WebView2's public
events, so this adapter trades history and warm-up for unambiguous page
termination. A page-originated navigation stays within the current browser
and is not replayed into a replacement.

An empty source never makes a browser and never emits a `loadProgress`
event; clearing before anything loaded is free. A source set while a
controller creation is in flight waits for that creation to end, then loads
into a new browser. A setup failure (no runtime, no window, no controller)
is reported as `started` followed by `failed` for the current source,
honestly; it does not poison later sources, so a successful fresh load
after an error is possible.

Sizing is in the child window's pixels. The controller's bounds mode is
pinned to raw pixels and its bounds are the child window's client area as
`WM_SIZE`/`GetClientRect` report it; its rasterization scale is the
window's DPI over 96. `draw` reports the component's frame in points, which
is converted to pixels with that same scale only until the window has a
client area, so a host that places the window without drawing the
component still sizes the page. Because bounds and scale share one unit
system, the page's CSS viewport is the window's pixels divided by its DPI
scale: on a 2x display the page has `devicePixelRatio` 2 and half the CSS
width of its pixels, instead of a 1x page stretched over the pixels. A
replacement controller is given the same bounds and scale as the one it
replaces.

## API

- `register(&Components)` registers `nsh-ext.webview` on an app's component
  registry.
- `component_factory()` returns the factory for `AppView::register_component`.

The component and WebView2 host are private implementation types; apps use
the shared `nsh_ext::web_view` element and the two registration functions
above. The host owns the stable child window, the shared environment, the
per-source controller and its native subscriptions. See "Browser lifetime
and its tradeoff" above for what happens on a source change, a clear and an
asynchronous controller creation.

## Examples and native verification

```powershell
cd crates/windows
cargo run --example progress
cargo run --example acceptance
```

From the repository root, the acceptance harness can also be invoked
without a workspace:

```powershell
cargo run --manifest-path crates/windows/Cargo.toml --example acceptance
```

- `examples/progress.rs` is the runnable NSH demo: a Rust root component
  with a reactive source, a progress status line, source switches and a
  Clear action.
- `examples/acceptance.rs` is the actual NSH acceptance: it registers the
  adapter on `AppOptions::components`, opens an explicit NSH desktop window,
  and fills it with `nsh_ext::web_view().on_load_progress(...)`.
  It starts with an empty source, so clearing before anything loaded must
  emit nothing and make no page. A local HTTP server provides a successful
  page, a 404 page, a slow page, a marker page whose timer, worker and open
  request report to the server, and a closed local port. The app checks a
  successful load, that clearing the source ends the old page's execution
  (every timer/worker beat and the open request stop, which in-place
  clearing would not), that the 404 is a completed document (not a
  failure), rapid clear-then-load, that re-setting an unchanged source does
  not reload while a same-URL revisit after a clear does, a loading
  navigation replaced and cancelled, a refused connection that fails
  honestly, and a window closed during a load whose disposal adds no
  cancellation or failure. Successful pages report
  `innerWidth`/`innerHeight` and `devicePixelRatio`; the check compares
  them with the WebView2 child window's client pixels and DPI scale, so it
  proves the controller fills the window at a nonzero size and rasterizes
  at the window's scale. On a 2x display that is the exact 2x proof
  (devicePixelRatio 2, CSS width half the pixel width); set
  `NSH_EXT_ACCEPTANCE_DPI=2` to make a 2x display a requirement of the
  run. Windows has no supported way to force a window to 2x, so a 2x
  display (200% scaling) is what provides that run; at any other scale the
  same unit relation is checked exactly. Every event comes from WebView2's
  native callbacks; `loading` fractions, a numeric estimate on
  `started`/`failed`, and zero used for unknown all fail the acceptance.
  The harness waits for the window to close before checking its verdict:
  exit code 0 passes, 1 fails, 2 times out.

Both require the WebView2 runtime. Run them on Windows with
`cd crates/windows`; the acceptance harness exits nonzero on any contract
assertion failure.

## Notes

- The page is a child window, so it follows NSH's "airspace" rules: it sits
  above NSH-drawn content and is clipped/moved with its view by NSH.
- The extension uses its own WebView2 profile under
  `%LOCALAPPDATA%\nsh-ext\<executable>\WebView2`, so it does not share or
  replace NSH's built-in web view profile.
- A window creation failure is reported through the first `failed`
  lifecycle event rather than a fake success.
- This crate is empty off Windows.

## Attribution

The WebView2 environment/controller setup was adapted from
`nsh-platform-windows` 0.5.1 (Apache-2.0, Longbridge), the release it was
written against; the adapter targets the published 0.7.0 API, whose public
`NativeComponent` contract is used here. The extension-owned host,
navigation correlation and progress mapping are its own.

Licensed under Apache-2.0. See `LICENSE-APACHE` in the repository root.
