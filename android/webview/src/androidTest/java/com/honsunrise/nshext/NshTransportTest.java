package com.honsunrise.nshext;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import android.app.Instrumentation;
import android.content.Intent;
import android.os.SystemClock;
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
import java.util.function.Predicate;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;
import org.junit.runner.RunWith;

/**
 * The full NSH path, with no injected sink anywhere:
 * {@code fixture/App.tsx} declares {@code nativeComponent("nsh-ext.webview")},
 * the real {@link NshView} makes the component through the SDK, the Java
 * component emits with the real {@code NshComponentContext.emit}, and the app's
 * {@code onLoadProgress} posts what it received back over its host channel,
 * which this test records.
 *
 * <p>The fixture bundle is compiled output, built once before the test:
 * {@code nsh build android/fixture --release --out
 * android/webview/src/androidTest/assets/nsh-ext-fixture}. The component's own
 * native behavior is covered separately by {@link WebViewComponentTest}, which
 * uses a recording sink in place of the SDK context.
 */
@RunWith(AndroidJUnit4.class)
public final class NshTransportTest {
    private static final String ASSET = "nsh-ext-fixture";
    private static final String CHANNEL = "nsh-ext.fixture";
    private static final String BUILD = "nsh build android/fixture --release --out "
            + "android/webview/src/androidTest/assets/" + ASSET;
    private static final long TIMEOUT_MS = 30_000;
    /** Time in which a late event would show up if a guard were missing. */
    private static final long SETTLE_MS = 700;
    private static final Set<String> PAYLOAD_KEYS =
            Collections.unmodifiableSet(new HashSet<>(Arrays.asList(
                    "navigation", "state", "progress", "message")));

    private final Instrumentation instrumentation = InstrumentationRegistry.getInstrumentation();
    private final List<String> errors = Collections.synchronizedList(new ArrayList<>());
    private LocalServer server;
    private TestActivity activity;
    private NshView nsh;
    private Posts posts;

    @Before
    public void setUp() throws IOException {
        server = new LocalServer();
        Intent intent = new Intent(instrumentation.getTargetContext(), TestActivity.class);
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        activity = (TestActivity) instrumentation.startActivitySync(intent);
    }

    @After
    public void tearDown() {
        NshView held = nsh;
        if (held != null) onUi(held::destroy);
        if (activity != null) {
            onUi(activity::finish);
            instrumentation.waitForIdleSync();
        }
        if (server != null) server.close();
    }

    @Test
    public void appReceivesTheLoadThroughItsNativeComponent() {
        start(server.url("/ok"));

        Map<String, Object> finished = posts.await("finished");
        assertTrue("the app reported no error, got " + errors, errors.isEmpty());
        assertEquals("started", posts.first().get("state"));
        assertEquals(1L, navigation(posts.first()));
        assertNull(posts.first().get("progress"));
        assertEquals(PAYLOAD_KEYS, posts.first().keySet());

        assertEquals("finished", finished.get("state"));
        assertEquals(1L, navigation(finished));
        assertEquals(1.0, number(finished.get("progress")), 0.0);
        assertNull(finished.get("message"));
        assertEquals(PAYLOAD_KEYS, finished.keySet());

        SystemClock.sleep(SETTLE_MS);
        assertEquals("finished", posts.last().get("state"));
        assertEquals(1, posts.count("finished"));
        posts.assertShapes();
    }

    @Test
    public void appReceivesAFailedLoadThroughTheTransport() {
        // A real network failure, refused before any document exists.
        start("http://127.0.0.1:1/");

        Map<String, Object> failed = posts.await("failed");
        assertEquals(1L, navigation(failed));
        assertNull(failed.get("progress"));
        Object message = failed.get("message");
        assertTrue("a failure has a diagnostic, got " + message, message instanceof String);
        assertTrue("the diagnostic is not empty", !((String) message).isEmpty());

        SystemClock.sleep(SETTLE_MS);
        assertEquals(0, posts.count("finished"));
        assertEquals(1, posts.count("failed"));
        posts.assertShapes();
    }

    @Test
    public void appSeesAnHttpErrorPageAsAFinishedLoad() {
        // A 404 is a document the browser renders and completes; the app hears it
        // finished, not failed (HTTP status alone is not a navigation failure).
        start(server.url("/missing"));

        Map<String, Object> finished = posts.await("finished");
        assertEquals(1L, navigation(finished));
        assertEquals(1.0, number(finished.get("progress")), 0.0);
        assertNull(finished.get("message"));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(0, posts.count("failed"));
        assertEquals(1, posts.count("finished"));
        posts.assertShapes();
    }

    @Test
    public void appFollowsAHostDrivenNavigation() {
        start(server.url("/slow"));
        posts.await("started");
        SystemClock.sleep(400);

        onUi(() -> assertTrue("the fixture did not take the host's event",
                nsh.send("navigate", Collections.singletonMap("url", server.url("/ok")))));

        Map<String, Object> cancelled = posts.await("cancelled");
        Map<String, Object> finished = posts.await("finished");
        assertEquals("the replaced navigation is cancelled", 1L, navigation(cancelled));
        assertNull(cancelled.get("progress"));
        assertEquals("the replacement is a new navigation", 2L, navigation(finished));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(1, posts.count("cancelled"));
        assertEquals(1, posts.count("finished"));
        for (Map<String, Object> post : posts.all()) {
            if ("finished".equals(post.get("state"))) {
                assertEquals("a cancelled navigation never finishes", 2L, navigation(post));
            }
        }
        posts.assertShapes();
    }

