package io.github.zzerding.androiddriver;

import android.content.AttributionSource;
import android.content.ComponentName;
import android.content.Context;
import android.content.ContextWrapper;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;
import android.os.UserHandle;
import android.util.Base64;
import android.view.Display;

import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.InputStreamReader;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

/**
 * Device-side helper for android-driver. It creates one independent virtual
 * display, exposes snapshots and activity metadata over a JSON-lines protocol,
 * and destroys the display when stdin closes or the host sends "close".
 */
public final class VirtualDisplayServer {
    private static final int EXPECTED_SDK = 34;
    private static final int SHELL_UID = 2000;
    private static final int MAX_PIXELS = 4_000_000;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final int width;
    private final int height;
    private final int dpi;
    private Context context;
    private ImageReader reader;
    private Image latestImage;
    private VirtualDisplay virtualDisplay;

    private VirtualDisplayServer(int width, int height, int dpi) {
        this.width = width;
        this.height = height;
        this.dpi = dpi;
    }

    private static final class ShellContext extends ContextWrapper {
        ShellContext(Context base) {
            super(base);
        }

        @Override
        public String getPackageName() {
            return "com.android.shell";
        }

        @Override
        public String getOpPackageName() {
            return getPackageName();
        }

        @Override
        public AttributionSource getAttributionSource() {
            return new AttributionSource.Builder(Process.myUid())
                    .setPackageName(getPackageName())
                    .build();
        }
    }

    private static int flag(Class<?> owner, String name) throws Exception {
        return owner.getField(name).getInt(null);
    }

    private void open() throws Exception {
        Class<?> activityThread = Class.forName("android.app.ActivityThread");
        Object thread = activityThread.getMethod("systemMain").invoke(null);
        Context systemContext = (Context) activityThread
                .getMethod("getSystemContext")
                .invoke(thread);
        context = new ShellContext(systemContext);
        reader = ImageReader.newInstance(width, height, android.graphics.PixelFormat.RGBA_8888, 3);
        reader.setOnImageAvailableListener(source -> {
            Image image = source.acquireLatestImage();
            if (image != null) {
                if (latestImage != null) {
                    latestImage.close();
                }
                latestImage = image;
            }
        }, handler);
        int flags = DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC
                | DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_SUPPORTS_TOUCH")
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_DESTROY_CONTENT_ON_REMOVAL")
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_TRUSTED")
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_OWN_DISPLAY_GROUP")
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_TOUCH_FEEDBACK_DISABLED")
                | flag(DisplayManager.class, "VIRTUAL_DISPLAY_FLAG_OWN_FOCUS");
        java.lang.reflect.Constructor<DisplayManager> constructor =
                DisplayManager.class.getDeclaredConstructor(Context.class);
        constructor.setAccessible(true);
        virtualDisplay = constructor.newInstance(context).createVirtualDisplay(
                "android-driver", width, height, dpi, reader.getSurface(), flags);
        if (virtualDisplay == null) {
            throw new IllegalStateException("Virtual display creation returned null");
        }
        if ((virtualDisplay.getDisplay().getFlags() & flag(Display.class, "FLAG_OWN_FOCUS"))
                == 0) {
            throw new IllegalStateException("Virtual display does not have independent focus");
        }
    }

    private JSONObject info() throws Exception {
        Display display = virtualDisplay.getDisplay();
        return new JSONObject()
                .put("display_id", display.getDisplayId())
                .put("display_flags", display.getFlags())
                .put("own_focus", (display.getFlags() & flag(Display.class, "FLAG_OWN_FOCUS")) != 0)
                .put("width", width)
                .put("height", height)
                .put("dpi", dpi)
                .put("pid", Process.myPid())
                .put("uid", Process.myUid());
    }

