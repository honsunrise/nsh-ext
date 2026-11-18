package com.honsunrise.nshext.example;

import android.app.Activity;
import android.graphics.Color;
import android.os.Bundle;
import android.util.Log;
import android.view.Gravity;
import android.widget.FrameLayout;
import android.widget.TextView;
import com.honsunrise.nshext.WebViewExtension;
import com.longbridge.nsh.NshView;

/**
 * A host app that runs the TSX app in {@code android/example/App.tsx} and
 * registers the extension's {@code nsh-ext.webview} component on its view.
 *
 * <p>The bundle is not checked in (it is compiled output): build it once with
 * {@code nsh build android/example --release --out
 * android/example/src/main/assets/nsh}. Without it the screen says so instead
 * of showing a blank view.
 */
public final class MainActivity extends Activity {
    private static final String TAG = "nsh-ext-example";

    private NshView nsh;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        FrameLayout root = new FrameLayout(this);
        nsh = new NshView(this);
        nsh.setErrorListener(message -> Log.e(TAG, message));
        // Before the bundle loads: the app creates its elements as it starts.
        WebViewExtension.register(nsh);
        root.addView(nsh, matchParent());

        if (!nsh.loadAssetBundle("nsh")) {
            TextView hint = new TextView(this);
            hint.setText("Build the example app first:\n\nnsh build android/example --release "
                    + "--out android/example/src/main/assets/nsh");
            hint.setTextColor(Color.DKGRAY);
            hint.setGravity(Gravity.CENTER);
            hint.setPadding(48, 48, 48, 48);
            root.addView(hint, matchParent());
        }
        setContentView(root);
    }

    @Override
    protected void onDestroy() {
        if (nsh != null) {
            nsh.destroy();
            nsh = null;
        }
        super.onDestroy();
    }

    private static FrameLayout.LayoutParams matchParent() {
        return new FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT);
    }
}
