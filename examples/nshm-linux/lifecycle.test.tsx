// The real GTK 4 / WebKitGTK 6 lifecycle, as `nsh test --target linux` sees
// it: every step reaches a real native window and the events are the ones
// WebKit emitted. `acceptance.py` starts the local fixture server on
// ephemeral loopback ports and runs this file; without its variables the
// tests fail rather than touch the network.
//
// The app prints `Linux host` only when its own platform is Linux, so these
// tests fail on the default headless engine (an iOS engine) instead of
// pretending the native path ran. `app.screenshot` saves real pixels for a
// person to inspect; a headless run draws nothing and rejects.

import { expect, open, test } from "nsh/test";
import type { App } from "nsh/test";

const TIMEOUT = 20_000;
const STARTED = "text:/^#1 started progress=null message=null$/";
const FINISHED_1 = "text:/^#1 finished progress=1\\.000 message=null$/";
const CANCELLED_1 = "text:/^#1 cancelled progress=null message=null$/";
const FAILED_1 = "text:/^#1 failed progress=null message=present$/";

type Row = {
  navigation: number;
  state: string;
  progress: number | null;
  message: string | null;
};

/** The event lines the app rendered, in order. */
function rows(app: App): Row[] {
  const out: Row[] = [];
  for (const text of app.texts()) {
    const match = /^#(\d+) (started|loading|finished|failed|cancelled) progress=(\S+) message=(\S+)$/.exec(
      text.trim(),
    );
    if (match === null) continue;
    out.push({
      navigation: Number(match[1]),
      state: match[2],
      progress: match[3] === "null" ? null : Number(match[3]),
      message: match[4] === "null" ? null : match[4],
    });
  }
  return out;
}

function eventsFor(rows: Row[], navigation: number): Row[] {
  return rows.filter((row) => row.navigation === navigation);
}

/** Opens the app on the Linux host; a headless run never gets this far. */
async function openLinux(): Promise<App> {
  const app = await open("./App.tsx");
  await app.waitFor("Linux host", { timeout: TIMEOUT });
  expect(app.texts()).toContain("Linux host");
  return app;
}

test("loads a page and saves a native screenshot", async () => {
  const app = await openLinux();
  await app.press("Load page");
  await app.waitFor(FINISHED_1, { timeout: TIMEOUT });

  const events = rows(app);
  const started = eventsFor(events, 1).find((row) => row.state === "started");
  expect(started).not.toBe(undefined);
  expect(started!.progress).toBe(null);
  expect(events.some((row) => row.state === "failed")).toBe(false);
  const finished = eventsFor(events, 1).find((row) => row.state === "finished");
  expect(finished!.progress).toBe(1);

  // Real pixels of the real GTK window and the WebKit surface in it.
  await app.wait(300);
  await app.screenshot(".nsh/acceptance-loaded.png");
});

test("reports WebKit's own estimate while a slow page streams", async () => {
  const app = await openLinux();
  await app.press("Load slow");
  await app.waitFor(STARTED, { timeout: TIMEOUT });
  await app.waitFor("text:/^#1 loading progress=/", { timeout: TIMEOUT });
  await app.waitFor(FINISHED_1, { timeout: TIMEOUT });

  const loading = eventsFor(rows(app), 1).filter((row) => row.state === "loading");
  expect(loading.length).toBeGreaterThan(0);
  expect(
    loading.some((row) => row.progress !== null && row.progress > 0 && row.progress < 1),
  ).toBe(true);
  expect(loading.every((row) => row.progress !== null && row.progress <= 1)).toBe(true);
});

test("reports a refused connection as a native failure", async () => {
  const app = await openLinux();
  await app.press("Fail");
  await app.waitFor(FAILED_1, { timeout: TIMEOUT });

  const events = eventsFor(rows(app), 1);
  expect(events.some((row) => row.state === "finished")).toBe(false);
  const failed = events.find((row) => row.state === "failed");
  expect(failed!.progress).toBe(null);
  expect(failed!.message).not.toBe(null);
});

test("cancels a slow navigation and finishes its replacement", async () => {
  const app = await openLinux();
  await app.press("Load slow");
  await app.waitFor(STARTED, { timeout: TIMEOUT });
  await app.press("Load page");
  await app.waitFor(CANCELLED_1, { timeout: TIMEOUT });
  await app.waitFor("text:/^#2 finished progress=1\\.000 message=null$/", { timeout: TIMEOUT });

  const events = rows(app);
  const first = eventsFor(events, 1);
  expect(first.some((row) => row.state === "finished")).toBe(false);
  expect(first[first.length - 1].state).toBe("cancelled");
  const second = eventsFor(events, 2);
  expect(second[0].state).toBe("started");
  expect(second.some((row) => row.state === "finished")).toBe(true);
});

test("clearing the source ends the load without a late completion", async () => {
  const app = await openLinux();
  await app.press("Load slow");
  await app.waitFor(STARTED, { timeout: TIMEOUT });
  await app.press("Clear");
  await app.waitFor(CANCELLED_1, { timeout: TIMEOUT });

  await app.wait(700);
  const after = eventsFor(rows(app), 1);
  expect(after.some((row) => row.state === "finished" || row.state === "failed")).toBe(false);

  await app.press("Load page");
  await app.waitFor("text:/^#2 finished progress=1\\.000 message=null$/", { timeout: TIMEOUT });
});

test("disposal fences late events and a remount starts a fresh view", async () => {
  const app = await openLinux();
  await app.press("Load slow");
  await app.waitFor(STARTED, { timeout: TIMEOUT });
  await app.press("Unmount");
  await app.waitFor("unmounted", { timeout: TIMEOUT });
  await app.wait(400);

  const before = rows(app);
  await app.wait(800);
  expect(rows(app)).toEqual(before);

  // A source set while unmapped must not load or report anything.
  await app.press("Load page");
  await app.wait(600);
  expect(rows(app)).toEqual(before);

  await app.press("Remount");
  await app.waitFor(FINISHED_1, { timeout: TIMEOUT });
  const after = rows(app);
  // The retired view never emitted again; the new one began at navigation 1.
  expect(after.some((row) => row.navigation === 2)).toBe(false);
  expect(after.filter((row) => row.navigation === 1 && row.state === "finished").length).toBe(1);
});

test("trackProgress false emits no events at all", async () => {
  const app = await openLinux();
  await app.press("Untracked");
  await app.wait(1500);
  expect(rows(app).length).toBe(0);
});
