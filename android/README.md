# nsh-ext on Android

The Android companion of the [`nsh-ext`](../README.md) crate. It provides the
`nsh-ext.webview` native component for Native Shell: a browser view the extension
owns, with the same loading-progress contract as the Rust wrapper
([`src/lib.rs`](../src/lib.rs)).

The host app embeds Native Shell's public Android SDK
(`com.longbridge.nsh.NshView`, `NshComponent`, `NshComponentContext`) and calls
`WebViewExtension.register(nsh)` before loading its app. There is no NSH fork, no
new JNI entry point and no patched runtime: events go through the SDK's existing
`NshComponentContext.emit`, and props through `NshComponent.setProp`.

```java
NshView nsh = new NshView(this);
WebViewExtension.register(nsh);   // registers "nsh-ext.webview" on this view
setContentView(nsh);
nsh.loadAssetBundle("nsh");
```

From Rust, the element is `nsh_ext::web_view()`; from TSX it is
`nativeComponent("nsh-ext.webview", { props: ["src", "trackProgress"], events: ["loadProgress"] })`.
[`example/App.tsx`](example/App.tsx) is a complete app: it registers the
component on its `NshView` and receives every load as `onLoadProgress` through
the ordinary native-component transport. [`fixture/App.tsx`](fixture/App.tsx)
is the minimal app the automated full-path test runs.

## Layout

| Path | What it is |
|---|---|
| `webview/` | The Android library: `com.honsunrise.nshext.WebViewExtension` (public API) and the package-private `WebViewComponent`. |
| `webview/src/androidTest/` | Instrumentation tests: a real WebView against a loopback HTTP server. |
| `example/` | An example app: `App.tsx`, `nsh.json`, and a `MainActivity` that registers the extension. |
| `fixture/` | The minimal app the full-path instrumentation builds and runs: it posts what it received over its host channel. |
| `libs/nsh.aar` | Where the host-supplied runtime goes (git-ignored). |

There is no Cargo package here. The Android adapter is Java only: the public SDK
already carries the props and events across the existing JNI boundary, so a Rust
crate would add no code, only a second lock graph. The Rust contract lives in the
repository root.

## Requirements

- The **unmodified released Native Shell 0.7.0 `nsh.aar`**. It is not downloaded
  or committed. Take the `nsh-embed-android.zip` asset of the
  [`v0.7.0` release](https://github.com/longbridge/nsh/releases/download/v0.7.0/nsh-embed-android.zip)
  and use `nsh-embed-android/nsh.aar` (its `nsh-dev.aar` is the development
  build), or `dist/nsh/embed/android/nsh.aar` from a 0.7.0 source distribution.
  Put it at `android/libs/nsh.aar`, or pass
  `-PnshAar=/absolute/path/to/nsh.aar`. Both `webview/` and `example/` fail
  configuration with that instruction when it is missing. (`nsh-android.tar.gz`
  is the CLI's runtime component — `runtime/android/.../classes.dex` and
  `libnsh.so` — not an embeddable library.)
- Android 8 (API 26) or newer, a device or emulator with a WebView provider, and
  `INTERNET` (declared by the library and by the AAR).
- JDK 17, Gradle 8.9+ and Android Gradle Plugin 8.7.3 (declared in the root
  `build.gradle`), `compileSdk 35`. No Gradle wrapper is checked in.

### Host registration

The released SDK retains the Java `NshComponent` factory and the public
`NshComponentContext` methods this adapter uses. The Java host calls
`WebViewExtension.register(NshView)` before loading the bundle, which declares
the view with `nativeComponent`. NSH can package an AAR into a standalone app,
but that does not install its Java factory on the app's `NshView`.

### Packaging

The released AAR is **not** modified, repackaged or patched:

- `webview/build.gradle` depends on it with `compileOnly`, so the library's own
  AAR does not embed a second runtime. Merging `com.longbridge.nsh` classes twice
  would break the native library's name lookups and the R8 rules the AAR ships.
- The host app's module adds the same file with `implementation(files(...))` (the
  example does), so the APK carries exactly one copy, its `libnsh.so` ABIs and the
  AAR's `proguard.txt`.
