// The minimal app the full-path instrumentation (NshTransportTest) runs.
//
// It goes through the real NSH transport end to end: `nativeComponent` declares
// the host's component, the SDK hands it `src`/`trackProgress`, the component's
// events arrive as `onLoadProgress`, and each event the app received is posted
// back over the app's host channel, which is what the test records. Nothing here
// knows about the Android implementation of the component.

import { host, mount, signal, Text, View } from "nsh";
import { nativeComponent } from "nsh";

/** One `loadProgress` payload, as `nsh_ext::LoadProgress` sends it. */
type LoadProgress = {
  navigation: number;
  state: "started" | "loading" | "finished" | "failed" | "cancelled";
  progress: number | null;
  message: string | null;
};

/** The host app's browser component, registered by its Java host. */
const Browser = nativeComponent<
  { src: string; trackProgress: boolean },
  { loadProgress: LoadProgress }
>("nsh-ext.webview", {
  props: ["src", "trackProgress"],
  events: ["loadProgress"],
});

/** The host channel the test registers with `NshView.handle`. */
const CHANNEL = "nsh-ext.fixture";

function App() {
  // The test passes `NshView.setParams({ url })`; the host moves it later with
  // `NshView.send("navigate", { url })`.
  const src = signal(typeof host.params.url === "string" ? host.params.url : "");
  const last = signal<LoadProgress | null>(null);
  host.on<{ url: string }>("navigate", (payload) => {
    if (payload && typeof payload.url === "string") src.set(payload.url);
  });
  return (
    <View style={{ flex: 1, gap: 8, padding: 12, backgroundColor: "#ffffff" }}>
      <Text style={{ fontSize: 16, fontWeight: "700" }}>nsh-ext fixture</Text>
      <Text style={{ color: "#57606a" }}>
        {last() === null ? "No load yet" : `${last()!.state} · navigation ${last()!.navigation}`}
      </Text>
      <Browser
        src={src()}
        trackProgress={true}
        onLoadProgress={(event) => {
          last.set(event);
          // The event the app really received, back to its embedding host.
          host.post(CHANNEL, event);
        }}
        style={{ flex: 1, borderRadius: 8, overflow: "hidden", backgroundColor: "#f6f8fa" }}
      />
    </View>
  );
}

mount(App);
