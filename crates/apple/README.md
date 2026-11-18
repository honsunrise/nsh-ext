# nsh-ext-apple

The iOS, Mac Catalyst and macOS adapter for `nsh-ext`'s WebView component. It
registers `nsh-ext.webview` and owns its own browser; it does not patch or
replace Native Shell's built-in web view, web client or delegates. The
component is a stable native container whose `WKWebView` is replaced on every
explicit `src` change.

The package is a separate Cargo package with its own lock graph and is not a
member of the root crate's workspace. It is built only for Apple targets:

| Target | NSH platform crate | Web view |
| --- | --- | --- |
| `aarch64-apple-ios`, `*-apple-ios-sim` | `nsh-platform-ios` | `WKWebView` |
| `aarch64-apple-ios-macabi` (Catalyst) | `nsh-platform-ios` | `WKWebView` |
| `aarch64-apple-darwin`, `x86_64-apple-darwin` | `nsh-platform-macos` | `WKWebView` |

The NSH platform dependencies use `default-features = false`: this is a
Rust-only adapter, so NSH's default JS/editor/shader feature sets are not
pulled in. They are pinned to the released NSH **0.6.0** (`nsh-platform-ios`
/ `nsh-platform-macos` `=0.6.0`): keep the application, its host and this
adapter on the same NSH version.

## Register it

Register the component before the app loads. With an `AppView` the host owns:

```rust,ignore
nsh_ext_apple::register(&app);
```

With a host that uses the platform's process-wide component hook:

```rust,ignore
nsh_platform_macos::set_components(nsh_ext_apple::register_components);
// iOS and Catalyst:
// nsh_platform_ios::set_components(nsh_ext_apple::register_components);
```

`nsh_ext_apple::factory()` is available when the host registers explicitly:

```rust,ignore
app.register_component(nsh_ext::COMPONENT_NAME, nsh_ext_apple::factory());
```

Then use the shared Rust wrapper:

```rust,ignore
let page = nsh_ext::web_view()
    .src("https://example.com".to_owned())
    .on_load_progress(|progress| {
        println!("{} {:?} {:?}", progress.navigation, progress.state, progress.progress);
    });
```

or the same name from TSX with `nativeComponent("nsh-ext.webview", …)`. NSH's
normal `Element` builders style the component.

## Wire contract

| Prop | Type | Meaning |
| --- | --- | --- |
| `src` | string | URL to load. Setting a different value cancels the load in flight (reported as `cancelled`), destroys the current browser and page (history, timers and workers included), and loads the new source in a fresh `WKWebView` inside the same container. An empty string or removing the prop destroys the page the same way and reports no blank navigation. |
| `trackProgress` | bool, default `false` | Gates construction and emission of `loadProgress` events. |

`loadProgress` payload:

```json
{
  "navigation": 3,
  "state": "started | loading | finished | failed | cancelled",
  "progress": 0.42,
  "message": null
}
```

* `navigation` is per component and positive. It identifies the WebKit
  navigation, not just the latest counter. The counter continues across `src`
  changes; a component that is removed and added again starts again at one.
* `progress` is WebKit's own `WKWebView.estimatedProgress`, in `0..=1`, or
  `null` when WebKit has no estimate. It is never invented, timed or estimated
  from page script.
* An estimate of `1` is still `loading`. `finished` means
  `webView:didFinishNavigation:` confirmed success.
* A genuine `NSError` reports `failed` with its localized description. A
  navigation WebKit cancels (`NSURLErrorCancelled`) or the host stops or
  replaces reports `cancelled`.
* An HTTP error status is not by itself a failed navigation: WebKit still
  reports `didFinish` for an error document (a 404 or 500 page), and this
  adapter follows that. Only native load failures (connection refusal,
  TLS/DNS errors, cancelled loads) report `failed`.

The initial `src` is applied on the next main-loop turn, so a props batch that
sets `src` before `trackProgress` still enables tracking for that first load.
`trackProgress` is only an emission gate: navigation identities advance even
while it is off, and enabling it halfway through a native load reports the
next native estimate or completion without inventing a `started` event.

Setting a different non-empty `src` is a browser replacement, not a
navigation inside the old page. The adapter first reports the old load as
`cancelled`, then destroys that `WKWebView` and its page (so the old page's
timers, workers and back/forward history are gone) and loads the new source in
a new `WKWebView` inside the same stable container. This is intentional: it is
the only way a late callback from the discarded page can be rejected by
construction, and it keeps a replaced navigation from taking over the new one.
The cost is that a back/forward history and any page state do not survive an
`src` change; a page-originated navigation (a link or `location.href`) stays in
the current browser and keeps its history.

An empty or removed `src` cancels the active load (if one is in flight),
destroys the page the same way, and reports nothing for the destruction: no
blank navigation is started, and after the `cancelled` event (when there was an
active load) the component emits no further event for that page.

## Examples and verification

Requirements: Rust 1.97 or newer, Xcode and the Apple targets.

```sh
rustup target add aarch64-apple-ios-sim aarch64-apple-ios-macabi
```

### Acceptance harness (real WebKit, pass/fail)

