package com.honsunrise.nshext;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertNotSame;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertSame;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import android.app.Instrumentation;
import android.content.Intent;
import android.os.SystemClock;
import android.view.View;
import android.view.ViewGroup;
import android.webkit.WebView;
import android.widget.FrameLayout;
import androidx.test.ext.junit.runners.AndroidJUnit4;
import androidx.test.platform.app.InstrumentationRegistry;
import com.longbridge.nsh.NshView;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Predicate;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;
import org.junit.runner.RunWith;

/**
 * Instrumentation of the extension against a real WebView: a real loopback HTTP
 * server, the real page callbacks and the real event payloads. This class uses a
 * recording {@code Sink} in place of {@link com.longbridge.nsh.NshComponentContext#emit}
 * to see the payloads; {@link NshTransportTest} covers the same component through
 * the real NSH SDK and a real app.
 *
 * <p>The component owns one browser per source: a source change releases the old
 * browser before the new source is judged. {@link #sourceChangeReleasesTheOldBrowser}
 * and {@link #rootIsStableAcrossBrowserReplacement} pin that boundary, while
 * {@link #pageNavigationBeginsANewIdentity} pins the page's own navigations inside
 * one browser.
 */
@RunWith(AndroidJUnit4.class)
public final class WebViewComponentTest {
    private static final long TIMEOUT_MS = 20_000;
    /** Time in which a late callback would show up if a guard were missing. */
    private static final long SETTLE_MS = 700;
    private static final Set<String> PAYLOAD_KEYS =
            Collections.unmodifiableSet(new HashSet<>(Arrays.asList(
                    "navigation", "state", "progress", "message")));

    private final Instrumentation instrumentation = InstrumentationRegistry.getInstrumentation();
    private LocalServer server;
    private TestActivity activity;
    private Recorder recorder;
    private WebViewComponent component;

    @Before
    public void setUp() throws IOException {
        server = new LocalServer();
        Intent intent = new Intent(instrumentation.getTargetContext(), TestActivity.class);
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        activity = (TestActivity) instrumentation.startActivitySync(intent);
    }

    @After
    public void tearDown() {
        WebViewComponent held = component;
        if (held != null) onUi(held::destroy);
        if (activity != null) {
            onUi(activity::finish);
            instrumentation.waitForIdleSync();
        }
        if (server != null) server.close();
    }

    @Test
    public void reportsStartedLoadingAndFinished() {
        start();
        setProps(true, server.url("/ok"));

        Map<String, Object> finished = await("finished");
        List<Event> events = recorder.events();
        assertOnlyProgressEvents(events);
        Map<String, Object> started = events.get(0).payload;
        assertEquals("started", started.get("state"));
        assertEquals(1L, navigation(started));
        assertNull(started.get("progress"));
        assertNull(started.get("message"));
        assertEquals(PAYLOAD_KEYS, started.keySet());

        assertEquals("finished", finished.get("state"));
        assertEquals(1L, navigation(finished));
        assertEquals(1.0, ((Double) finished.get("progress")), 0.0);
        assertNull(finished.get("message"));
        assertEquals(PAYLOAD_KEYS, finished.keySet());

        // The engine's own estimates: fractions, deduplicated when unchanged, and
        // never assumed to increase (nsh_ext::ProgressTracker keeps a changed value).
        Double previous = null;
        for (Event event : events) {
            Map<String, Object> payload = event.payload;
            assertEquals(PAYLOAD_KEYS, payload.keySet());
            if (!"loading".equals(payload.get("state"))) continue;
            double estimate = (Double) payload.get("progress");
            assertTrue("an estimate is a fraction from zero to one", estimate >= 0 && estimate <= 1);
            assertTrue("an unchanged estimate is not sent twice",
                    previous == null || previous.doubleValue() != estimate);
            assertEquals(1L, navigation(payload));
            assertNull(payload.get("message"));
            previous = estimate;
        }

        SystemClock.sleep(SETTLE_MS);
        assertEquals("finished", last().payload.get("state"));
        assertEquals(1, count("finished"));
        assertEvents();
    }

