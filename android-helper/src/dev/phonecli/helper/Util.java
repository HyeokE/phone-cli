package dev.phonecli.helper;

import java.io.File;
import java.io.FileInputStream;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;

import org.json.JSONObject;

final class Util {
    private Util() {}

    static String describe(Throwable t) {
        while (t instanceof InvocationTargetException && t.getCause() != null) {
            t = t.getCause();
        }
        String m = t.getMessage();
        return t.getClass().getName() + (m == null ? "" : ": " + m);
    }

    /** The first method of {@code c} named {@code name} with {@code params} parameters. */
    static Method method(Class<?> c, String name, int params) throws NoSuchMethodException {
        for (Method m : c.getMethods()) {
            if (m.getName().equals(name) && m.getParameterTypes().length == params) {
                return m;
            }
        }
        for (Method m : c.getDeclaredMethods()) {
            if (m.getName().equals(name) && m.getParameterTypes().length == params) {
                m.setAccessible(true);
                return m;
            }
        }
        throw new NoSuchMethodException(c.getName() + "." + name + "/" + params);
    }

    /** Arguments for a binder method by parameter type: the package for the first String, null for others, 0 for ints. */
    static Object[] binderArgs(Method m, Object first, String pkg) {
        Class<?>[] types = m.getParameterTypes();
        Object[] args = new Object[types.length];
        boolean stringUsed = false;
        boolean firstUsed = first == null;
        for (int i = 0; i < types.length; i++) {
            Class<?> t = types[i];
            if (!firstUsed && t.isInstance(first)) {
                args[i] = first;
                firstUsed = true;
            } else if (t == String.class) {
                args[i] = stringUsed ? null : pkg;
                stringUsed = true;
            } else if (t == int.class) {
                args[i] = 0;
            } else if (t == long.class) {
                args[i] = 0L;
            } else if (t == boolean.class) {
                args[i] = false;
            } else {
                args[i] = null;
            }
        }
        return args;
    }

    /** The pid of the process whose command line is {@code pkg}, from /proc. */
    static JSONObject pidOf(String pkg) throws Exception {
        JSONObject out = new JSONObject();
        File[] procs = new File("/proc").listFiles();
        if (procs != null) {
            byte[] buf = new byte[256];
            for (File p : procs) {
                String name = p.getName();
                if (name.isEmpty() || !Character.isDigit(name.charAt(0))) {
                    continue;
                }
                try (FileInputStream in = new FileInputStream(new File(p, "cmdline"))) {
                    int n = in.read(buf);
                    if (n <= 0) {
                        continue;
                    }
                    int end = 0;
                    while (end < n && buf[end] != 0) {
                        end++;
                    }
                    if (new String(buf, 0, end, StandardCharsets.UTF_8).equals(pkg)) {
                        out.put("pid", Long.parseLong(name));
                        return out;
                    }
                } catch (Exception ignored) {
                    // gone, or not ours to read
                }
            }
        }
        out.put("pid", JSONObject.NULL);
        return out;
    }
}
