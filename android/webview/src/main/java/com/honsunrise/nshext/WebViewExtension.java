// The public entry point hosts embed: register the extension on an NshView.
package com.honsunrise.nshext;

import com.longbridge.nsh.NshView;

/**
 * Registers the {@code nsh-ext.webview} component on one Native Shell view.
 *
 * <p>The app declares the component with {@code native} (Rust) or
 * {@code nativeComponent} (TSX) under {@link #COMPONENT_NAME}; the Rust wrapper is
 * {@code nsh_ext::web_view()}. The component owns a stable view and the
 * {@link android.webkit.WebView} it makes for the app's source, and reports the
 * app's loads as {@link #PROGRESS_EVENT} events with the
 * {@code nsh_ext::LoadProgress} payload.
 *
 * <p>Call {@link #register(NshView)} before loading the app's bundle. Registration
 * is per view — every {@code NshView} has its own components — and registering the
 * same name twice replaces the earlier factory.
 *
 * <p><b>The component is trusted host code.</b> The URLs the app can load are
 * decided by the host app, not by this library: NSH's capability grants do not
 * restrict a registered native component's WebView.
 */
public final class WebViewExtension {
    /** The component name; the same string as {@code nsh_ext::COMPONENT_NAME}. */
    public static final String COMPONENT_NAME = "nsh-ext.webview";

    /** The load-progress event; the same string as {@code nsh_ext::PROGRESS_EVENT}. */
    public static final String PROGRESS_EVENT = "loadProgress";

    private WebViewExtension() {}

    /**
     * Registers the component on {@code view} for every element the app creates from now on.
     *
     * @param view the view the app runs in
     * @throws NullPointerException if {@code view} is null
     */
    public static void register(NshView view) {
        if (view == null) throw new NullPointerException("view");
        view.registerComponent(COMPONENT_NAME, WebViewComponent::new);
    }
}
