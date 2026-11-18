// The nsh app the example Android host runs. The host registers
// `nsh-ext.webview` (android/webview: `WebViewExtension.register`), so this page
// shows web content in the host app's own WebView and reads its load progress.
//
// The Rust wrapper of the same component is `nsh_ext::web_view()`.

import { mount, signal, Text, View } from "nsh";
import { nativeComponent } from "nsh";

/** One `loadProgress` payload, exactly as `nsh_ext::LoadProgress` sends it. */
type LoadProgress = {
  navigation: number;
  state: "started" | "loading" | "finished" | "failed" | "cancelled";
  progress: number | null;
  message: string | null;
};

/** The host app's browser view: `src` loads a page, empty stops and clears it. */
const Browser = nativeComponent<
  { src: string; trackProgress: boolean },
  { loadProgress: LoadProgress }
>("nsh-ext.webview", {
  props: ["src", "trackProgress"],
  events: ["loadProgress"],
});

const PAGES = [
  { label: "example.com", src: "https://example.com/" },
  { label: "rust-lang.org", src: "https://www.rust-lang.org/" },
];

function Tab(props: { label: string; selected: boolean; onPress: () => void }) {
  return (
    <View
      role="button"
      onPress={props.onPress}
      style={{
        paddingHorizontal: 12,
        paddingVertical: 8,
        borderRadius: 8,
        backgroundColor: props.selected ? "#0969da" : "#d0d7de",
      }}
    >
      <Text style={{ color: props.selected ? "#ffffff" : "#24292f", fontWeight: "600" }}>
        {props.label}
      </Text>
    </View>
  );
}

function describe(progress: LoadProgress | null): string {
  if (progress === null) return "No load yet";
  const fraction = progress.progress === null ? "" : ` · ${Math.round(progress.progress * 100)}%`;
  const message = progress.message === null ? "" : ` · ${progress.message}`;
  return `${progress.state}${fraction} · navigation ${progress.navigation}${message}`;
}

function App() {
  const page = signal(0);
  const last = signal<LoadProgress | null>(null);
  return (
    <View style={{ flex: 1, gap: 12, padding: 16, backgroundColor: "#f6f8fa" }}>
      <Text style={{ fontSize: 20, fontWeight: "700" }}>Host WebView extension</Text>
      <Text style={{ color: "#57606a" }}>
        The page below is an android.webkit.WebView the host app owns. The nsh app got it with
        nativeComponent("nsh-ext.webview").
      </Text>
      <View style={{ flexDirection: "row", gap: 8 }}>
        {PAGES.map((entry, index) => (
          <Tab
            label={entry.label}
            selected={page() === index}
            onPress={() => page.set(index)}
          />
        ))}
      </View>
      <Text style={{ color: "#57606a" }}>{describe(last())}</Text>
      <Browser
        src={PAGES[page()].src}
        trackProgress={true}
        onLoadProgress={(event) => last.set(event)}
        style={{ flex: 1, borderRadius: 12, overflow: "hidden", backgroundColor: "#ffffff" }}
      />
    </View>
  );
}

mount(App);
