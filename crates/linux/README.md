# nsh-ext-linux

The Linux adapter for [`nsh-ext`](../../README.md): an extension-owned WebView
for Native Shell apps on GTK 4 and WebKitGTK 6.0.

The component registers as `nsh-ext.webview` and creates its own
`WebKitWebView`. It never replaces NSH's built-in WebView, its network session
or its signal handlers, and it needs no NSH source changes; it is built only on
`nsh-platform-linux`'s public native-component API (`NativeComponent`,
`Components`, `ComponentContext`) from NSH 0.6.0. It links WebKitGTK itself,
so NSH does not need its `webview` feature, and its built-in WebView can
coexist with this one.

## Requirements

- Rust 1.97 or newer (edition 2024).
- GTK 4 and WebKitGTK 6.0 at build and run time: for example
  `libwebkitgtk-6.0-dev` (Debian/Ubuntu), `webkitgtk6.0-devel` (Fedora), or
  `webkitgtk-6.0` (Arch). The adapter needs the GTK main thread and a display.
- An NSH app on Linux, with `nsh-platform-linux` 0.6.0.
- Hosts enabling `nsh-platform-linux/sound` also need GStreamer core/base
  development packages (`libgstreamer1.0-dev` and
  `libgstreamer-plugins-base1.0-dev` on Debian/Ubuntu).

This package is its own Cargo package and lock graph, so the Linux GTK 4 /
WebKitGTK 6.0 stack never enters a macOS, Windows, Android or iOS build.

## Register

Register the adapter before the app's elements are created. From Rust:

```rust
use nsh_platform_linux::{AppOptions, AppSource, Root, run_app};

fn main() -> nsh_platform_linux::gtk4::glib::ExitCode {
    let options = AppOptions {
        title: "My app".to_owned(),
        app_id: "com.example.myapp".to_owned(),
        ..Default::default()
    };
    nsh_ext_linux::register(&options.components);
    run_app(AppSource::Rust(Root::of::<App>()), options)
}
```

From an NSH app whose native components are generated, call
`nsh_ext_linux::register` for the `Components` registry the generated
`#[native_components(linux)]` function receives. `register` has the signature
`set_components` takes, so a host that installs components process-wide can use
it directly:

```rust
nsh_platform_linux::set_components(nsh_ext_linux::register);
```

## Use

Rust:

```rust
use nsh_ext::web_view;
use nsh_core::prelude::*;

view().flex_1().child(
    web_view()
        .src("https://example.com")
        .on_load_progress(|progress| {
            println!(
                "{} {:?} {:?}",
                progress.navigation, progress.state, progress.progress
            );
        })
        .flex_1(),
)
```

TSX with `nativeComponent` uses the same wire:

```tsx
import { nativeComponent } from "nsh";

type Progress = {
  navigation: number;
  state: "started" | "loading" | "finished" | "failed" | "cancelled";
  progress: number | null;
  message: string | null;
};
const Browser = nativeComponent<
  { src: string; trackProgress: boolean },
  { loadProgress: Progress }
>("nsh-ext.webview", {
  props: ["src", "trackProgress"],
  events: ["loadProgress"],
});

// Enable tracking when listening for the native event.
<Browser src="https://example.com" trackProgress onLoadProgress={console.log} />;
```

Props and events are the shared contract in
[`nsh-ext`](../../src/lib.rs):

| Prop | Type | Meaning |
| --- | --- | --- |
| `src` | `string` | URL to show. An empty string, or removing the prop, stops the load and clears the page. |
| `trackProgress` | `boolean` (default `false`) | Report `loadProgress` events; the Rust wrapper sets it when `on_load_progress` is used. |

`loadProgress` payload:

```json
{
  "navigation": 2,
  "state": "loading",
  "progress": 0.42,
  "message": null
}
```

`state` is one of `started`, `loading`, `finished`, `failed` or `cancelled`.
`progress` is WebKit's `estimated-load-progress` when it has one, else `null`;
an estimate of one is still an estimate and does not mean success, which only
`finished` means. `message` is the diagnostic of a `failed` navigation:
WebKit's error text, or the end of its web process.

## Lifecycle and native guards

