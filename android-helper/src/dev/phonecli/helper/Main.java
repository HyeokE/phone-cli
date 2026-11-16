package dev.phonecli.helper;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;

import android.os.Build;
import android.os.Looper;
import android.os.SystemClock;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * phone-cli's helper on an Android phone.
 *
 * <p>Pushed to /data/local/tmp and started by the phone-cli daemon with
 * {@code CLASSPATH=/data/local/tmp/phone-helper.jar app_process / --nice-name=phone-helper
 * dev.phonecli.helper.Main}, as scrcpy starts its server. It is not an installed app, runs with
 * the shell's permissions and goes away with the adb connection (it exits when stdin closes).
 *
 * <p>Protocol over stdin/stdout: frames of {@code 'P' 'H' <u32 big-endian length> <UTF-8 JSON>}.
 * The helper speaks first with a hello frame ({@code {"hello":"phone-helper","version":N,...}});
 * then every request {@code {"id":N,"op":"..."}} gets exactly one answer {@code {"id":N,"ok":...}}.
 * Anything else the runtime prints goes to stderr, never stdout.
 */
public final class Main {
    /** Bumped whenever a request or answer changes shape. phone-cli checks it in the hello. */
    public static final int VERSION = 1;

    private static final int MAX_FRAME = 16 << 20;

    private Main() {}

    public static void main(String[] args) {
        OutputStream raw = new BufferedOutputStream(new FileOutputStream(FileDescriptor.out), 1 << 16);
        // Libraries that print must not corrupt the protocol.
        PrintStream err = new PrintStream(new FileOutputStream(FileDescriptor.err), true);
        System.setOut(err);
        try {
            if (Looper.getMainLooper() == null) {
                Looper.prepareMainLooper();
            }
        } catch (Throwable ignored) {
            // Only some services want a main looper; none of ours needs it to run.
        }
        DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(FileDescriptor.in), 1 << 16));
        int code = 0;
        try {
            new Main().serve(in, raw);
        } catch (Throwable t) {
            t.printStackTrace(err);
            code = 1;
        }
        System.exit(code);
    }

    private Uia uia;
    private String uiaError;
    private final Injector injector = new Injector();

    private void serve(DataInputStream in, OutputStream out) throws Exception {
        long t0 = SystemClock.uptimeMillis();
        try {
            uia = Uia.connect();
        } catch (Throwable t) {
            uiaError = Util.describe(t);
        }
        JSONObject hello = new JSONObject();
        hello.put("hello", "phone-helper");
        hello.put("version", VERSION);
        hello.put("sdk", Build.VERSION.SDK_INT);
        hello.put("pid", android.os.Process.myPid());
        JSONObject caps = new JSONObject();
        caps.put("snapshot", uia != null);
        caps.put("inject", injector.available());
        caps.put("clipboard", Clip.available());
        caps.put("labels", true);
        hello.put("caps", caps);
        if (uiaError != null) {
            hello.put("uia_error", uiaError);
        }
        if (injector.error() != null) {
            hello.put("inject_error", injector.error());
        }
        if (Clip.error() != null) {
            hello.put("clipboard_error", Clip.error());
        }
        hello.put("start_ms", SystemClock.uptimeMillis() - t0);
        write(out, hello);

        while (true) {
            int a = in.read();
            if (a < 0) {
                return; // phone-cli went away
            }
            int b = in.read();
            if (a != 'P' || b != 'H') {
                throw new IOException("bad frame magic");
            }
            int len = in.readInt();
            if (len < 0 || len > MAX_FRAME) {
                throw new IOException("bad frame length " + len);
            }
            byte[] buf = new byte[len];
            in.readFully(buf);
            JSONObject req = new JSONObject(new String(buf, StandardCharsets.UTF_8));
            String op = req.optString("op");
            long started = SystemClock.uptimeMillis();
            int injectedBefore = injector.injected();
            JSONObject resp;
            try {
                resp = handle(op, req);
                resp.put("ok", true);
            } catch (Fail f) {
                resp = f.extra == null ? new JSONObject() : f.extra;
                resp.put("ok", false);
                resp.put("code", f.code);
                resp.put("error", f.getMessage());
                resp.put("sent", f.sent || injector.injected() > injectedBefore);
            } catch (Throwable t) {
                resp = new JSONObject();
                resp.put("ok", false);
                resp.put("code", "helper_error");
                resp.put("error", Util.describe(t));
                resp.put("sent", injector.injected() > injectedBefore);
            }
            resp.put("id", req.optLong("id"));
            resp.put("ms", SystemClock.uptimeMillis() - started);
            write(out, resp);
            if ("quit".equals(op)) {
                return;
            }
        }
    }

    private JSONObject handle(String op, JSONObject req) throws Exception {
        switch (op) {
            case "ping":
            case "quit":
                return new JSONObject();
            case "snapshot":
                return needUia().snapshot(req.optLong("idle_ms", 200), req.optLong("timeout_ms", 3000),
                        req.optInt("width", 0), req.optInt("height", 0));
            case "tap":
                injector.tap((float) req.getDouble("x"), (float) req.getDouble("y"), req.optLong("hold_ms", 0),
                        req.optInt("count", 1));
                return new JSONObject();
            case "taps": {
                JSONArray pts = req.getJSONArray("points");
                for (int i = 0; i < pts.length(); i++) {
                    JSONArray p = pts.getJSONArray(i);
                    injector.tap((float) p.getDouble(0), (float) p.getDouble(1), 0, 1);
                }
                return new JSONObject();
            }
            case "swipe":
                injector.swipe((float) req.getDouble("x1"), (float) req.getDouble("y1"), (float) req.getDouble("x2"),
                        (float) req.getDouble("y2"), req.optLong("ms", 250), req.optLong("hold_ms", 0));
                return new JSONObject();
            case "key": {
                JSONArray mods = req.optJSONArray("modifiers");
                int[] m = new int[mods == null ? 0 : mods.length()];
                for (int i = 0; i < m.length; i++) {
                    m[i] = mods.getInt(i);
                }
                injector.key(req.getInt("code"), m);
                return new JSONObject();
            }
            case "keys": {
                JSONArray codes = req.getJSONArray("codes");
                for (int i = 0; i < codes.length(); i++) {
                    injector.key(codes.getInt(i), new int[0]);
                }
                return new JSONObject();
            }
            case "text":
                injector.text(req.getString("text"));
                return new JSONObject();
            case "paste":
                return Clip.paste(req.getString("text"), req.optLong("wait_ms", 800), uia, injector);
            case "labels":
                return Labels.labels(req.getJSONArray("packages"), req.optJSONObject("activities"));
            case "pid":
                return Util.pidOf(req.getString("package"));
            default:
                throw new Fail("unknown_op", "unknown op `" + op + "`", false);
        }
    }

    private Uia needUia() throws Fail {
        if (uia == null) {
            throw new Fail("no_uiautomation", "UiAutomation is not connected: " + uiaError, false);
        }
        return uia;
    }

    private static void write(OutputStream out, JSONObject o) throws IOException {
        byte[] body = o.toString().getBytes(StandardCharsets.UTF_8);
        int n = body.length;
        out.write('P');
        out.write('H');
        out.write((n >>> 24) & 0xff);
        out.write((n >>> 16) & 0xff);
        out.write((n >>> 8) & 0xff);
        out.write(n & 0xff);
        out.write(body);
        out.flush();
    }
}