    private JSONObject snapshot() throws Exception {
        if (latestImage == null) {
            throw new IllegalStateException("No virtual display frame yet");
        }
        Image.Plane plane = latestImage.getPlanes()[0];
        if (plane.getPixelStride() != 4) {
            throw new IllegalStateException("Unexpected pixel stride");
        }
        ByteBuffer buffer = plane.getBuffer();
        byte[] pixels = new byte[width * height * 4];
        for (int row = 0; row < height; row++) {
            buffer.position(row * plane.getRowStride());
            buffer.get(pixels, row * width * 4, width * 4);
        }
        Bitmap bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888);
        try (ByteArrayOutputStream png = new ByteArrayOutputStream()) {
            bitmap.copyPixelsFromBuffer(ByteBuffer.wrap(pixels));
            if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, png)) {
                throw new IllegalStateException("PNG encoding failed");
            }
            return info()
                    .put("timestamp", latestImage.getTimestamp())
                    .put("png", Base64.encodeToString(png.toByteArray(), Base64.NO_WRAP));
        } finally {
            bitmap.recycle();
        }
    }

    private JSONObject activityInfo(String component, int userId) throws Exception {
        if (userId < 0 || userId > 999) {
            throw new IllegalArgumentException("Invalid Android user");
        }
        ComponentName target = ComponentName.unflattenFromString(component);
        if (target == null) {
            throw new IllegalArgumentException("Invalid component name");
        }
        Object user = UserHandle.class.getMethod("of", int.class).invoke(null, userId);
        Context userContext = (Context) Context.class
                .getMethod("createContextAsUser", UserHandle.class, int.class)
                .invoke(context, user, 0);
        ActivityInfo activity = userContext.getPackageManager().getActivityInfo(
                target, PackageManager.ComponentInfoFlags.of(0));
        return new JSONObject()
                .put("user", userId)
                .put("component", target.flattenToString())
                .put("launch_mode", activity.launchMode)
                .put("task_affinity", activity.taskAffinity)
                .put("exported", activity.exported);
    }

    private static void reply(int id, boolean ok, JSONObject result) throws Exception {
        System.out.println(new JSONObject().put("id", id).put("ok", ok).put("result", result));
        System.out.flush();
    }

    private void handle(JSONObject request) {
        int id = request.optInt("id", -1);
        try {
            if (id < 1) {
                throw new IllegalArgumentException("Positive request id required");
            }
            JSONObject result;
            switch (request.getString("command")) {
                case "info":
                    result = info();
                    break;
                case "snapshot":
                    result = snapshot();
                    break;
                case "activity_info":
                    result = activityInfo(
                            request.getString("component"),
                            request.getInt("user"));
                    break;
                case "close":
                    reply(id, true, new JSONObject());
                    close();
                    return;
                default:
                    throw new IllegalArgumentException("Unknown command");
            }
            reply(id, true, result);
        } catch (Exception error) {
            try {
                reply(id, false, new JSONObject().put("error", error.toString()));
            } catch (Exception outputError) {
                close();
            }
        }
    }

    private void readRequests() {
        try (BufferedReader input = new BufferedReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8))) {
            String line;
            while ((line = input.readLine()) != null) {
                JSONObject request = new JSONObject(line);
                handler.post(() -> handle(request));
            }
        } catch (Exception error) {
            System.err.println(error);
        } finally {
            handler.post(this::close);
        }
    }

    private void release() {
        if (latestImage != null) {
            latestImage.close();
            latestImage = null;
        }
        if (virtualDisplay != null) {
            virtualDisplay.release();
            virtualDisplay = null;
        }
        if (reader != null) {
            reader.close();
            reader = null;
        }
    }

    private void close() {
        try {
            release();
        } finally {
            System.exit(0);
        }
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        if (Build.VERSION.SDK_INT != EXPECTED_SDK) {
            throw new IllegalStateException("This helper requires Android SDK " + EXPECTED_SDK);
        }
        if (Process.myUid() != SHELL_UID) {
            throw new IllegalStateException("This helper requires adb shell uid " + SHELL_UID);
        }
        if (args.length != 3) {
            throw new IllegalArgumentException("Expected width height dpi");
        }
        int width = Integer.parseInt(args[0]);
        int height = Integer.parseInt(args[1]);
        int dpi = Integer.parseInt(args[2]);
        if (width < 1 || height < 1 || dpi < 1 || (long) width * height > MAX_PIXELS) {
            throw new IllegalArgumentException("Invalid display dimensions");
        }
        Looper.prepareMainLooper();
        VirtualDisplayServer server = new VirtualDisplayServer(width, height, dpi);
        try {
            server.open();
            reply(0, true, server.info());
            new Thread(server::readRequests, "android-driver-helper-input").start();
            Looper.loop();
        } catch (Throwable error) {
            server.release();
            StringBuilder detail = new StringBuilder(error.toString());
            for (StackTraceElement frame : error.getStackTrace()) {
                if (detail.length() > 2000) {
                    break;
                }
                detail.append("\n  at ").append(frame);
            }
            reply(0, false, new JSONObject().put("error", detail.toString()));
            System.exit(1);
        }
    }
}