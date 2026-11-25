# nsh-ext WebView on Linux

An NSH app that shows the typed `WebView` from [`nsh-ext`](../../) on a real
GTK 4 / WebKitGTK 6 host. The app imports `WebView` from `nsh-ext`; the
native component itself comes from the local adapter crate
[`nsh-ext-linux`](../../crates/linux). Both modules are listed in `nsh.json`
under `"dependencies"`.

`WebView` takes:

- `src`: the URL to load; an empty string stops the load and clears the page;
- `trackProgress` (optional, default `false`): whether native progress
  events are emitted;
- `onLoadProgress`: receives `{ navigation, state, progress, message }`,
  where `progress` is WebKit's own estimate or `null`, and `message` is a
  failure diagnostic or `null`.

The buttons drive the lifecycle scenarios. The footer shows the six most recent
events, keeping room for the native viewport while progress updates arrive.

## Run it

Needs Linux, Rust and a C toolchain, GTK 4 / WebKitGTK 6 development packages,
and GStreamer core/base development packages for NSH's development host. Use a
display or `xvfb-run -a` in a headless session.

Use the released NSH 0.7.0 CLI. Both local modules are already listed in
`nsh.json` and `nsh.lock`. To add them to another app, see the
[installation instructions](../../README.md#install-as-an-nshm).

```console
nsh run examples/nshm-linux/App.tsx
```

Without the fixture environment variables, the example uses example.com for
its page buttons and `http://127.0.0.1:9/` for its failure button.

## Acceptance test

`acceptance.py` is the runner. It starts the fixture on an ephemeral loopback
port, reserves a second non-listening port for a genuine connection refusal,
runs `nsh test . --target linux` with the supplied NSH 0.7.0 CLI, and shuts the
server down on every path. The tests fail unless the app reports its own
platform as Linux, so a headless run cannot pass.

```console
xvfb-run -a python3 examples/nshm-linux/acceptance.py \
    --nsh /path/to/nsh
```

The successful run saves a real screenshot of the GTK window and its WebKit
surface to `.nsh/acceptance-loaded.png` (ignored by git); the script verifies
the PNG and prints its path.

With the fixture addresses exported, the same tests run directly from the
example directory (so the screenshot lands in its `.nsh/`):

```console
cd examples/nshm-linux
export NSH_EXT_FIXTURE_BASE=http://127.0.0.1:<port>/
export NSH_EXT_FIXTURE_REFUSED=http://127.0.0.1:<closed-port>/
nsh test . --target linux
```

The suite covers: a successful load with WebKit's native estimates and a
native screenshot; a refused connection reported as `failed` (not an HTTP
error document); a slow load replaced mid-navigation (`cancelled`, then a new
identity's `finished`); clearing `src` mid-load with no late completion;
disposal mid-load with no late events and a fresh navigation identity after
remount; and `trackProgress: false` emitting nothing.