    @Test
    public void appFollowsARevisitedSource() {
        start(server.url("/a"));
        assertEquals(1L, navigation(posts.awaitFinished(1)));

        // A source change loads that URL again in a browser of its own, even when the
        // app visited it before: the app sees a new navigation, not a rejected revisit.
        onUi(() -> assertTrue("the fixture did not take the host's event",
                nsh.send("navigate", Collections.singletonMap("url", server.url("/b")))));
        assertEquals(2L, navigation(posts.awaitFinished(2)));

        onUi(() -> assertTrue("the fixture did not take the host's event",
                nsh.send("navigate", Collections.singletonMap("url", server.url("/a")))));
        assertEquals(3L, navigation(posts.awaitFinished(3)));

        SystemClock.sleep(SETTLE_MS);
        assertEquals(3, posts.count("started"));
        assertEquals(3, posts.count("finished"));
        assertEquals("each source change happened while nothing was loading", 0,
                posts.count("cancelled"));
        posts.assertShapes();
    }

    @Test
    public void disposalEndsTheTransport() {
        start(server.url("/slow"));
        posts.await("started");

        onUi(nsh::destroy);
        SystemClock.sleep(SETTLE_MS);
        int afterDisposal = posts.size();
        SystemClock.sleep(SETTLE_MS * 2);
        assertEquals("no event reaches the app after its view is destroyed",
                afterDisposal, posts.size());
        assertEquals(0, posts.count("finished"));
    }

    /** Creates the real host view, registers the extension and runs the fixture. */
    private void start(String url) {
        onUi(() -> {
            posts = new Posts();
            nsh = new NshView(activity);
            nsh.setErrorListener(message -> errors.add(message));
            // The public Java API: NshView.registerComponent("nsh-ext.webview", ...).
            WebViewExtension.register(nsh);
            // What the app posts, and the only channel it may use.
            nsh.handle(CHANNEL, (data, reply) -> posts.add(data));
            assertTrue("the fixture did not take the launch parameters",
                    nsh.setParams(Collections.singletonMap("url", url)));
            activity.setContentView(nsh);
            assertTrue("the fixture bundle is missing; build it with: " + BUILD,
                    nsh.loadAssetBundle(ASSET));
        });
    }

    private void onUi(Runnable action) {
        instrumentation.runOnMainSync(action);
    }

    private static long navigation(Map<String, Object> payload) {
        Object value = payload.get("navigation");
        assertTrue("navigation is a number, got " + value, value instanceof Number);
        return ((Number) value).longValue();
    }

    private static double number(Object value) {
        assertTrue("a fraction is a number, got " + value, value instanceof Number);
        return ((Number) value).doubleValue();
    }

    /**
     * Every post is one {@code nsh_ext::LoadProgress} that survived the app's JSON
     * round trip, so numbers come back as whatever JSON parsed into.
     */
    static final class Posts {
        private final List<Map<String, Object>> posts =
                Collections.synchronizedList(new ArrayList<>());

        /** The host's calls and posts, as {@code NshView.HostHandler} gives them. */
        void add(Object data) {
            Map<String, Object> copy = new LinkedHashMap<>();
            if (data instanceof Map) {
                for (Map.Entry<?, ?> entry : ((Map<?, ?>) data).entrySet()) {
                    copy.put(String.valueOf(entry.getKey()), entry.getValue());
                }
            }
            posts.add(copy);
        }

        List<Map<String, Object>> all() {
            synchronized (posts) {
                return new ArrayList<>(posts);
            }
        }

        int size() {
            return posts.size();
        }

        Map<String, Object> first() {
            List<Map<String, Object>> all = all();
            assertTrue("the app posted nothing", !all.isEmpty());
            return all.get(0);
        }

        Map<String, Object> last() {
            List<Map<String, Object>> all = all();
            assertTrue("the app posted nothing", !all.isEmpty());
            return all.get(all.size() - 1);
        }

        int count(String state) {
            int found = 0;
            for (Map<String, Object> post : all()) {
                if (state.equals(post.get("state"))) found++;
            }
            return found;
        }

        Map<String, Object> await(String state) {
            return await(post -> state.equals(post.get("state")));
        }

        /** The finished event of one navigation identity. */
        Map<String, Object> awaitFinished(long identity) {
            return await(post -> "finished".equals(post.get("state"))
                    && navigation(post) == identity);
        }

        Map<String, Object> await(Predicate<Map<String, Object>> match) {
            long deadline = SystemClock.uptimeMillis() + TIMEOUT_MS;
            while (SystemClock.uptimeMillis() < deadline) {
                for (Map<String, Object> post : all()) {
                    if (match.test(post)) return post;
                }
                SystemClock.sleep(20);
            }
            fail("the app posted no matching event; it posted " + all());
            return null;
        }

        void assertShapes() {
            for (Map<String, Object> payload : all()) {
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
                        double fraction = number(progress);
                        assertTrue("an estimate is a fraction from zero to one",
                                fraction >= 0 && fraction <= 1);
                        assertNull(message);
                        break;
                    case "finished":
                        assertEquals(1.0, number(progress), 0.0);
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
        }
    }
}
