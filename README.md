# nsh-ext

Opt-in native WebView extensions for [Native Shell](https://nsh.dev), maintained independently by Honsun Zhu. Applications register an extension-owned native component through NSH's public embedding APIs; the upstream NSH crates remain unchanged.

## Package boundary

`nsh-ext` contains the common Rust element and loading-event contract. Host adapters live in separate packages so applications choose their native dependencies. In particular, native Linux uses GTK 4 / WebKitGTK 6, while GPUI on Linux uses GTK 3 / WebKitGTK 4.1; these are separate dependency graphs, not an enable-everything feature.

Package versions start at **0.6.0**, matching the released NSH **0.6.0** API. Keep the application,
its host and its adapter on the same NSH version. Register the platform adapter
before the application starts loading its elements.

| Extension version | Native Shell | GPUI Fast baseline |
| --- | --- | --- |
| 0.6.0 | 0.6.0 | 0.1.16 |

The extension version follows the Native Shell version it targets. The shared
Rust event, element and registration types belong to that NSH dependency graph.

This library is scoped to loading progress. Page capture and Cookie management
remain separate work.

| Host | Adapter | Registration |
| --- | --- | --- |
| Linux, GTK 4 / WebKitGTK 6 | [`nsh-ext-linux`](crates/linux/) | `register(&Components)` |
| iOS / Mac Catalyst / macOS | [`nsh-ext-apple`](crates/apple/) | `register(&AppView)` or `register_components(&Components)` |
| Windows / WebView2 | [`nsh-ext-windows`](crates/windows/) | `register(&Components)` |
| GPUI on Linux / macOS / Windows | [`nsh-ext-gpui`](crates/gpui/) | `register(&Components)` or `install(AppOptions)` |
| Android | [Java companion library](android/) | `WebViewExtension.register(NshView)` |

The Android companion is built as an AAR against the unmodified released NSH
SDK. It uses the same native-component protocol as the Rust element. Source
changes on every adapter retire the owned browser instance;
this provides exact callback isolation but discards cross-source tab history
and page state and incurs browser warm-up. Page-originated navigation remains
inside the current browser. See each adapter's contract before choosing it.

## Rust element

Use this inside a Rust NSH component after the host has registered its adapter:

```rust
use nsh_core::prelude::*;
use nsh_ext::{LoadState, web_view};

let status = signal(String::from("Waiting"));
let page = web_view()
    .src("https://example.com")
    .on_load_progress(move |event| {
        let label = match (event.state, event.progress) {
            (LoadState::Finished, _) => "Loaded".to_owned(),
            (LoadState::Failed, _) => event.message.clone().unwrap_or_else(|| "Load failed".into()),
            (LoadState::Cancelled, _) => "Cancelled".to_owned(),
            (_, Some(fraction)) => format!("Loading {:.0}%", fraction.0 * 100.0),
            _ => "Loading…".to_owned(),
        };
        status.set(label);
    })
    .flex_1();

let content = view().child(text(status)).child(page);
```

`src` also accepts a signal, memo or closure returning a string. An empty source stops and clears the page. Ordinary NSH layout, style and element-ref builders apply to the extension element.

This is a separate native component, named `nsh-ext.webview`, not a mutation of NSH's built-in `WebView`. Its source and progress API are deliberately smaller than the built-in component's API. Register and use the component only where the matching host adapter is present.

## Loading semantics

Each event carries:

- `navigation`: a positive per-view navigation identity, exactly representable by JavaScript;
- `state`: `started`, `loading`, `finished`, `failed` or `cancelled`;
- `progress`: the native estimated fraction in `0..=1`, or `null` when unknown;
- `message`: an optional diagnostic for a failure.

A number is an estimate, not a portable network-byte percentage. An estimate of `1` in a `loading` event does not establish success: only `state: "finished"` does. Engines without an estimate keep `progress` unknown during loading. No timer or resource-count heuristic fills in invented values.

A new navigation gets a new identity. Late replies must be fenced by the adapter's native navigation/view lifetime; simply labeling an old callback with the current counter is not sufficient. Duplicate estimates are suppressed, but an engine's changing estimate is not artificially made monotonic. Failure and cancellation terminate that navigation without a later successful completion.

## Host and TSX integration

The public native-component transport uses:

- component name: `nsh-ext.webview`;
- `src`: URL string;
- `trackProgress`: boolean, default `false`;
- native event: `loadProgress`, with the event object described above.

The Rust `.on_load_progress(...)` builder enables `trackProgress` automatically. A TSX application using `nativeComponent` declares the same props/event and enables tracking when it listens. The host supplies the native implementation; dynamically delivered TSX cannot install a Rust or Java extension itself.

Native components are **trusted host code**. NSH's built-in WebView URL grants and storage policy are not automatically applied to an application-registered component. Hosts must authorize URLs and registration when embedding untrusted modules. Platform website-data behavior belongs to the adapter/host; the common loading API makes no cross-platform Cookie-store or incognito guarantee.

## Development

The root package and each adapter are separate Cargo packages. Run commands with the appropriate manifest rather than combining GTK 3 and GTK 4 into one build graph. A native adapter requires its operating system's SDK and browser engine; compilation of an empty off-target crate is not native verification.

The root lifecycle and wire-boundary regressions run with:

```sh
cargo test --manifest-path Cargo.toml --test progress
```

Native acceptance must exercise real slow/successful/failed/replaced loads, multiple views and disposal, in addition to checking callback decoding. Platform examples and their commands belong beside the corresponding adapter.

## License

Apache-2.0. See [LICENSE-APACHE](LICENSE-APACHE). Native Shell and its upstream contributors retain their respective copyrights; this project is an independent extension library.