    @Test
    public void httpErrorPageCompletesAsARenderedDocument() {
        start();
        // A 404 is a document the browser renders and finishes, not a failed navigation.
        setProps(true, server.url("/missing"));

        Map<String, Object> finished = await("finished");
        assertEquals(1L, navigation(finished));
        assertEquals(1.0, ((Double) finished.get("progress")), 0.0);
        assertNull(finished.get("message"));
        String url = pageUrl();
        assertTrue("the error page itself committed, got " + url,
                url != null && url.contains("/missing"));

        SystemClock.sleep(SETTLE_MS);
        assertEquals("HTTP status is not a navigation failure", 0, count("failed"));
        assertEquals(1, count("finished"));
        assertEvents();
    }

    @Test
    public void connectionFailureReportsFailed() {
        start();
        setProps(true, "http://127.0.0.1:1/");

        Map<String, Object> failed = await("failed");
        assertEquals(1L, navigation(failed));
        assertNull(failed.get("progress"));
        assertTrue("a failure has a diagnostic",
                failed.get("message") instanceof String
                        && !((String) failed.get("message")).isEmpty());

        SystemClock.sleep(SETTLE_MS);
        assertEquals(0, count("finished"));
        assertEquals(1, count("failed"));
        assertEvents();
    }

    @Test
    public void unsupportedSchemeReportsFailed() {
        start();
        // loadUrl would run this script silently and leave a navigation that never
        // completes, so it is rejected before a browser is made.
        setProps(true, "javascript:alert(1)");

        Map<String, Object> failed = await("failed");
        assertEquals(1L, navigation(failed));
        assertNull(failed.get("progress"));
        assertTrue("the failure names the scheme, got " + failed.get("message"),
                String.valueOf(failed.get("message")).contains("scheme"));
        onUi(() -> assertNull("no browser is made for a URL it cannot load", browserOnUi()));
        assertEquals(0, count("finished"));
        assertEvents();
    }

    @Test
    public void invalidSourceRetiresTheLoadInFlight() {
        start();
        setProps(true, server.url("/slow"));
        await("started");
        // Let the engine start the slow load itself, so a running native load is
        // what the rejected source has to stop.
        SystemClock.sleep(300);
        onUi(() -> component.setProp("src", "javascript:alert(1)"));

        Map<String, Object> failed = await("failed");
        assertEquals("the rejected source is the second identity", 2L, navigation(failed));
        assertEquals(1, count("cancelled"));
        assertEquals(2, count("started"));
        onUi(() -> assertNull("the stopped page's browser is released", browserOnUi()));

        SystemClock.sleep(SETTLE_MS * 2);
        assertEquals("the retired load does not resurrect", 2, count("started"));
        assertEquals(0, count("finished"));
        assertEquals(1, count("failed"));
        assertEvents();
    }

    @Test
    public void sourceChangeReleasesTheOldBrowser() {
        start();
        // A is still loading; B replaces it. The browser A ran in is released, so its
        // delayed completion or error has no view left to report from.
        setProps(true, server.url("/slow"));
        await("started");
        AtomicReference<WebView> first = new AtomicReference<>();
        onUi(() -> first.set(browserOnUi()));
        assertNotNull("the slow load has a browser", first.get());

        setProps(true, server.url("/a"));
        Map<String, Object> cancelled = await("cancelled");
        assertEquals(1L, navigation(cancelled));
        assertEquals(2L, navigation(awaitFinished(2)));

        onUi(() -> {
            assertNull("the replaced browser is not in the view tree", first.get().getParent());
            assertNotSame("a source change loads in a browser of its own", first.get(),
                    browserOnUi());
        });

        SystemClock.sleep(SETTLE_MS);
        assertEquals(0, count("failed"));
        assertEquals(1, count("cancelled"));
        assertEquals(1, count("finished"));
        assertEquals(2, count("started"));
        assertEvents();
    }

