package org.pengudroid.simpleactivity;

import android.app.Activity;
import android.graphics.Color;
import android.os.Bundle;
import android.util.Log;
import android.view.Gravity;
import android.widget.TextView;

/** A dependency-free Java Activity used to isolate ART and HWUI bring-up. */
public final class MainActivity extends Activity {
    private static final String TAG = "PenguDroidFixture";
    private TextView textView;
    private int frameUpdate;

    private final Runnable frameUpdater = new Runnable() {
        @Override
        public void run() {
            if (textView == null || frameUpdate >= 3) {
                return;
            }
            frameUpdate++;
            textView.setText("PenguDroid EGL frame update " + frameUpdate);
            textView.setBackgroundColor(Color.rgb(24 + frameUpdate * 24, 64, 112));
            Log.i(TAG, "SimpleActivity frame update " + frameUpdate);
            textView.postDelayed(this, 250L);
        }
    };

    static {
        Log.i(TAG, "SimpleActivity class initialized");
    }

    public MainActivity() {
        super();
        Log.i(TAG, "SimpleActivity constructor");
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        Log.i(TAG, "SimpleActivity onCreate");
        TextView view = new TextView(this);
        textView = view;
        view.setText("PenguDroid AArch64: Java Activity works");
        view.setTextSize(22.0f);
        view.setTextColor(Color.WHITE);
        view.setBackgroundColor(Color.rgb(24, 64, 112));
        view.setGravity(Gravity.CENTER);
        setContentView(view);
        Log.i(TAG, "SimpleActivity content view installed");
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "SimpleActivity onResume");
        if (textView != null && frameUpdate == 0) {
            textView.postDelayed(frameUpdater, 250L);
        }
    }

    @Override
    protected void onPause() {
        if (textView != null) {
            textView.removeCallbacks(frameUpdater);
        }
        super.onPause();
    }

    @Override
    public void onAttachedToWindow() {
        super.onAttachedToWindow();
        Log.i(TAG, "SimpleActivity attached to window");
    }
}
