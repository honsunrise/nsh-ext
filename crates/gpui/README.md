# nsh-ext-gpui

The [`nsh-ext`](../..) WebView for Native Shell's **GPUI** backend, as a
`NativeComponent`: `nsh-ext.webview` (the name `nsh_ext::web_view()` and a TSX
`nativeComponent` send) renders an owned wry web view filling its Native Shell
container.

It implements the published `NativeComponent` contract of
`nsh-platform-gpui =0.6.0` and builds its own native view with GPUI Fast's
public window-composition API. It does not replace, patch or reuse the backend's
own WebView, and it needs no fork of Native Shell.

| OS | Engine | Native child |
| --- | --- | --- |
| macOS | WKWebView | a child view of GPUI's window |
| Windows | WebView2 | a child window of GPUI's window |
| Linux | WebKitGTK 4.1 on GTK 3 | an X11 child window, hosted by a GPUI Fast composition surface when one can be made |

This package is a workspace of its own (`[workspace]` in `Cargo.toml`): its lock
graph, and its GTK 3 / WebKitGTK 4.1 dependencies, never mix with another
adapter's. Linux needs an X11 (or Xwayland) window: WebKitGTK does not embed
into Wayland surfaces.

The manifest fixes GPUI Fast to NSH 0.6.0's published `0.1.16` baseline, the
whole family exactly (`gpui-fast`, `gpui-fast-platform`, `gpui-fast-macros`).
An application embedding the adapter resolves that same family, as it must with
the backend itself: one GPUI per application, `cargo tree -i gpui-fast` showing
one version.

## Use

```rust
use nsh_core::prelude::*;
use nsh_platform_gpui::{AppOptions, AppSource, Root, run_app};

// The app's root component (what the app's own code declares).
struct App;

impl Component for App {
    type Props = ();
    type Event = NoEvent;

    fn view(_: (), _: Scope<NoEvent>) -> impl IntoElement {
        nsh_ext::web_view()
            .src("https://example.com/")
            .on_load_progress(|progress| {
                println!(
                    "{} {:?} {:?}",
                    progress.navigation, progress.state, progress.progress
                );
            })
            .size_full()
    }
}

let mut options = AppOptions::default();
options.title = "My app".to_owned();
let options = nsh_ext_gpui::install(options);
run_app(AppSource::Rust(Root::of::<App>()), options);
```

`install` registers the component in `AppOptions::components`; register before
loading the app. With an existing registry:

```rust
nsh_ext_gpui::register(&options.components);
```

The app uses the shared API from the root crate:

```rust
nsh_ext::web_view()
    .src("https://example.com/")
    .on_load_progress(|progress| { /* ... */ })
```

In TSX the same wire is a `nativeComponent` named `nsh-ext.webview` with props
`src` (string; empty removes the page) and `trackProgress` (boolean, default
false), and an event `loadProgress` carrying `{ navigation, state, progress,
message }`.

## Loading progress

`on_load_progress` receives one `LoadProgress` per change, with a per-view
navigation identity that starts at one.

| OS | Estimate | Start | Native success | Native failure |
| --- | --- | --- | --- | --- |
| macOS | WKWebView `estimatedProgress` (KVO) | `didStartProvisionalNavigation` | `didFinishNavigation` | `didFailProvisionalNavigation` / `didFailNavigation` (a cancellation is a stop) |
| Linux | WebKitGTK `estimated-load-progress` | `load-changed` (Started) | `load-changed` (Finished) | `load-failed` (an interruption is a stop) |
| Windows | none: WebView2 has no fraction | `NavigationStarting` | `NavigationCompleted` with `IsSuccess` | `NavigationCompleted` with `IsSuccess` false |

On macOS the extension owns the web view's `WKNavigationDelegate` through a
proxy that forwards what wry's delegate implements (navigation and response
policy, download hand-offs, commit-time script injection) and adds the three
lifecycle callbacks. Success and failure are matched against the `WKNavigation`
the engine names, so a late callback cannot finish a replacement. On Windows the
matching identity is WebView2's navigation id. WebKitGTK has no navigation id;
its signals are ordered, and `load-failed` ends a load before that load's
`Finished`, so an ended load is never finished later.

