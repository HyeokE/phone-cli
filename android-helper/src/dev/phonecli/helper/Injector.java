package dev.phonecli.helper;

import java.lang.reflect.Method;

import android.os.SystemClock;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

/**
 * Touches and keys through {@code InputManager.injectInputEvent}, the call `input` makes, without
 * starting a new `input` process for each one. Events are built as `input` builds them.
 */
final class Injector {
    /** InputManager.INJECT_INPUT_EVENT_MODE_WAIT_FOR_FINISH, as `input` uses. */
    private static final int WAIT_FOR_FINISH = 2;
    private static final long MOVE_EVERY_MS = 8;
    private static final long DOUBLE_TAP_GAP_MS = 90;

    private Object manager;
    private Method inject;
    private String error;
    private int injected;
    private int touchDevice = 0;

    Injector() {
        try {
            // Android 14+.
            Class<?> g = Class.forName("android.hardware.input.InputManagerGlobal");
            manager = g.getMethod("getInstance").invoke(null);
            inject = g.getMethod("injectInputEvent", InputEvent.class, int.class);
        } catch (Throwable t) {
            try {
                Class<?> c = Class.forName("android.hardware.input.InputManager");
                Method get = c.getDeclaredMethod("getInstance");
                get.setAccessible(true);
                manager = get.invoke(null);
                inject = c.getMethod("injectInputEvent", InputEvent.class, int.class);
            } catch (Throwable t2) {
                error = Util.describe(t2);
                manager = null;
                inject = null;
            }
        }
        try {
            for (int id : InputDevice.getDeviceIds()) {
                InputDevice d = InputDevice.getDevice(id);
                if (d != null && d.supportsSource(InputDevice.SOURCE_TOUCHSCREEN)) {
                    touchDevice = id;
                    break;
                }
            }
        } catch (Throwable ignored) {
            touchDevice = 0;
        }
    }

    boolean available() {
        return inject != null;
    }

    String error() {
        return error;
    }

    int injected() {
        return injected;
    }

    private void send(InputEvent e) throws Exception {
        if (inject == null) {
            throw new Fail("no_injection", "input injection is unavailable: " + error, false);
        }
        try {
            inject.invoke(manager, e, WAIT_FOR_FINISH);
        } catch (java.lang.reflect.InvocationTargetException ite) {
            Throwable cause = ite.getCause() == null ? ite : ite.getCause();
            throw new Fail("input_rejected", Util.describe(cause), injected > 0);
        }
        injected++;
    }

    private void motion(long down, long when, int action, float x, float y, float pressure) throws Exception {
        MotionEvent.PointerProperties[] props = {new MotionEvent.PointerProperties()};
        props[0].id = 0;
        props[0].toolType = MotionEvent.TOOL_TYPE_FINGER;
        MotionEvent.PointerCoords[] coords = {new MotionEvent.PointerCoords()};
        coords[0].x = x;
        coords[0].y = y;
        coords[0].pressure = pressure;
        coords[0].size = 1.0f;
        MotionEvent e = MotionEvent.obtain(down, when, action, 1, props, coords, 0, 0, 1.0f, 1.0f, touchDevice, 0,
                InputDevice.SOURCE_TOUCHSCREEN, 0);
        try {
            send(e);
        } finally {
            e.recycle();
        }
    }

    void tap(float x, float y, long holdMs, int count) throws Exception {
        for (int i = 0; i < Math.max(1, count); i++) {
            if (i > 0) {
                SystemClock.sleep(DOUBLE_TAP_GAP_MS);
            }
            long down = SystemClock.uptimeMillis();
            motion(down, down, MotionEvent.ACTION_DOWN, x, y, 1.0f);
            if (holdMs > 0) {
                SystemClock.sleep(holdMs);
            }
            motion(down, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, x, y, 0.0f);
        }
    }

    /**
     * A finger from (x1,y1) to (x2,y2) over {@code ms}. With {@code holdMs} it rests at the end before
     * lifting, so the list stops where the finger did; without, it lifts at speed (a fling).
     */
    void swipe(float x1, float y1, float x2, float y2, long ms, long holdMs) throws Exception {
        long down = SystemClock.uptimeMillis();
        long end = down + Math.max(1, ms);
        motion(down, down, MotionEvent.ACTION_DOWN, x1, y1, 1.0f);
        long now = SystemClock.uptimeMillis();
        while (now < end) {
            float a = (float) (now - down) / (float) (end - down);
            motion(down, now, MotionEvent.ACTION_MOVE, x1 + (x2 - x1) * a, y1 + (y2 - y1) * a, 1.0f);
            SystemClock.sleep(MOVE_EVERY_MS);
            now = SystemClock.uptimeMillis();
        }
        motion(down, SystemClock.uptimeMillis(), MotionEvent.ACTION_MOVE, x2, y2, 1.0f);
        if (holdMs > 0) {
            SystemClock.sleep(holdMs);
            motion(down, SystemClock.uptimeMillis(), MotionEvent.ACTION_MOVE, x2, y2, 1.0f);
        }
        motion(down, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, x2, y2, 0.0f);
    }

    private static int metaOf(int code) {
        switch (code) {
            case KeyEvent.KEYCODE_CTRL_LEFT:
                return KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_LEFT_ON;
            case KeyEvent.KEYCODE_CTRL_RIGHT:
                return KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_RIGHT_ON;
            case KeyEvent.KEYCODE_SHIFT_LEFT:
                return KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_LEFT_ON;
            case KeyEvent.KEYCODE_SHIFT_RIGHT:
                return KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_RIGHT_ON;
            case KeyEvent.KEYCODE_ALT_LEFT:
                return KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON;
            case KeyEvent.KEYCODE_ALT_RIGHT:
                return KeyEvent.META_ALT_ON | KeyEvent.META_ALT_RIGHT_ON;
            case KeyEvent.KEYCODE_META_LEFT:
                return KeyEvent.META_META_ON | KeyEvent.META_META_LEFT_ON;
            default:
                return 0;
        }
    }

    private void keyEvent(long down, int action, int code, int meta) throws Exception {
        long now = SystemClock.uptimeMillis();
        send(new KeyEvent(down, now, action, code, 0, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0,
                KeyEvent.FLAG_FROM_SYSTEM, InputDevice.SOURCE_KEYBOARD));
    }

    /** One key, with modifier keys held down around it. */
    void key(int code, int[] modifiers) throws Exception {
        long down = SystemClock.uptimeMillis();
        int meta = 0;
        for (int m : modifiers) {
            meta |= metaOf(m);
            keyEvent(down, KeyEvent.ACTION_DOWN, m, meta);
        }
        keyEvent(down, KeyEvent.ACTION_DOWN, code, meta);
        keyEvent(down, KeyEvent.ACTION_UP, code, meta);
        for (int i = modifiers.length - 1; i >= 0; i--) {
            meta &= ~metaOf(modifiers[i]);
            keyEvent(down, KeyEvent.ACTION_UP, modifiers[i], meta);
        }
    }

    /** Text as key presses on the virtual keyboard map, as `input text` does. Fails when a character has no key. */
    void text(String text) throws Exception {
        KeyCharacterMap kcm = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD);
        KeyEvent[] events = kcm.getEvents(text.toCharArray());
        if (events == null) {
            throw new Fail("unmappable_text", "some characters have no key on the virtual keyboard", false);
        }
        for (KeyEvent e : events) {
            KeyEvent ev = KeyEvent.changeTimeRepeat(e, SystemClock.uptimeMillis(), 0);
            ev.setSource(InputDevice.SOURCE_KEYBOARD);
            send(ev);
        }
    }
}