    @Test
    public void replacedLoadsLateFailureDoesNotFailTheReplacement() {
        start();
        // A is slow to fail; B replaces it before A's error can arrive.
        setProps(true, server.url("/error-later"));
        await("started");
        setProps(true, server.url("/a"));

        assertEquals(2L, navigation(awaitFinished(2)));

        // A's delayed connection failure would land here if its browser still lived.
        SystemClock.sleep(1500);
        assertEquals("a replaced load's error is not the new navigation's", 0, count("failed"));
        assertEquals(1, count("finished"));
        assertEquals(1, count("cancelled"));
        assertEvents();
    }

    @Test
    public void replacedLoadsLateCompletionDoesNotFinishTheReplacement() {
        start();
        // A's response is delayed; B replaces it and finishes first.
        setProps(true, server.url("/finish-later"));
        await("started");
        setProps(true, server.url("/a"));

        assertEquals(2L, navigation(awaitFinished(2)));

        SystemClock.sleep(1500);
        assertEquals("a replaced load's completion is not the new navigation's",
                1, count("finished"));
        assertEquals(2L, navigation(lastFinished()));
        assertEvents();
    }

    @Test
    public void replacingTheSourceCancelsTheLoadInFlight() {
        start();
        setProps(true, server.url("/slow"));
        Map<String, Object> started = await("started");
        assertEquals(1L, navigation(started));

        SystemClock.sleep(400);
        setProps(true, server.url("/a"));

        Map<String, Object> cancelled = await("cancelled");
        Map<String, Object> finished = await("finished");
        assertEquals("the replaced navigation is cancelled", 1L, navigation(cancelled));
        assertNull(cancelled.get("progress"));
        assertNull(cancelled.get("message"));
        assertEquals("the replacement is a new navigation", 2L, navigation(finished));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(1, count("cancelled"));
        assertEquals(1, count("finished"));
        for (Event event : recorder.events()) {
            if ("finished".equals(event.payload.get("state"))) {
                assertEquals("a cancelled navigation never finishes", 2L, navigation(event.payload));
            }
        }
        assertEvents();
    }

    @Test
    public void redirectStaysOneNavigation() {
        start();
        setProps(true, server.url("/redirect"));

        Map<String, Object> finished = await("finished");
        assertEquals("a server redirect is part of the load that owns it", 1L, navigation(finished));
        String url = pageUrl();
        assertTrue("the redirect's target committed, got " + url,
                url != null && url.endsWith("/ok"));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(1, count("started"));
        assertEquals(1, count("finished"));
        assertEquals(0, count("cancelled"));
        assertEquals(0, count("failed"));
        assertEvents();
    }

    @Test
    public void hostSourceRevisitLoadsAgain() {
        start();
        // A source change is a new load of that URL, even when the app visited it
        // before: nothing remembers it, so nothing rejects the revisit.
        setProps(true, server.url("/a"));
        assertEquals(1L, navigation(awaitFinished(1)));

        setProps(true, server.url("/b"));
        assertEquals(2L, navigation(awaitFinished(2)));

        setProps(true, server.url("/a"));
        assertEquals(3L, navigation(awaitFinished(3)));

        SystemClock.sleep(SETTLE_MS);
        assertEquals("each source change happened while nothing was loading", 0,
                count("cancelled"));
        assertEquals(3, count("started"));
        assertEquals(3, count("finished"));
        assertEvents();
    }

    @Test
    public void pageNavigationBeginsANewIdentity() {
        start();
        setProps(true, server.url("/a"));
        assertEquals(1L, navigation(awaitFinished(1)));
        AtomicReference<WebView> browser = new AtomicReference<>();
        onUi(() -> browser.set(browserOnUi()));

        // The page navigates itself, then comes back to the URL it started on: a
        // link/history navigation is a new identity in the same browser, and the
        // revisit is not mistaken for a stale callback.
        navigateInPage(server.url("/b"));
        assertEquals(2L, navigation(awaitFinished(2)));

        navigateInPage(server.url("/a"));
        assertEquals(3L, navigation(awaitFinished(3)));

        SystemClock.sleep(SETTLE_MS);
        assertEquals("the page's own navigations are not cancellations", 0, count("cancelled"));
        onUi(() -> assertSame("the page keeps its browser", browser.get(), browserOnUi()));
        assertEquals(3, count("started"));
        assertEquals(3, count("finished"));
        assertEvents();
    }