- A source change loads on the next main-loop iteration, after NSH's whole
  props batch, so `trackProgress` is already known when the first navigation
  starts. A non-empty source for an unmapped view waits until it is mapped:
  WebKitGTK can leave a view loaded before mapping blank. Clearing `src` with
  an empty string or by removing the prop stops the engine's load and clears
  the page on the next main-loop iteration, whether or not it is mapped.
- Replacing `src`, setting it empty, or disposing the element stops the
  engine's load. A replaced navigation reports `cancelled`; its engine
  cancellation never surfaces as `failed`.
- Explicit source changes retire the old native browser inside a stable GTK
  container before the next one is created. Old signal handlers are disconnected,
  and callbacks are checked against that browser generation. No old URL list or
  assumed stop-completion callback is used to identify a new load.
- This deliberately trades cross-source page history/session state and browser
  warm-up for exact source-replacement isolation. Page-originated navigations
  stay in their existing browser. Website-data behavior is WebKit's; this is
  not an incognito or shared-session API.
- Clearing removes the browser even while the component is hidden; a new
  non-empty source waits until the stable container is mapped. The retired
  page cannot continue executing behind an empty source.
- No timers, simulated percentages, page scripts or invented completion:
  estimates come only from WebKit, and `finished` only from WebKit's
  `finished` load event after a load that did not fail. A failed load and a
  terminated web process both report `failed` with WebKit's diagnostic; the
  engine's own error page is suppressed so the app's layout stands.
- The component is trusted host code. NSH's URL grants do not sandbox it; the
  embedding app decides which URLs it may load and whether untrusted code may
  use the component. Cookies and screenshots are not part of this release.

## Run the interactive example

```console
cargo run --manifest-path crates/linux/Cargo.toml --example progress -- https://example.com
```

The header reports each native lifecycle event and WebKit's estimate.

## Native acceptance harness

`examples/acceptance.rs` registers the adapter in a real NSH app, starts a
local HTTP server and drives real GTK/WebKit widgets through the lifecycle.
It is the acceptance check for this adapter:

From the repository root:

```console
cargo run --manifest-path crates/linux/Cargo.toml --example acceptance
cargo run --manifest-path crates/linux/Cargo.toml --example acceptance -- success
```

or, from `crates/linux`:

```console
cargo run --example acceptance              # every scenario, each in a child process
cargo run --example acceptance -- success   # one scenario
cargo run --example acceptance -- slow
cargo run --example acceptance -- failure
cargo run --example acceptance -- replacement
cargo run --example acceptance -- replacement-failure
cargo run --example acceptance -- hidden-clear
cargo run --example acceptance -- disposal
```

It exits `0` only when every selected scenario's assertions hold, `1` on a
failed assertion, and `2` when it cannot start (bad arguments or no display).
Without a session display, use `xvfb-run -a cargo run --example acceptance`.

Assertions, made from the real `on_load_progress` callbacks:

- `started` and `cancelled` carry `progress: null` (an unavailable estimate is
  never reported as zero); `finished` is confirmed success with `1.0`.
- `slow` streams a known-length body in chunks and requires WebKit's native
  estimate strictly between zero and one while loading.
- `failure` loads a just-closed port (a connection refusal, not an HTTP error
  status) and requires `failed` with WebKit's message and no `finished`.
- `replacement` changes `src` mid-load and requires `cancelled` for the
  replaced navigation and `finished` for the new one.
- `replacement-failure` replaces a load with a refused connection and requires
  `cancelled` for the replaced navigation, then a genuine `failed` for the
  replacement: retirement of the old browser must not swallow the new failure.
- `hidden-clear` unmaps the component's stable GTK root mid-load, clears `src`
  while it is unmapped, then assigns a new source while hidden and remaps. The
  hidden navigation is `cancelled` and never finishes or fails; the new source
  waits for the map and finishes.
- `disposal` removes the element mid-load and requires that no terminal event
  arrives.

## License

Apache-2.0, like the repository. The adapter's WebKitGTK lifecycle handling
follows the conventions of NSH's own Linux WebView (Apache-2.0) while using
only NSH's public API and its own browser view.