- The instrumentation tests add it with `androidTestImplementation`, because they
  run the real `NshView` and the real `libnsh.so`.

### Using the companion archive

The 0.7.0 companion archive is named `nsh-ext-android-0.7.0.zip` and targets NSH
0.7.0. Keep the runtime, app and companion on the same version. Copy
`nsh-ext-android-0.7.0.aar` and the unmodified NSH SDK `nsh.aar` into the host
application's `libs/` directory, then add both files:

```kotlin
dependencies {
    implementation(files("libs/nsh.aar"))
    implementation(files("libs/nsh-ext-android-0.7.0.aar"))
}
```

Register `WebViewExtension` on each `NshView` before loading the application.
To build the companion from source, run `gradle -p android :webview:assembleRelease
-PnshAar=/absolute/path/to/nsh.aar` from the repository root; the result is
`android/webview/build/outputs/aar/webview-release.aar`.


## The wire contract

| Item | Value |
|---|---|
| Component name | `nsh-ext.webview` (`WebViewExtension.COMPONENT_NAME`) |
| Event | `loadProgress` (`WebViewExtension.PROGRESS_EVENT`) |
| Props | `src`: string. `trackProgress`: boolean, default `false`. |

Every event payload has exactly four keys, matching `nsh_ext::LoadProgress` and
decoding through `LoadProgress::from_value`:

```json
{ "navigation": 1, "state": "loading", "progress": 0.4, "message": null }
```

| State | When | `progress` | `message` |
|---|---|---|---|
| `started` | A top-level load begins. | `null` | `null` |
| `loading` | The engine reported a new estimate. | fraction in `0..1` | `null` |
| `finished` | The engine confirmed the navigation's document finished. | `1.0` | `null` |
| `failed` | A main-frame navigation failed in the network stack. | `null` | the diagnostic |
| `cancelled` | The host replaced or stopped a load that was in flight. | `null` | `null` |

`navigation` is a positive integer, a new identity per top-level navigation of
the component, and never exceeds `2^53 - 1`. An estimate of `1.0` is a `loading`
event, not success: only `finished` is success, and only the engine's
`onPageFinished` produces it. No percentage is ever simulated, and a navigation
that failed or was cancelled can never be reported as finished afterwards. A
source change is a new load: its identity advances, and the identity it replaced
is `cancelled` only if it was still loading.

A **changed estimate is reported as the engine gave it**, including a lower value
than the previous one; only an estimate equal to the one before it is not sent
again. That is `nsh_ext::ProgressTracker::estimate`'s rule — the component does
not impose an invented ordering on native estimates.

An **HTTP error status is not a navigation failure**: a 404 (or any 4xx/5xx)
response is a document the browser renders, so it completes as `finished`. Only a
network-stack failure of the main frame (`onReceivedError`) is `failed`.

`trackProgress` gates **allocation and emission**: while it is `false` the
component allocates no payload and sends no event. It gates each event as it
happens; it is not a replay. Enabling it later neither re-sends nor relabels the
events of a load that started while it was off — in particular no `started` is
synthesized for a navigation already in flight — and only the engine's own
subsequent events are delivered.

## How the component behaves

- **Owned view.** `view()` is a stable `FrameLayout` the component owns. The
  `android.webkit.WebView` is a child, made once per source and released when the
  source changes. It never uses, replaces or wraps NSH's private WebView client or
  delegate, and a replacement touches only this root's child — never the view the
  SDK's component box cached.
- **First source load is deferred** one main-thread turn, so every prop of the
  mount batch — in particular `trackProgress` — is installed before the load
  starts. Source changes in the same turn coalesce to the latest value. With no
  source there is no browser: the element is an empty box.
- **A source change releases the previous browser first**: the active navigation
  is cancelled (`cancelled`), the browser is stopped, detached and destroyed, and
  only then is the new source loaded, cleared or rejected. A stopped page cannot
  keep running, and nothing it reports afterwards can reach the new navigation.
