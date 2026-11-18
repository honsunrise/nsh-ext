// The `nsh-ext.webview` native component: an owned android.webkit.WebView.
//
// The component reports its load lifecycle as `loadProgress` events whose
// payload is exactly nsh_ext::LoadProgress (`src/lib.rs`):
//   { navigation, state, progress, message }
// and mirrors nsh_ext::ProgressTracker's rules: one identity per top-level
// navigation, changed estimates as the engine gives them (only an unchanged
// value is deduped), and no success after a failure or a cancellation.
package com.honsunrise.nshext;

import android.content.Context;
import android.graphics.Bitmap;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewParent;
import android.webkit.RenderProcessGoneDetail;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import com.longbridge.nsh.NshComponent;
import com.longbridge.nsh.NshComponentContext;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Set;

/**
 * One browser for one app element: the view, the props and the load-progress events.
 *
 * <p>The component's view is a stable {@link FrameLayout} it owns. The browser is a
 * child of it, created for a source and released when the source changes: the
 * native instance is the isolation boundary. Every callback is checked against the
 * browser that produced it, so a released browser can never report into the next
 * one. No URL list, counter or progress value is used to guess whose callback it is,
 * and nothing a page did is ever replayed.
 *
 * <p>Android's WebView callbacks carry no navigation id, so identity comes from two
 * native facts. The browser instance is the isolation boundary: each source loads in
 * a browser of its own, and every callback is checked against the browser that is
 * current now, so a released browser can never report into the next one. Inside one
 * browser, the source's own load owns every main-frame report of that browser until
 * it ends (a redirect resolves within the load, and no other load exists in a fresh
 * browser); the page's own later navigations carry the URLs the engine reports, and a
 * completion or failure is accepted only for a URL the current page navigation owns.
 * A navigation that failed or was cancelled is never turned into a success afterwards.
 */
final class WebViewComponent implements NshComponent {
    private static final String TAG = "nsh-ext";
    /** The largest navigation identity JavaScript represents exactly. */
    private static final long MAX_NAVIGATION = 9_007_199_254_740_991L;
    private static final String STARTED = "started";
    private static final String LOADING = "loading";
    private static final String FINISHED = "finished";
    private static final String FAILED = "failed";
    private static final String CANCELLED = "cancelled";

    /** Where the component's events go: the SDK context in an app, a recorder in a test. */
    interface Sink {
        void emit(String event, Object payload);
    }

    private final Context context;
    private final Sink sink;
    /** Runs the deferred source apply, after the mount batch that brought the props. */
    private final Handler main = new Handler(Looper.getMainLooper());
    private final Runnable applySource = this::applySource;

    /** The view the SDK box holds. It never changes; the browser child may. */
    private final FrameLayout root;
    /** The browser of the current source, or null while no source is loaded. */
    private WebView web;
    private boolean disposed;

    /** The source the props last set; empty means stopped and cleared. */
    private String source = "";
    private boolean sourceScheduled;
    /** The app asked for load-progress events. Nothing is built or sent while false. */
    private boolean trackProgress;
    /**
     * No navigation of the current browser is this component's: no source yet, a
     * stopped or rejected source, a failure, or a lost renderer. Only a later source
     * change starts one again; the page's own callbacks never resurrect it.
     */
    private boolean retired = true;

    // The navigation state of the current browser, mirroring nsh_ext::ProgressTracker.

    /** The current navigation identity; one per top-level navigation of this component. */
    private long navigation;
    private boolean active;
    /** The source's load has not been confirmed by a native callback yet. */
    private boolean pendingStart;
    /**
     * The current navigation is the source's own load, in the browser made for it.
     * Until it ends, every main-frame report of that browser is its own: a redirect
     * resolves within the load, and no other load can exist in a fresh browser.
     */
    private boolean sourceLoad;
    /** The URLs native callbacks proved belong to the current navigation. */
    private final Set<String> owned = new LinkedHashSet<>();
    /** The last estimate handed to the app, for deduplication. */
    private Double lastEstimate;