    @Test
    public void sameUrlReloadIsANewNavigationWhenIdle() {
        start();
        setProps(true, server.url("/a"));
        assertEquals(1L, navigation(awaitFinished(1)));

        // A same-URL navigation the page started: no navigation was in flight, so it
        // is a new identity, not the finished one still being reported.
        onUi(() -> {
            WebView browser = browserOnUi();
            assertNotNull(browser);
            browser.evaluateJavascript("location.reload()", null);
        });
        assertEquals(2L, navigation(awaitFinished(2)));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(2, count("started"));
        assertEquals(2, count("finished"));
        assertEquals(0, count("cancelled"));
        assertEvents();
    }

    @Test
    public void rootIsStableAcrossBrowserReplacement() {
        start();
        AtomicReference<View> root = new AtomicReference<>();
        onUi(() -> root.set(component.view()));
        assertTrue("the component's view is its own root", root.get() instanceof FrameLayout);
        onUi(() -> assertNull("an empty source shows no browser", browserOnUi()));

        setProps(true, server.url("/a"));
        awaitFinished(1);
        onUi(() -> assertTrue("the root holds the browser", browserOnUi() != null));
        AtomicReference<WebView> first = new AtomicReference<>();
        onUi(() -> first.set(browserOnUi()));

        setProps(true, server.url("/b"));
        awaitFinished(2);
        onUi(() -> {
            assertSame("the SDK's cached view never changes", root.get(), component.view());
            assertNotSame("the source change replaces the browser", first.get(), browserOnUi());
            assertNull("the old browser is detached", first.get().getParent());
        });
    }

    @Test
    public void emptySourceCancelsAndReleasesThePage() {
        start();
        setProps(true, server.url("/slow"));
        await("started");

        setProps(true, "");
        Map<String, Object> cancelled = await("cancelled");
        assertEquals(1L, navigation(cancelled));
        assertNull(cancelled.get("progress"));
        assertNull(cancelled.get("message"));
        onUi(() -> assertNull("an empty source releases the browser", browserOnUi()));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(1, count("cancelled"));
        assertEquals("clearing the view is not a load", 1, count("started"));
        assertEquals(0, count("finished"));
        assertEquals(0, count("failed"));
        assertEvents();
    }

    @Test
    public void noEventsUntilTheAppAsksForThem() {
        start();
        setProps(false, server.url("/ok"));
        awaitDocumentTitle("ok");
        assertTrue("nothing is sent while trackProgress is false", recorder.events().isEmpty());

        // The same source is not a new navigation; the finished load is not announced
        // behind the app's back when it subscribes afterwards.
        setProps(true, server.url("/ok"));
        SystemClock.sleep(400);
        assertTrue(recorder.events().isEmpty());
    }

    @Test
    public void subscribingMidLoadDoesNotRelabelTheNavigation() {
        start();
        setProps(false, server.url("/slow"));
        // The deferred source load has started by now; the app subscribes while it runs.
        SystemClock.sleep(400);
        onUi(() -> component.setProp("trackProgress", true));
        SystemClock.sleep(SETTLE_MS);

        assertEquals("no start is synthesized for a navigation already in flight",
                0, count("started"));
        for (Event event : recorder.events()) {
            assertEquals("events belong to the load the engine is running",
                    1L, navigation(event.payload));
            assertEquals(WebViewExtension.PROGRESS_EVENT, event.name);
            assertTrue("only engine estimates arrive, got " + event,
                    "loading".equals(event.payload.get("state")));
        }
        assertEvents();
    }

