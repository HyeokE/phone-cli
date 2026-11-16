package dev.phonecli.helper;

import java.lang.reflect.Method;

import android.content.ComponentName;
import android.content.pm.ActivityInfo;
import android.content.pm.ApplicationInfo;
import android.content.res.AssetManager;
import android.content.res.Resources;

import org.json.JSONArray;
import org.json.JSONObject;

/**
 * App names as the phone shows them, in its language: the launcher activity's label when phone-cli
 * names one (that is what the home screen shows), else the application's. There is no Context here,
 * so the package manager is the binder one and resources come from an AssetManager opened on the APK.
 */
final class Labels {
    private Labels() {}

    static JSONObject labels(JSONArray packages, JSONObject activities) throws Exception {
        Class<?> at = Class.forName("android.app.ActivityThread");
        Object pm = at.getMethod("getPackageManager").invoke(null);
        Method appInfo = Util.method(pm.getClass(), "getApplicationInfo", 3);
        Method activityInfo = null;
        try {
            activityInfo = Util.method(pm.getClass(), "getActivityInfo", 3);
        } catch (NoSuchMethodException ignored) {
            // application labels only
        }
        Method addAssetPath = AssetManager.class.getDeclaredMethod("addAssetPath", String.class);
        addAssetPath.setAccessible(true);
        JSONObject labels = new JSONObject();
        JSONObject errors = new JSONObject();
        for (int i = 0; i < packages.length(); i++) {
            String pkg = packages.getString(i);
            try {
                CharSequence label = null;
                String activity = activities == null ? null : activities.optString(pkg, null);
                if (activityInfo != null && activity != null && !activity.isEmpty()) {
                    ComponentName cn = ComponentName.unflattenFromString(pkg + "/" + activity);
                    ActivityInfo ai = cn == null ? null
                            : (ActivityInfo) activityInfo.invoke(pm, cn, flags(activityInfo, 1), 0);
                    if (ai != null && ai.applicationInfo != null) {
                        label = resolve(ai.nonLocalizedLabel, ai.labelRes, ai.applicationInfo.sourceDir, addAssetPath);
                    }
                }
                if (label == null) {
                    ApplicationInfo ai = (ApplicationInfo) appInfo.invoke(pm, pkg, flags(appInfo, 1), 0);
                    if (ai != null) {
                        label = resolve(ai.nonLocalizedLabel, ai.labelRes, ai.sourceDir, addAssetPath);
                    }
                }
                if (label != null && label.toString().trim().length() > 0) {
                    labels.put(pkg, label.toString().trim());
                }
            } catch (Throwable t) {
                errors.put(pkg, Util.describe(t));
            }
        }
        JSONObject out = new JSONObject();
        out.put("labels", labels);
        if (errors.length() > 0) {
            out.put("errors", errors);
        }
        return out;
    }

    /** 0 flags, as the int or long (Android 13+) the method takes at {@code index}. */
    private static Object flags(Method m, int index) {
        return m.getParameterTypes()[index] == long.class ? (Object) 0L : (Object) 0;
    }

    private static CharSequence resolve(CharSequence nonLocalized, int labelRes, String apk, Method addAssetPath)
            throws Exception {
        if (nonLocalized != null) {
            return nonLocalized;
        }
        if (labelRes == 0 || apk == null) {
            return null;
        }
        AssetManager am = AssetManager.class.newInstance();
        try {
            addAssetPath.invoke(am, apk);
            Resources sys = Resources.getSystem();
            Resources r = new Resources(am, sys.getDisplayMetrics(), sys.getConfiguration());
            return r.getText(labelRes);
        } finally {
            am.close();
        }
    }
}