```sh
cargo run --manifest-path crates/apple/Cargo.toml --example acceptance
crates/apple/examples/run-ios-simulator.sh acceptance
crates/apple/examples/run-catalyst.sh acceptance
```

The harness exercises six real components through Native Shell: a page that
must finish after a genuine intermediate estimate; a connection to a freshly
freed loopback port that WebKit must refuse (a native failure, not an HTTP
status); a navigation replaced while its TLS connection hangs (old identity
cancelled, new identity finished); a source revisit through a page, an HTTP 500
error document that must still finish, the page again, and a refused
connection that must fail, with identities continuing across browser
replacements; a page whose timer and worker request the local fixture server
and then an empty `src`, which must cancel the active load, stop both counters,
and start no blank navigation; and a component removed mid-load (silent
teardown) then added again, whose new identity must restart at one. It checks
identities, estimates, failure diagnostics and phase ordering, then exits `0`
only when all six assertions pass. Any violation, timeout or early close exits
nonzero. The scripts wait for the app and exit nonzero unless the PASS line is
present.

### Interactive demo

```sh
cargo run --manifest-path crates/apple/Cargo.toml --example progress
```

The same `progress` example is what the iOS Simulator and Catalyst scripts
build. It stays open and prints the native lifecycle.

### iOS Simulator

```sh
crates/apple/examples/run-ios-simulator.sh
# Pick a booted/available device:
SIMULATOR="iPhone 16 Pro" crates/apple/examples/run-ios-simulator.sh
```

The script builds the requested example (default `progress`) for the simulator,
creates and ad-hoc signs a `.app`, boots the simulator, installs it and runs
it. With `acceptance` it attaches to the console, verifies the PASS line and
exits nonzero otherwise. To read a demo's stdout:

```sh
xcrun simctl launch --console "iPhone 16" dev.nsh.ext.apple.example
```

### Mac Catalyst

```sh
crates/apple/examples/run-catalyst.sh
```

The script builds the requested example for `aarch64-apple-ios-macabi` (or
x86_64 on Intel), creates a Catalyst `.app` bundle, signs it ad hoc and runs
it. With `acceptance` it waits for exit, prints the log and exits nonzero
unless the PASS line is present.

Both scripts honour `CARGO_TARGET_DIR` and target the example in this package,
not the root crate.

### Unit tests

The navigation state machine is covered without WebKit:

```sh
cargo test --manifest-path crates/apple/Cargo.toml
```

These tests exercise replacement, stale callbacks, cancellation, the native
binding being cleared on a source change while the component's identities
continue, `trackProgress` defaults, identities advancing while it is off,
mid-load enablement without a synthesized start, failure, and process
termination.

## Implementation notes

* The component's own `NSView`/`UIView` is the stable view NSH lays out. Every
  applied `src` change builds a fresh `WKWebView` as its child, sized to the
  container and configured with flexible width/height so a later resize reaches
  the page. `src` and `trackProgress` are still applied in prop-batch order:
  the first non-empty `src` is deferred one main-loop turn, so an initial load
  sees the `trackProgress` that arrives after it in the same batch.
* Each browser has its own callback generation. A callback arriving from a
  discarded browser (`WKNavigationDelegate`, `estimatedProgress` KVO, or
  content-process termination) finds a stale generation and changes nothing;
  the component also checks the web view that produced it. This is why the
  design does not need to retain an ever-growing list of old navigations.
* Within one browser, every navigation is keyed by the `WKNavigation` object
  address. A callback carrying a stale navigation changes nothing, and
  `didFinish`/`didFail` never finish a different navigation. `loadRequest:`
  returned identities are retained only while a late start is possible, so a
  replaced navigation's start cannot be mistaken for a new one, while
  page/user-initiated navigations remain accepted. A terminal callback or the
  browser's destruction releases those identities.
* `estimatedProgress` is observed with KVO. The observer checks that the
  observed object is this browser's `WKWebView` and that the generation is
  current; it cannot report for another view or for a discarded browser.
* The delegate object is per browser. WebKit's navigation delegate is weak, so
  the browser retains the handler; the handler retains its browser state (and a
  weak reference to the component), so no retain cycle is formed.
* On drop the adapter cancels queued source applications, disables delivery
  and releases the current browser, whose drop removes the KVO observer, stops
  loading, clears the delegate and removes it from the stable container. The
  component's page is destroyed; disposal is silent (no cancellation or
  terminal event). WebKit is loaded with `dlopen` on first use, so apps
  without this component do not link or pay for it at launch.
* An empty source destroys the browser instead of navigating it to a blank
  document, so no blank navigation exists to report and the old page's timers,
  workers and history go with it. A queued initial load is cancelled when an
  empty source is applied immediately.
* Native components are trusted host code. This adapter does not apply Native
  Shell's URL grants to the pages it loads; the embedding application decides
  which URLs are allowed.

Loading-progress only: screenshots and cookie access are not part of this
release.

## Attribution

The dynamic WebKit loading, KVO observation and navigation-delegate shape
adapt Native Shell's Apache-2.0 WebKit host
(<https://github.com/longbridge/nsh>). The extension owns its own view,
delegate, navigation identities and progress contract.

Licensed under Apache-2.0.