    @Test
    public void disposalStopsTheLoadAndSilencesLateCallbacks() {
        start();
        setProps(true, server.url("/slow"));
        await("started");

        onUi(component::destroy);
        SystemClock.sleep(SETTLE_MS);
        int afterDisposal = recorder.events().size();
        SystemClock.sleep(SETTLE_MS * 2);
        assertEquals("no event arrives after disposal", afterDisposal, recorder.events().size());
        assertEquals(0, count("finished"));

        // A disposed component ignores further props.
        onUi(() -> component.setProp("src", server.url("/ok")));
        SystemClock.sleep(SETTLE_MS);
        assertEquals(afterDisposal, recorder.events().size());

        AtomicReference<Boolean> detached = new AtomicReference<>();
        onUi(() -> {
            detached.set(component.view().getParent() == null);
            assertNull("the browser is released", browserOnUi());
        });
        assertTrue("the root is detached", detached.get());
        assertEvents();
    }

    @Test
    public void registersOnAnNshView() {
        assertEquals("nsh-ext.webview", WebViewExtension.COMPONENT_NAME);
        assertEquals("loadProgress", WebViewExtension.PROGRESS_EVENT);
        onUi(() -> {
            NshView nsh = new NshView(activity);
            try {
                WebViewExtension.register(nsh);
                // A second registration replaces the factory, as the SDK documents.
                WebViewExtension.register(nsh);
            } finally {
                nsh.destroy();
            }
        });
    }

    @Test(expected = NullPointerException.class)
    public void registerRejectsANullView() {
        WebViewExtension.register(null);
    }

    // The component and its events.

    private void start() {
        onUi(() -> {
            recorder = new Recorder();
            component = new WebViewComponent(activity, recorder);
            activity.setContentView(component.view());
        });
    }

    /** The order the Rust wrapper sets them in: {@code src}, then {@code trackProgress}. */
    private void setProps(boolean trackProgress, String src) {
        onUi(() -> {
            component.setProp("src", src);
            component.setProp("trackProgress", trackProgress);
        });
    }

    private void onUi(Runnable action) {
        instrumentation.runOnMainSync(action);
    }

    /** The browser child of the stable root; call on the UI thread. */
    private WebView browserOnUi() {
        View view = component.view();
        if (!(view instanceof ViewGroup)) return null;
        ViewGroup group = (ViewGroup) view;
        for (int i = 0; i < group.getChildCount(); i++) {
            if (group.getChildAt(i) instanceof WebView) return (WebView) group.getChildAt(i);
        }
        return null;
    }

    /** Navigates the page itself, as a link or history move would. */
    private void navigateInPage(String url) {
        onUi(() -> {
            WebView browser = browserOnUi();
            assertNotNull("the browser is gone", browser);
            browser.evaluateJavascript("location.href='" + url + "'", null);
        });
    }

    private String pageUrl() {
        AtomicReference<String> url = new AtomicReference<>();
        onUi(() -> {
            WebView web = browserOnUi();
            if (web != null) url.set(web.getUrl());
        });
        return url.get();
    }

    private Map<String, Object> await(String state) {
        return awaitEvent(event -> state.equals(event.payload.get("state")));
    }

    private Map<String, Object> awaitFinished(long identity) {
        return awaitEvent(event -> "finished".equals(event.payload.get("state"))
                && navigation(event.payload) == identity);
    }

    private Map<String, Object> awaitEvent(Predicate<Event> match) {
        long deadline = SystemClock.uptimeMillis() + TIMEOUT_MS;
        while (SystemClock.uptimeMillis() < deadline) {
            for (Event event : recorder.events()) {
                if (match.test(event)) return event.payload;
            }
            SystemClock.sleep(20);
        }
        fail("no matching loadProgress event; saw " + recorder.events());
        return null;
    }

    private Event last() {
        List<Event> events = recorder.events();
        assertTrue("no event was sent", !events.isEmpty());
        return events.get(events.size() - 1);
    }