    /**
     * The component the SDK factory makes.
     *
     * @param context the SDK context of the element this component belongs to
     */
    WebViewComponent(NshComponentContext context) {
        this(context.getContext(), context::emit);
    }

    /** The component with an explicit context and event sink (tests). */
    WebViewComponent(Context context, Sink sink) {
        this.context = context;
        this.sink = sink;
        root = new FrameLayout(context);
        // No browser until a source is set: an element without one is an empty box.
    }

    @Override
    public View view() {
        return root;
    }

    /**
     * {@inheritDoc}
     *
     * <p>{@code src} loads a URL in a browser of its own; changing it releases the
     * previous browser (cancelling its navigation) before the new source is loaded
     * or rejected. {@code trackProgress} enables the {@code loadProgress} events.
     * The first source load waits one main-thread turn, so every prop of the mount
     * batch (in particular {@code trackProgress}) is installed first.
     */
    @Override
    public void setProp(String name, Object value) {
        if (disposed) return;
        if ("src".equals(name)) {
            String next = value instanceof String ? (String) value : "";
            // A prop that did not change is not a load: the contract has no reload,
            // and NSH hands a component each prop when it changes.
            if (next.equals(source)) return;
            source = next;
            scheduleSource();
        } else if ("trackProgress".equals(name)) {
            // A gate, and nothing else: enabling it later neither replays nor
            // relabels events of a navigation that started while it was off.
            trackProgress = Boolean.TRUE.equals(value);
        }
    }

    /** The element is gone: stop the load and release the browser. Silent, like the SDK's own views. */
    @Override
    public void destroy() {
        if (disposed) return;
        disposed = true;
        main.removeCallbacks(applySource);
        sourceScheduled = false;
        active = false;
        pendingStart = false;
        sourceLoad = false;
        retired = true;
        owned.clear();
        retireBrowser();
        ViewParent parent = root.getParent();
        if (parent instanceof ViewGroup) ((ViewGroup) parent).removeView(root);
    }

    private void scheduleSource() {
        if (sourceScheduled) return;
        sourceScheduled = true;
        main.post(applySource);
    }

    /** Applies the source the props installed, once the mount batch is over. */
    private void applySource() {
        sourceScheduled = false;
        if (disposed) return;
        String url = source;
        // The old page's navigation is cancelled and its browser released before the
        // new source is judged: nothing of it can keep running, and nothing it reports
        // later can reach this component (its callbacks fail the instance check).
        cancel();
        retireBrowser();
        retired = true;
        if (url.isEmpty()) {
            // An empty source leaves an empty root: the page is stopped and cleared.
            return;
        }
        String scheme = unsupportedScheme(url);
        if (scheme != null) {
            begin(url, true);
            fail(scheme.isEmpty()
                    ? "the WebView cannot load \"" + url + "\": no URL scheme"
                    : "the WebView cannot load the \"" + scheme + "\" URL scheme");
            return;
        }
        if (!createBrowser()) {
            begin(url, true);
            fail("WebView is not available on this device");
            return;
        }
        begin(url, true);
        load(url);
    }

    /**
     * The scheme of {@code url} when the WebView cannot start a navigation to it, or
     * null when it can. An empty string means the URL has no scheme at all. A URL with
     * a scheme the WebView cannot navigate to (a bare host name, {@code javascript:},
     * {@code tel:}, …) would otherwise leave a navigation that never completes, or
     * run script instead of loading a page.
     */
    private static String unsupportedScheme(String url) {
        String scheme = android.net.Uri.parse(url).getScheme();
        if (scheme == null) return "";
        switch (scheme.toLowerCase(java.util.Locale.ROOT)) {
            case "http":
            case "https":
            case "file":
            case "content":
            case "data":
            case "about":
            case "blob":
                return null;
            default:
                return scheme;
        }
    }