- **`src` empty or removed** cancels the active navigation (`cancelled`) and
  releases the browser: the root is left empty. Clearing is not a load.
- **An unsupported URL scheme** (`javascript:`, `tel:`, a bare host name, …) is
  rejected with `started` then `failed` that says so, instead of running script or
  leaving a navigation the WebView would never complete. The previous browser was
  already released, and no browser is made for the rejected URL.
- **Failures** are main-frame network errors only, and final: `onReceivedError`
  emits `failed` with the engine's description and error code. The browser stays,
  showing the engine's error page, in a retired state with no navigation owned; a
  later source change loads in a browser of its own. The engine's error page never
  becomes a new navigation, and a rendered 4xx/5xx document is not a failure.
- **Renderer death** (`onRenderProcessGone`): a navigation in flight is failed
  (`failed`); the crashed browser is stopped, detached and destroyed, leaving the
  root empty. Nothing is recreated or reloaded: only a later explicit source change
  makes a browser again. (A death while nothing is loading has no navigation to
  report and leaves the same empty root.)
- **`destroy()`** (element removed, app stopped, or another app loaded) stops the
  load, releases the browser, detaches the root, drops the pending deferred load
  and silences callbacks. It emits nothing: the element it would report about is
  gone, and the SDK's own views release silently the same way.
- **No WebView provider** (a device with WebView missing or disabled): the root
  stays empty instead of crashing the host, the reason is logged, and a `src`
  load reports `started` then `failed` with it.
- **The SDK path is the only transport.** `WebViewExtension.register(nsh)` calls
  `NshView.registerComponent`, and `loadProgress` leaves through
  `NshComponentContext.emit`; there is no side channel. The fixture below proves
  that path end to end.

### Android's native navigation identity

`android.webkit` callbacks carry no navigation identifier. Two native facts give
identity instead: **the browser instance**, and **the source load's own window**
inside it. Neither a URL blacklist nor a progress value is involved.

- **A browser is made for one source's load.** The component creates it when the
  source is applied and releases it when the source changes. Every callback is
  checked against the browser that is current now
  (`view != owner || owner != browser || disposed`), so a released browser's late
  start, completion or error cannot cancel, finish or fail the next source's load.
  Going A → B → A is therefore three loads, not a stale callback being rejected.
- **The source's own load owns its browser until it ends.** That browser has no
  other load, so every main-frame report from it — the first start (a redirect can
  resolve to another URL before its document starts), a later start of a redirect
  hop, a completion, a failure — is that navigation's. No URL list decides it.
- **A page's own navigations** in the same browser carry the URLs the engine
  reports. A start of a further document cancels the in-flight page navigation and
  begins a new identity; a completion or failure of the cancelled one is rejected
  by its URL. A same-URL navigation the page starts while nothing is loading
  (`location.reload()`) is a new identity; while that URL is already loading, its
  start is that navigation's own, as the callbacks carry nothing more to tell the
  two apart.

The tradeoffs and irreducible limits, stated rather than papered over:

- **A source change loses page state.** The old browser is destroyed, so its
  history, form state, scroll position and JavaScript context go with it, and the
  new source pays a renderer warm-up. That is the cost of an exact replacement
  without a native navigation id or a history API; the component never replays a
  URL, and the contract has no reload — setting the same `src` string again is not
  a load.
- A page navigation the engine starts *before* the source's own load ends is
  reported as part of that load: Android has no navigation id with which to tell
  it from a redirect hop of the load in flight.