    private Map<String, Object> lastFinished() {
        List<Event> events = recorder.events();
        for (int i = events.size() - 1; i >= 0; i--) {
            if ("finished".equals(events.get(i).payload.get("state"))) return events.get(i).payload;
        }
        fail("no finished event; saw " + events);
        return null;
    }

    private int count(String state) {
        int found = 0;
        for (Event event : recorder.events()) {
            if (state.equals(event.payload.get("state"))) found++;
        }
        return found;
    }

    private static long navigation(Map<String, Object> payload) {
        Object value = payload.get("navigation");
        assertTrue("navigation is a number, got " + value, value instanceof Number);
        return ((Number) value).longValue();
    }

    private void assertEvents() {
        for (Event event : recorder.events()) {
            assertEquals(WebViewExtension.PROGRESS_EVENT, event.name);
            assertPayload(event.payload);
        }
    }

    /** Every event is one documented {@code nsh_ext::LoadProgress}. */
    private static void assertPayload(Map<String, Object> payload) {
        assertEquals(PAYLOAD_KEYS, payload.keySet());
        long identity = navigation(payload);
        assertTrue("navigation is positive, got " + identity, identity >= 1);
        Object state = payload.get("state");
        assertTrue("state is a string, got " + state, state instanceof String);
        Object progress = payload.get("progress");
        Object message = payload.get("message");
        switch ((String) state) {
            case "started":
            case "cancelled":
                assertNull("no estimate when nothing is loading", progress);
                assertNull(message);
                break;
            case "loading":
                assertTrue("an estimate is a number, got " + progress, progress instanceof Double);
                double fraction = (Double) progress;
                assertTrue("an estimate is a fraction from zero to one", fraction >= 0 && fraction <= 1);
                assertNull(message);
                break;
            case "finished":
                assertTrue("success carries its completion, got " + progress, progress instanceof Double);
                assertEquals(1.0, (Double) progress, 0.0);
                assertNull(message);
                break;
            case "failed":
                assertNull("a failure has no estimate", progress);
                assertTrue("a failure has a diagnostic, got " + message,
                        message instanceof String && !((String) message).isEmpty());
                break;
            default:
                fail("unknown state " + state);
        }
    }

    private static void assertOnlyProgressEvents(List<Event> events) {
        for (Event event : events) {
            assertEquals(WebViewExtension.PROGRESS_EVENT, event.name);
        }
    }

    /** The page committed and ran script: the load finished, whether or not it was tracked. */
    private void awaitDocumentTitle(String expected) {
        long deadline = SystemClock.uptimeMillis() + TIMEOUT_MS;
        AtomicReference<String> seen = new AtomicReference<>();
        while (SystemClock.uptimeMillis() < deadline) {
            onUi(() -> {
                WebView web = browserOnUi();
                if (web != null) {
                    web.evaluateJavascript("document.title", value -> seen.set(value));
                }
            });
            String title = seen.get();
            if (title != null && title.contains(expected)) return;
            SystemClock.sleep(50);
        }
        fail("the page did not finish loading; its title was " + seen.get());
    }

    /** One emitted event: its name and a copy of its payload. */
    static final class Event {
        final String name;
        final Map<String, Object> payload;

        Event(String name, Map<String, Object> payload) {
            this.name = name;
            this.payload = payload;
        }

        @Override
        public String toString() {
            return name + " " + payload;
        }
    }

    /** The event sink the component uses in place of {@code NshComponentContext.emit}. */
    static final class Recorder implements WebViewComponent.Sink {
        private final List<Event> events = Collections.synchronizedList(new ArrayList<>());

        @Override
        public void emit(String event, Object payload) {
            Map<String, Object> copy = new LinkedHashMap<>();
            if (payload instanceof Map) {
                for (Map.Entry<?, ?> entry : ((Map<?, ?>) payload).entrySet()) {
                    copy.put(String.valueOf(entry.getKey()), entry.getValue());
                }
            }
            events.add(new Event(event, copy));
        }

        List<Event> events() {
            synchronized (events) {
                return new ArrayList<>(events);
            }
        }
    }
}
