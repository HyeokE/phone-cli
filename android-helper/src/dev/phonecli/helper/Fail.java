package dev.phonecli.helper;

import org.json.JSONObject;

/** A refusal with a code. {@code sent} is true when something may already have reached the screen. */
final class Fail extends Exception {
    final String code;
    final boolean sent;
    /** Fields to add to the answer, or null. */
    JSONObject extra;

    Fail(String code, String message, boolean sent) {
        super(message);
        this.code = code;
        this.sent = sent;
    }

    Fail with(String key, Object value) {
        try {
            if (extra == null) {
                extra = new JSONObject();
            }
            extra.put(key, value);
        } catch (Exception ignored) {
            // a plain key and value cannot fail
        }
        return this;
    }
}