- `onProgressChanged` carries no URL, so an estimate is gated on the current
  navigation but cannot be attributed more precisely than that identity. A late
  estimate from a page navigation that was cancelled inside one browser can
  therefore be reported under its successor (the fraction is still the engine's).
- A redirect during a page navigation that the engine reports neither before its
  document starts nor through `shouldOverrideUrlLoading` leaves that navigation's
  completion or failure unowned: the component reports neither rather than guess
  whose callback it is (the browser still shows the engine's result). A start the
  engine does report is handled as a new page navigation, with a conservative
  `cancelled` for the one it replaced.
- A renderer death while nothing is loading has no event to report (the contract
  has no idle state); the empty root stays until an explicit source change.
- No success is ever inferred from a URL, a progress value or a counter.

## Building and testing

```sh
# From the repository root. Gradle 8.9+, JDK 17, ANDROID_HOME set, and the
# unmodified 0.7.0 nsh.aar either at android/libs/nsh.aar or passed with -PnshAar.
export ANDROID_HOME=/path/to/android-sdk
NSH_AAR=/absolute/path/to/nsh.aar

gradle -p android :webview:assembleDebug -PnshAar="$NSH_AAR"     # the library
gradle -p android :example:assembleDebug -PnshAar="$NSH_AAR"     # the example app

# Build its app bundle once, then install and run:
nsh build android/example --release --out android/example/src/main/assets/nsh
gradle -p android :example:installDebug -PnshAar="$NSH_AAR"

# Instrumentation: a device or emulator, API 26+, with a WebView provider.
adb devices
# The component harness only: no app bundle, no example app.
gradle -p android :webview:connectedDebugAndroidTest \
    --tests com.honsunrise.nshext.WebViewComponentTest -PnshAar="$NSH_AAR"

# The full NSH path: build the fixture bundle once, then run its tests.
nsh build android/fixture --release --out android/webview/src/androidTest/assets/nsh-ext-fixture
gradle -p android :webview:connectedDebugAndroidTest \
    --tests com.honsunrise.nshext.NshTransportTest -PnshAar="$NSH_AAR"

# Everything (after the fixture bundle is built):
gradle -p android :webview:connectedDebugAndroidTest -PnshAar="$NSH_AAR"
# or, with several devices attached:
ANDROID_SERIAL=emulator-5554 gradle -p android :webview:connectedDebugAndroidTest -PnshAar="$NSH_AAR"
```

Two instrumentation classes, with different jobs:

`WebViewComponentTest` is the **component harness**. It hosts the component in a
real activity, drives a real WebView against a loopback HTTP server and records
the events through a test `Sink` in place of `NshComponentContext.emit`. It needs
no app bundle, so it checks the native behavior and the exact event payloads
without NSH's engine in the loop.

| Test | What it proves |
|---|---|
| `reportsStartedLoadingAndFinished` | Exact payload shape and keys, identity starts at 1, estimates are fractions with no unchanged repeat, success is only `finished`, and it comes last. |
| `httpErrorPageCompletesAsARenderedDocument` | A 404 commits as a document and completes as `finished`, never `failed`. |
| `connectionFailureReportsFailed` | A refused connection is `failed`, never `finished`. |
| `unsupportedSchemeReportsFailed` | A scheme the WebView cannot load is `started` then `failed` up front, with no browser made for it, not a navigation that never completes. |
| `invalidSourceRetiresTheLoadInFlight` | A rejected source stops a running load first: the old browser is released, its identity is `cancelled`, the rejected source is identity 2 and `failed`, and the stopped load never resurrects. |
| `sourceChangeReleasesTheOldBrowser` | A source change during a load releases the old browser (detached, replaced by a new instance), so its delayed completion or error cannot reach the replacement. |
| `replacedLoadsLateFailureDoesNotFailTheReplacement` | A replaced load's delayed network error does not fail the navigation that replaced it. |
| `replacedLoadsLateCompletionDoesNotFinishTheReplacement` | A replaced load's delayed completion does not finish the navigation that replaced it. |
| `replacingTheSourceCancelsTheLoadInFlight` | The replaced identity is `cancelled`, the replacement is a new identity, and the cancelled one never finishes. |
| `hostSourceRevisitLoadsAgain` | A → B → A as source changes gives three identities and three finishes: nothing remembers a URL, so a revisit is a load, not a rejected callback. |
| `pageNavigationBeginsANewIdentity` | Navigations the page starts are their own identities in the same browser, including returning to the URL it started on. |
| `sameUrlReloadIsANewNavigationWhenIdle` | `location.reload()` while nothing is loading is a new identity, not the finished one reported again. |
| `redirectStaysOneNavigation` | A server redirect belongs to the load that owns it: one identity, no spurious cancellation. |
| `rootIsStableAcrossBrowserReplacement` | The view the SDK holds never changes; it starts empty, holds the browser, and replaces only that child. |
| `emptySourceCancelsAndReleasesThePage` | Empty `src` cancels and releases the browser (an empty root); the clear is not a load. |
| `noEventsUntilTheAppAsksForThem` | `trackProgress=false` sends nothing, and a finished load is not announced when the app subscribes later. |
| `subscribingMidLoadDoesNotRelabelTheNavigation` | Enabling it during a load synthesizes no `started` event for the navigation that began while it was off. |
| `disposalStopsTheLoadAndSilencesLateCallbacks` | No event arrives after `destroy()`, further props are ignored, the browser is released and the root is detached. |
| `registersOnAnNshView` | The public API registers on a real `NshView` (twice), and the constants match the Rust side. |

`NshTransportTest` is the **full NSH path**, with no injected sink: a real
`NshView`, `WebViewExtension.register` (→ `NshView.registerComponent`), the real
`NshComponentContext.emit`, the real Rust/JS native-component transport and a
real app. [`fixture/App.tsx`](fixture/App.tsx) declares the component with
`nativeComponent`, receives `onLoadProgress`, and posts every payload it got back
over its host channel (`host.post("nsh-ext.fixture", event)`); the test registers
that channel with `NshView.handle` and asserts what arrived.

| Test | What it proves |
|---|---|
| `appReceivesTheLoadThroughItsNativeComponent` | A declared `nsh-ext.webview` element gets a load, the app's `onLoadProgress` fires, and the payload crosses the host channel with the documented keys and values; `started`/`finished` identities match and success is final. |
| `appReceivesAFailedLoadThroughTheTransport` | A real main-frame network failure reaches the app as `failed` with the diagnostic, and never as `finished`. |
| `appSeesAnHttpErrorPageAsAFinishedLoad` | A 404 page reaches the app as `finished`, never `failed`. |
| `appFollowsAHostDrivenNavigation` | `NshView.send("navigate", { url })` changes the app's `src`; the app sees the replaced navigation `cancelled` and the replacement (a new identity) `finished`. |
| `appFollowsARevisitedSource` | A → B → A driven by the host arrives as three new identities and three `finished` events: a revisited source loads again. |
| `disposalEndsTheTransport` | After `NshView.destroy()`, nothing more reaches the app. |

The fixture bundle is compiled output and is not committed. Build it once, then
run the transport tests (the component harness needs neither step):

```sh
nsh build android/fixture --release --out android/webview/src/androidTest/assets/nsh-ext-fixture
gradle -p android :webview:connectedDebugAndroidTest \
    --tests com.honsunrise.nshext.NshTransportTest -PnshAar="$NSH_AAR"
```

The whole instrumentation sets `usesCleartextTraffic` in the test manifest, so
the loopback server works; the library itself does not allow clear text.

## Security

Native components are **trusted host code**: NSH's app capabilities do not
constrain a registered component's WebView. The host decides which URLs may be
loaded, whether untrusted apps may use the component at all, and it should grant
`INTERNET` only if it means it.

The component itself: enables JavaScript and DOM storage (a browser needs them),
disables file and content access from pages, requires a user gesture for media
playback, adds no JavaScript bridge, and declares no new JNI surface. Mixed
content, safe browsing and SSL error handling keep the WebView's defaults; SSL
errors therefore fail the navigation, never show a bypass dialog.

## Attribution

The renderer-death handling and the order in which a browser is stopped, detached
and destroyed follow Native Shell's own `WebBox` (Apache-2.0,
`crates/nsh-platform-android/java/com/longbridge/nsh/`); no NSH source file is
copied. The per-source browser lifecycle — one browser per source, released before
the next source is judged — is this adapter's own design. This adapter is
Apache-2.0 like the rest of `nsh-ext`.
