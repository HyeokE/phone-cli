package dev.phonecli.helper;

import java.lang.reflect.Method;

import android.content.ClipData;
import android.os.IBinder;
import android.os.SystemClock;
import android.view.KeyEvent;
import android.view.accessibility.AccessibilityNodeInfo;

import org.json.JSONObject;

/**
 * The clipboard service as the shell (scrcpy's approach: {@code IClipboard} from the service manager),
 * for pasting text `input text` cannot type (Korean, emoji, accents). The user's clipboard is read
 * first and put back afterwards; its contents never leave the phone.
 */
final class Clip {
    private static final String SHELL = "com.android.shell";

    private static Object service;
    private static Method set;
    private static Method get;
    private static Method clear;
    private static String error;

    static {
        try {
            Class<?> sm = Class.forName("android.os.ServiceManager");
            IBinder binder = (IBinder) sm.getMethod("getService", String.class).invoke(null, "clipboard");
            if (binder == null) {
                throw new IllegalStateException("no clipboard service");
            }
            Class<?> stub = Class.forName("android.content.IClipboard$Stub");
            service = stub.getMethod("asInterface", IBinder.class).invoke(null, binder);
            Class<?> iface = Class.forName("android.content.IClipboard");
            set = first(iface, "setPrimaryClip");
            get = first(iface, "getPrimaryClip");
            clear = first(iface, "clearPrimaryClip");
        } catch (Throwable t) {
            error = Util.describe(t);
            service = null;
        }
    }

    private Clip() {}

    private static Method first(Class<?> c, String name) {
        for (Method m : c.getMethods()) {
            if (m.getName().equals(name)) {
                return m;
            }
        }
        return null;
    }

    static boolean available() {
        return service != null && set != null;
    }

    static String error() {
        return error;
    }

    private static void setClip(ClipData clip) throws Exception {
        set.invoke(service, Util.binderArgs(set, clip, SHELL));
    }

    private static ClipData getClip() throws Exception {
        return (ClipData) get.invoke(service, Util.binderArgs(get, null, SHELL));
    }

    private static void clearClip() throws Exception {
        clear.invoke(service, Util.binderArgs(clear, null, SHELL));
    }

    private static String textOf(AccessibilityNodeInfo n) {
        if (n == null) {
            return null;
        }
        n.refresh();
        CharSequence t = n.getText();
        return t == null ? "" : t.toString();
    }

    /**
     * Put {@code text} on the clipboard, paste it into the focused field, wait (up to {@code waitMs})
     * for the field to change, then put the user's clipboard back.
     */
    static JSONObject paste(String text, long waitMs, Uia uia, Injector injector) throws Exception {
        if (!available()) {
            throw new Fail("no_clipboard", "the clipboard service is unavailable: " + error, false);
        }
        ClipData saved = null;
        boolean read = false;
        if (get != null) {
            try {
                saved = getClip();
                read = true;
            } catch (Throwable t) {
                read = false;
            }
        }
        try {
            setClip(ClipData.newPlainText("phone-cli", text));
        } catch (Throwable t) {
            throw new Fail("clipboard_failed", "could not set the clipboard: " + Util.describe(t), false);
        }
        String method = "key_paste";
        AccessibilityNodeInfo field = uia == null ? null : uia.inputFocus();
        String before = textOf(field);
        Throwable pasteError = null;
        try {
            boolean done = false;
            if (field != null) {
                done = field.performAction(AccessibilityNodeInfo.ACTION_PASTE);
                if (done) {
                    method = "accessibility_paste";
                }
            }
            if (!done) {
                injector.key(KeyEvent.KEYCODE_PASTE, new int[0]);
            }
            long end = SystemClock.uptimeMillis() + Math.max(0, waitMs);
            if (field != null) {
                while (SystemClock.uptimeMillis() < end) {
                    String now = textOf(field);
                    if (now != null && !now.equals(before)) {
                        break;
                    }
                    SystemClock.sleep(40);
                }
            } else {
                SystemClock.sleep(Math.min(waitMs, 400));
            }
        } catch (Throwable t) {
            pasteError = t;
        }
        boolean restored = false;
        if (read) {
            try {
                if (saved == null && clear != null) {
                    clearClip();
                } else if (saved != null) {
                    setClip(saved);
                }
                restored = saved != null || clear != null;
            } catch (Throwable t) {
                restored = false;
            }
        }
        if (pasteError != null) {
            throw new Fail("paste_failed", Util.describe(pasteError), true).with("clipboard_restored", restored);
        }
        JSONObject out = new JSONObject();
        out.put("method", method);
        out.put("clipboard_saved", read);
        out.put("clipboard_restored", restored);
        return out;
    }
}
