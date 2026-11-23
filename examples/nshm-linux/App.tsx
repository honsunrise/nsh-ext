// The nsh-ext WebView on a real NSH Linux (GTK 4 / WebKitGTK 6) host.
//
// `WebView` is `nsh-ext`'s typed element. The app installs `nsh-ext` and
// `nsh-ext-linux` through `nsh.json` dependencies; NSH registers the Linux
// native view. The app knows nothing about GTK or WebKit: it sets `src`
// and displays `onLoadProgress` events.
//
// The buttons drive the states `lifecycle.test.tsx` checks. The fixture
// server (`acceptance.py`) passes its ephemeral addresses in these
// variables; outside the acceptance run the example uses example.com and a
// refused port for the failure probe.

import { mount, platform, signal, Text, View } from "nsh";
import { WebView } from "nsh-ext";
import process from "process";

/** One `loadProgress` event, exactly as `nsh_ext::LoadProgress` sends it. */
type LoadProgress = {
  navigation: number;
  state: "started" | "loading" | "finished" | "failed" | "cancelled";
  progress: number | null;
  message: string | null;
};

const BASE = process.env.NSH_EXT_FIXTURE_BASE ?? "https://example.com/";
const REFUSED = process.env.NSH_EXT_FIXTURE_REFUSED ?? "http://127.0.0.1:9/";
const PAGE = `${BASE}ok`;
const SLOW = `${BASE}slow`;

function Action(props: { label: string; onPress: () => void }) {
  return (
    <View
      role="button"
      onPress={props.onPress}
      style={{
        paddingHorizontal: 10,
        paddingVertical: 7,
        borderRadius: 7,
        backgroundColor: "#d0d7de",
      }}
    >
      <Text style={{ fontSize: 13, fontWeight: "600", color: "#24292f" }}>{props.label}</Text>
    </View>
  );
}

/** One event as a line the test can read: nullable fields stay `null`. */
function line(event: LoadProgress): string {
  const progress = event.progress === null ? "null" : event.progress.toFixed(3);
  const message = event.message === null ? "null" : "present";
  return `#${event.navigation} ${event.state} progress=${progress} message=${message}`;
}

function App() {
  const src = signal("");
  const tracked = signal(true);
  const mounted = signal(true);
  const events = signal<LoadProgress[]>([]);

  const load = (url: string, withProgress: boolean) => {
    tracked.set(withProgress);
    src.set(url);
  };
  const record = (event: LoadProgress) => {
    events.update((list) => [...list, event].slice(-16));
  };

  return (
    <View style={{ flex: 1, gap: 8, padding: 12, backgroundColor: "#f6f8fa" }}>
      <Text style={{ fontSize: 18, fontWeight: "700" }}>nsh-ext WebView on Linux</Text>
      <Text style={{ fontSize: 12, color: "#57606a" }}>
        {platform.os === "linux" ? "Linux host" : `Not Linux: ${platform.os}`}
      </Text>
      <Text style={{ fontSize: 12, color: "#57606a" }}>
        source = {src() === "" ? "(empty)" : src()} · tracking = {tracked() ? "on" : "off"} · view ={" "}
        {mounted() ? "mounted" : "unmounted"}
      </Text>
      <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
        <Action label="Load page" onPress={() => load(PAGE, true)} />
        <Action label="Load slow" onPress={() => load(SLOW, true)} />
        <Action label="Fail" onPress={() => load(REFUSED, true)} />
        <Action label="Untracked" onPress={() => load(PAGE, false)} />
        <Action
          label="Clear"
          onPress={() => {
            src.set("");
          }}
        />
        <Action
          label="Unmount"
          onPress={() => {
            mounted.set(false);
            src.set("");
          }}
        />
        <Action label="Remount" onPress={() => mounted.set(true)} />
      </View>
      <View style={{ flex: 1, borderRadius: 8, overflow: "hidden", backgroundColor: "#ffffff" }}>
        {mounted() ? (
          <WebView
            style={{ flex: 1 }}
            src={src()}
            trackProgress={tracked()}
            onLoadProgress={(event) => record(event)}
          />
        ) : (
          <Text style={{ fontSize: 13, color: "#57606a", padding: 12 }}>unmounted</Text>
        )}
      </View>
      {/* Keep a visible browser viewport as the native progress log grows. */}
      <Each of={events().slice(-6)}>
        {(event) => <Text style={{ fontSize: 11, color: "#24292f" }}>{line(event())}</Text>}
      </Each>
    </View>
  );
}

mount(App);