    /** Puts a browser in the root. */
    private boolean createBrowser() {
        if (web != null) return true;
        WebView created;
        try {
            created = newWebView();
        } catch (RuntimeException error) {
            Log.w(TAG, "WebView is not available on this device: " + error);
            return false;
        }
        web = created;
        root.addView(created, new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        return true;
    }

    private WebView newWebView() {
        WebView made = new WebView(context);
        WebSettings settings = made.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        // Pages are untrusted content: they get no access to the app's files.
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setMediaPlaybackRequiresUserGesture(true);
        made.setWebViewClient(new Client(made));
        made.setWebChromeClient(new Chrome(made));
        return made;
    }

    /** Stops and releases the current browser; its callbacks are ignored from here on. */
    private void retireBrowser() {
        WebView gone = web;
        if (gone == null) return;
        // First: a callback delivered during teardown is no longer this component's.
        web = null;
        root.removeView(gone);
        gone.stopLoading();
        gone.destroy();
    }

    /** Starts a load, or reports why the engine refused it. */
    private void load(String url) {
        WebView target = web;
        if (target == null) {
            fail("the WebView is not available");
            return;
        }
        try {
            target.loadUrl(url);
        } catch (RuntimeException error) {
            fail("the WebView could not load " + url + ": " + error.getMessage());
            // A browser that refused to start the load is not usable for it.
            retireBrowser();
        }
    }

    // Navigation identity: the same rules as nsh_ext::ProgressTracker.

    /**
     * Begins one top-level navigation of the current browser. The caller has already
     * cancelled the previous one, so a completed navigation can never come back.
     *
     * @param url what is loading
     * @param hostInitiated the source prop started it (the engine confirms it later)
     */
    private void begin(String url, boolean hostInitiated) {
        if (navigation >= MAX_NAVIGATION) {
            throw new IllegalStateException("navigation identities exhausted");
        }
        navigation += 1;
        active = true;
        pendingStart = hostInitiated;
        sourceLoad = hostInitiated;
        retired = false;
        owned.clear();
        own(url);
        lastEstimate = null;
        emit(STARTED, null, null);
    }

    /** Records a URL a native callback gave for the current navigation. */
    private void own(String url) {
        if (url != null) owned.add(url);
    }

    private boolean owns(String url) {
        return url != null && owned.contains(url);
    }

    /** Confirmed completion: the engine finished this navigation's document. */
    private void finish() {
        if (!active) return;
        active = false;
        pendingStart = false;
        sourceLoad = false;
        emit(FINISHED, 1.0, null);
    }

    /**
     * A genuine native failure. It is final for the current browser: the engine's
     * error page is not a new navigation, and only a later source change starts one
     * again (in a browser of its own).
     */
    private void fail(String message) {
        if (!active) return;
        active = false;
        pendingStart = false;
        sourceLoad = false;
        retired = true;
        emit(FAILED, null, message);
    }

    /** The host stopped or replaced the current navigation. */
    private void cancel() {
        if (!active) return;
        active = false;
        pendingStart = false;
        sourceLoad = false;
        owned.clear();
        emit(CANCELLED, null, null);
    }

    /**
     * Sends one event. While {@code trackProgress} is false nothing is allocated or
     * sent; the load itself is still followed, so the events after a later
     * subscription are the engine's own.
     */
    private void emit(String state, Double progress, String message) {
        if (!trackProgress || disposed) return;
        Map<String, Object> payload = new LinkedHashMap<>(4);
        payload.put("navigation", navigation);
        payload.put("state", state);
        payload.put("progress", progress);
        payload.put("message", message);
        sink.emit(WebViewExtension.PROGRESS_EVENT, payload);
    }

    /** Whether a callback came from a browser this component no longer uses. */
    private boolean unavailable(WebView callbackView, WebView owner) {
        return disposed || callbackView != owner || owner != web;
    }

    /** The page lifecycle of one browser: its source's load, owned navigations and failures. */
    private final class Client extends WebViewClient {
        private final WebView owner;

        Client(WebView owner) {
            this.owner = owner;
        }

        @Override
        public boolean shouldOverrideUrlLoading(WebView callbackView, WebResourceRequest request) {
            if (unavailable(callbackView, owner) || request == null || !request.isForMainFrame()) {
                return false;
            }
            if (active && request.isRedirect()) {
                // A server-side redirect of the load this browser is running: native
                // evidence that the redirect target belongs to the same navigation.
                own(request.getUrl() == null ? null : request.getUrl().toString());
            }
            return false;
        }

        @Override
        public void onPageStarted(WebView callbackView, String url, Bitmap favicon) {
            if (unavailable(callbackView, owner) || url == null) return;
            if (!active) {
                if (retired) return;
                // A top-level navigation the page started: a link or a history move.
                begin(url, false);
                return;
            }
            if (owns(url)) {
                // This navigation's own start, or a hop it already owns.
                pendingStart = false;
                return;
            }
            if (pendingStart) {
                // The engine's first report of the source's own load. A redirect can
                // resolve to another URL before its document starts, so this start is
                // that load's, whatever the source prop said.
                own(url);
                pendingStart = false;
                return;
            }
            if (sourceLoad) {
                // A further start while the source's own load is in flight: a redirect
                // hop the engine reported as a start. That browser was made for this
                // source, so no other load can exist in it.
                own(url);
                return;
            }
            // A different document started: the page's own navigation, not a hop of
            // the load in flight.
            cancel();
            begin(url, false);
        }

        @Override
        public void onPageFinished(WebView callbackView, String url) {
            if (unavailable(callbackView, owner) || !active || url == null) return;
            // A document this navigation does not own; the source's own load and its
            // redirects are its browser's only load, so any of its completions is this
            // navigation's.
            if (!owns(url) && !sourceLoad) return;
            finish();
        }

        @Override
        public void onReceivedError(WebView callbackView, WebResourceRequest request,
                WebResourceError error) {
            if (unavailable(callbackView, owner) || request == null
                    || !request.isForMainFrame() || !active) {
                return;
            }
            String url = request.getUrl() == null ? null : request.getUrl().toString();
            // The source's own failed load and its redirects are its browser's only
            // load; another document's failure is not this navigation's.
            if (!sourceLoad && !owns(url)) return;
            fail(describe(error));
        }

        @Override
        public boolean onRenderProcessGone(WebView callbackView, RenderProcessGoneDetail detail) {
            if (disposed || callbackView != owner || owner != web) return true;
            // The crashed browser cannot be reused. Its load fails (there is no
            // navigation state to report when it dies idle), and it is released: only
            // an explicit source change makes a new browser, so nothing reloads itself.
            fail(detail != null && detail.didCrash()
                    ? "the web renderer process crashed"
                    : "the web renderer process was killed");
            retired = true;
            retireBrowser();
            return true;
        }
    }

    /** The engine's own estimates for the current navigation. */
    private final class Chrome extends WebChromeClient {
        private final WebView owner;

        Chrome(WebView owner) {
            this.owner = owner;
        }

        @Override
        public void onProgressChanged(WebView callbackView, int newProgress) {
            if (unavailable(callbackView, owner) || !trackProgress || !active || pendingStart) {
                return;
            }
            if (newProgress < 0 || newProgress > 100) return;
            double fraction = newProgress / 100.0;
            // The engine's estimate as it gave it. Only an unchanged value is
            // deduplicated, as nsh_ext::ProgressTracker does; a changed estimate is
            // reported even when it is lower than the previous one.
            if (lastEstimate != null && lastEstimate.doubleValue() == fraction) return;
            lastEstimate = fraction;
            emit(LOADING, fraction, null);
        }
    }

    /** A failure's diagnostic: the engine's description and error code. */
    private static String describe(WebResourceError error) {
        CharSequence description = error == null ? null : error.getDescription();
        String text = description == null ? "" : description.toString().trim();
        int code = error == null ? 0 : error.getErrorCode();
        if (text.isEmpty()) return "navigation failed (error " + code + ")";
        return text + " (error " + code + ")";
    }
}