Windows reports no numeric estimate while loading: `progress` stays absent
(`null` on the wire), never zero. All platforms report `Some(1)` on confirmed
success. An estimate of one is not success: `Finished` comes only from the
engine's completion callback, and a later `Finished`/`Failed` for a navigation
that ended is dropped. An explicit source change retires the owned browser,
cancels its active navigation and creates a fresh browser for the new source.
A cleared `src` removes the browser without starting a blank-page navigation.
This isolates old callbacks but discards cross-source history and page state.
A request the engine never accepted (an invalid source, a web view that could
not be made) starts and fails under one navigation identity.

## Examples

Both examples are real Native Shell apps on the GPUI backend.

Interactive demo (any URL as its argument; with none it serves its own page
from a loopback HTTP server, so it needs no network):

```sh
cargo run --example progress -- https://example.com/
RUST_LOG=info cargo run --example progress
```

The header line reports the engine's own `loadProgress` as it arrives: the
navigation identity, the state, the engine's estimate when it has one, and the
engine's message for a failure.

Native acceptance harness. It is a script over the public wire (a local page
with a pending image replaced mid-load by a fast page, a streamed page to
completion, a revisit of
the fast page, a cleared `src`, a refused connection, the fast page again, then
the web view removed from the tree and added again with a new source). It
advances on native events, with a quiet interval after clearing/removal to
detect stale callbacks. It asserts the `loadProgress` contract and **exits
non-zero on any violation**; every started navigation ends exactly once, the
run reports exactly one failure, and a replaced load is cancelled.

```sh
cargo run --example acceptance
echo $?          # 0: contract held, 1: violations printed
RUST_LOG=debug cargo run --example acceptance
```

On Windows the refused connection must be reported as `failed`: wry's generic
page-load event cannot tell a failure from a success, so that event is the
proof that `NavigationCompleted`'s native `IsSuccess` was read. On macOS the
harness also requires the failure message to carry the `NSError` domain and
code, which only the `WKNavigationDelegate` failure callback produces — a
failure inferred from `loading`/`estimatedProgress` cannot satisfy it. On macOS
and Linux the harness requires at least one real native estimate; on Windows it
requires no numeric estimates during loading and confirmed `1` on success.

Platform prerequisites:

- macOS: a GUI session (AppKit); no extra packages.
- Windows: the WebView2 runtime (present with Edge); run from a terminal.
- Linux: GTK 3 and WebKitGTK 4.1 development/runtime packages, an X11 or
  Xwayland display (`unset WAYLAND_DISPLAY` when needed). If WebKit's DMA-BUF
  renderer cannot allocate buffers on Xwayland (pages stay black), run with
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`.

## Native child behaviour

- The web view is built on the first frame after the `src` prop arrives and is
  navigated once its native callbacks exist, so the first navigation cannot
  outrun them.
- It is hidden until placed, then follows its container's bounds (GPUI points)
  and the window's scale factor; it is hidden when the container is not drawn,
  and dropped with the component (its native subscriptions removed first).
- On macOS and Windows it draws above everything GPUI draws, as the backend's
  own web view does. On Linux a composition surface keeps GPUI's overlays above
  it and clips it to the window; without one the plain child window is not
  clipped by its ancestors.
- Focus, hit-testing and scrolling are the engine's own native behaviour; the
  component's element is a transparent box that Native Shell blocks mouse input
  under, as it does for every native component.
- Native components are trusted host code. The embedding application decides
  which URLs its page may reach; this adapter does not grant or filter
  navigation.

Screenshot and cookie APIs are not part of the wire and are not implemented
here.

## Attribution

The GTK/WebKitGTK embedding, the X11 parent shim and the composition-surface
teardown follow Native Shell's `nsh-platform-gpui` web view (Apache-2.0,
Copyright Longbridge), adapted to this component contract. See the root
`LICENSE-APACHE`.

## License

Apache-2.0.
