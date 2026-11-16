package dev.phonecli.helper;

import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.concurrent.TimeoutException;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.UiAutomation;
import android.graphics.Point;
import android.graphics.Rect;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.SystemClock;
import android.view.Display;
import android.view.accessibility.AccessibilityNodeInfo;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * A UiAutomation connection made the way `uiautomator` makes its own (a {@code UiAutomationConnection}
 * handed to the hidden {@code UiAutomation} constructor), but kept open, so a reading is one walk of
 * the tree instead of a new VM, a new connection and a fixed one-second idle wait.
 */
final class Uia {
    /** UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES: the user's own services keep running. */
    private static final int DONT_SUPPRESS = 1;

    static final int F_CHECKABLE = 1;
    static final int F_CHECKED = 1 << 1;
    static final int F_CLICKABLE = 1 << 2;
    static final int F_ENABLED = 1 << 3;
    static final int F_FOCUSABLE = 1 << 4;
    static final int F_FOCUSED = 1 << 5;
    static final int F_SCROLLABLE = 1 << 6;
    static final int F_LONG_CLICKABLE = 1 << 7;
    static final int F_PASSWORD = 1 << 8;
    static final int F_SELECTED = 1 << 9;
    static final int F_EDITABLE = 1 << 10;
    static final int F_SHOWING_HINT = 1 << 11;

    final UiAutomation ua;

    private Uia(UiAutomation ua) {
        this.ua = ua;
    }

    static Uia connect() throws Exception {
        HandlerThread thread = new HandlerThread("phone-helper-uia");
        thread.setDaemon(true);
        thread.start();
        Class<?> connClass = Class.forName("android.app.UiAutomationConnection");
        Constructor<?> cc = connClass.getDeclaredConstructor();
        cc.setAccessible(true);
        Object conn = cc.newInstance();
        Class<?> iconn = Class.forName("android.app.IUiAutomationConnection");
        Constructor<UiAutomation> c = UiAutomation.class.getDeclaredConstructor(Looper.class, iconn);
        c.setAccessible(true);
        UiAutomation ua = c.newInstance(thread.getLooper(), conn);
        Method connect = UiAutomation.class.getDeclaredMethod("connect", int.class);
        connect.setAccessible(true);
        connect.invoke(ua, DONT_SUPPRESS);
        AccessibilityServiceInfo info = ua.getServiceInfo();
        if (info != null) {
            info.flags |= AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS
                    | AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS;
            ua.setServiceInfo(info);
        }
        return new Uia(ua);
    }

    /** The active window's tree, after the UI has been quiet for {@code idleMs} (at most {@code timeoutMs}). */
    JSONObject snapshot(long idleMs, long timeoutMs, int width, int height) throws Exception {
        long t0 = SystemClock.uptimeMillis();
        boolean idle = true;
        if (idleMs > 0) {
            try {
                ua.waitForIdle(idleMs, Math.max(idleMs, timeoutMs));
            } catch (TimeoutException e) {
                idle = false;
            }
        }
        long waited = SystemClock.uptimeMillis() - t0;
        AccessibilityNodeInfo root = null;
        for (int i = 0; i < 8 && root == null; i++) {
            root = rootInActiveWindow();
            if (root == null) {
                SystemClock.sleep(100);
            }
        }
        if (root == null) {
            throw new Fail("no_root", "no active window to read (screen off, locked or changing)", false);
        }
        Rect screen = appArea();
        if (screen == null && width > 0 && height > 0) {
            screen = new Rect(0, 0, width, height);
        }
        int[] count = new int[1];
        JSONObject tree = node(root, 0, screen, count, 0);
        JSONObject out = new JSONObject();
        CharSequence pkg = root.getPackageName();
        out.put("package", pkg == null ? JSONObject.NULL : pkg.toString());
        out.put("root", tree);
        out.put("nodes", count[0]);
        out.put("idle", idle);
        out.put("idle_wait_ms", waited);
        out.put("walk_ms", SystemClock.uptimeMillis() - t0 - waited);
        return out;
    }

    /**
     * The default display's app area ({@code Display.getSize}: the screen less the navigation bar),
     * which `uiautomator dump` clips bounds to; null when it cannot be had.
     */
    static Rect appArea() {
        try {
            Class<?> g = Class.forName("android.hardware.display.DisplayManagerGlobal");
            Object dmg = g.getMethod("getInstance").invoke(null);
            Display d = (Display) g.getMethod("getRealDisplay", int.class).invoke(dmg, Display.DEFAULT_DISPLAY);
            if (d == null) {
                return null;
            }
            Point p = new Point();
            d.getSize(p);
            return p.x > 0 && p.y > 0 ? new Rect(0, 0, p.x, p.y) : null;
        } catch (Throwable t) {
            return null;
        }
    }

    AccessibilityNodeInfo rootInActiveWindow() {
        return ua.getRootInActiveWindow();
    }

    /** The node with input focus, if any. */
    AccessibilityNodeInfo inputFocus() {
        try {
            return ua.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        } catch (Throwable t) {
            return null;
        }
    }

    private static JSONObject node(AccessibilityNodeInfo n, int index, Rect screen, int[] count, int depth)
            throws Exception {
        count[0]++;
        JSONObject o = new JSONObject();
        o.put("i", index);
        put(o, "t", n.getText());
        put(o, "d", n.getContentDescription());
        put(o, "r", n.getViewIdResourceName());
        put(o, "k", n.getClassName());
        put(o, "p", n.getPackageName());
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        if (screen != null && !r.intersect(screen)) {
            r.set(0, 0, 0, 0);
        }
        JSONArray b = new JSONArray();
        b.put(r.left).put(r.top).put(r.right).put(r.bottom);
        o.put("b", b);
        int f = 0;
        if (n.isCheckable()) f |= F_CHECKABLE;
        if (n.isChecked()) f |= F_CHECKED;
        if (n.isClickable()) f |= F_CLICKABLE;
        if (n.isEnabled()) f |= F_ENABLED;
        if (n.isFocusable()) f |= F_FOCUSABLE;
        if (n.isFocused()) f |= F_FOCUSED;
        if (n.isScrollable()) f |= F_SCROLLABLE;
        if (n.isLongClickable()) f |= F_LONG_CLICKABLE;
        if (n.isPassword()) f |= F_PASSWORD;
        if (n.isSelected()) f |= F_SELECTED;
        if (n.isEditable()) f |= F_EDITABLE;
        if (n.isShowingHintText()) f |= F_SHOWING_HINT;
        o.put("f", f);
        int kids = n.getChildCount();
        if (kids > 0 && depth < 200) {
            JSONArray c = new JSONArray();
            for (int i = 0; i < kids; i++) {
                AccessibilityNodeInfo child = n.getChild(i);
                if (child == null) {
                    continue;
                }
                // As `uiautomator dump`: what the user cannot see is left out.
                if (child.isVisibleToUser()) {
                    c.put(node(child, i, screen, count, depth + 1));
                }
            }
            if (c.length() > 0) {
                o.put("c", c);
            }
        }
        return o;
    }

    private static void put(JSONObject o, String k, CharSequence v) throws Exception {
        if (v != null && v.length() > 0) {
            o.put(k, v.toString());
        }
    }
}
