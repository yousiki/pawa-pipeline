package moe.siki.pawa.rmdriver;

import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.util.UUID;

/**
 * Assertion self-check for the driver's pure logic, run at image-build time.
 *
 * <p>The mod has no test framework and no place to put one: it is compiled by a bare {@code javac}
 * in a Docker layer, with Fabric Loader and Gson on the classpath and nothing else. So the check is a
 * {@code main} full of {@code assert}s, run once with {@code -ea} right after the compile. If it
 * fails, the image build fails, which is the only gate this code has.
 *
 * <p>It covers only what can run WITHOUT Minecraft or ReplayMod. Everything the driver does through
 * reflection is verified by an actual render; what is here is the arithmetic and string handling that
 * would otherwise be verified by nothing at all.
 *
 * <p>Not covered, deliberately: the CFR frame-grid arithmetic. It used to live here and disagreed
 * with a second copy over rounding (half-up against banker's); it now lives only in the Rust side
 * ({@code crates/align}, frames.jsonl assembly), tested there.
 */
final class SelfCheck {
  private SelfCheck() {}

  public static void main(String[] args) {
    if (!assertionsEnabled()) {
      throw new IllegalStateException("run with -ea; without it every check below is a no-op");
    }
    offlineUuidMatchesTheServersDerivation();
    uuidStringsPassThroughUnchanged();
    quoteEscapesWhatJsonRequires();
    vec3IsLenientAboutBadStartPositions();
    renderSettingsShapeRejectsDrift();
    captureFrameSizeMatchesEachPipeline();
    System.out.println("[rm-driver] self-check OK");
  }

  @SuppressWarnings("AssertWithSideEffects")
  private static boolean assertionsEnabled() {
    boolean enabled = false;
    assert enabled = true; // side effect runs only when -ea is set
    return enabled;
  }

  /**
   * The spectator camera finds its POV by matching this UUID against the recording's spawn packets,
   * so if the derivation drifts from the server's, every job fails {@code camera_attach_failed}. The
   * expected values are Minecraft's documented offline-mode rule: {@code
   * UUID.nameUUIDFromBytes("OfflinePlayer:<name>")}.
   */
  private static void offlineUuidMatchesTheServersDerivation() {
    UUID expected =
        UUID.nameUUIDFromBytes("OfflinePlayer:CapBot0".getBytes(StandardCharsets.UTF_8));
    assert ReplayModDriver.resolveTargetUuid("CapBot0").equals(expected)
        : "offline UUID derivation drifted from UUID.nameUUIDFromBytes(\"OfflinePlayer:\" + name)";
    // Offline UUIDs are name-based (MD5) by definition; a v4 here would mean something random got
    // in, which would attach the camera to nothing.
    assert expected.version() == 3 : "offline UUID must be a name-based (version 3) UUID";
    assert !ReplayModDriver.resolveTargetUuid("CapBot0")
            .equals(ReplayModDriver.resolveTargetUuid("CapBot1"))
        : "two bots must not resolve to one UUID";
  }

  /** A job may name its POV by UUID instead of username; that must not be re-hashed. */
  private static void uuidStringsPassThroughUnchanged() {
    String raw = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
    assert ReplayModDriver.resolveTargetUuid(raw).toString().equals(raw)
        : "a UUID-shaped player_id must pass through, not be hashed as a username";
    assert ReplayModDriver.resolveTargetUuid("  " + raw + "  ").toString().equals(raw)
        : "surrounding whitespace must not turn a UUID into a username";
  }

  /** The status file is hand-assembled, so its escaping is the only thing keeping it parsable. */
  private static void quoteEscapesWhatJsonRequires() {
    assert ReplayModDriver.quote("plain").equals("\"plain\"");
    assert ReplayModDriver.quote("a\"b").equals("\"a\\\"b\"") : "quotes must be escaped";
    assert ReplayModDriver.quote("a\\b").equals("\"a\\\\b\"") : "backslashes must be escaped";
    assert ReplayModDriver.quote("a\nb").equals("\"a\\nb\"") : "newlines must be escaped";
    assert ReplayModDriver.quote("a\tb").equals("\"a\\tb\"");
    assert ReplayModDriver.quote("a\rb").equals("\"a\\rb\"");
    // A stack trace reaches the status file through here; a raw control byte would break the JSON.
    assert ReplayModDriver.quote("a\u0001b").equals("\"a\\u0001b\"")
        : "control characters must be escaped as \\uXXXX";
    // Windows paths appear in failure messages verbatim.
    assert ReplayModDriver.quote("C:\\out\\p00.mp4").equals("\"C:\\\\out\\\\p00.mp4\"");
  }

  /**
   * A malformed {@code start_pos} must never fail a job — it only disables the positional fallback
   * for camera attach. Losing that distinction would turn a cosmetic manifest defect into a dead POV.
   */
  private static void vec3IsLenientAboutBadStartPositions() {
    JsonObject good = object("{\"start_pos\":[1.5,64,-2.5]}");
    double[] parsed = ReplayModDriver.vec3(good, "start_pos");
    assert parsed != null && parsed.length == 3;
    assert parsed[0] == 1.5 && parsed[1] == 64.0 && parsed[2] == -2.5;

    assert ReplayModDriver.vec3(object("{}"), "start_pos") == null : "absent is null, not a crash";
    assert ReplayModDriver.vec3(object("{\"start_pos\":null}"), "start_pos") == null;
    assert ReplayModDriver.vec3(object("{\"start_pos\":[1,2]}"), "start_pos") == null
        : "a 2-element array is not a position";
    assert ReplayModDriver.vec3(object("{\"start_pos\":[1,2,3,4]}"), "start_pos") == null;
    assert ReplayModDriver.vec3(object("{\"start_pos\":\"1,2,3\"}"), "start_pos") == null
        : "a string is not a position";
    assert ReplayModDriver.vec3(object("{\"start_pos\":[1,\"x\",3]}"), "start_pos") == null
        : "an unparsable element must return null rather than throw";
  }

  /**
   * The 22-arg {@code RenderSettings} fill is positional, so a reordered parameter would otherwise
   * render the wrong settings in silence. Feed the checker a correct shape and several drifted ones.
   */
  private static void renderSettingsShapeRejectsDrift() {
    assert ReplayModDriver.renderSettingsShapeError(goodShape()) == null
        : "the shape this driver targets must validate against itself";

    Class<?>[] intForFile = goodShape();
    intForFile[6] = int.class; // outputFile slot became an int
    assert ReplayModDriver.renderSettingsShapeError(intForFile) != null;

    Class<?>[] swapped = goodShape();
    swapped[2] = boolean.class; // width slot became a boolean
    assert ReplayModDriver.renderSettingsShapeError(swapped) != null;

    Class<?>[] notAnEnum = goodShape();
    notAnEnum[0] = String.class; // RenderMethod slot stopped being an enum
    assert ReplayModDriver.renderSettingsShapeError(notAnEnum) != null;

    Class<?>[] primitiveForColor = goodShape();
    primitiveForColor[12] = int.class; // the null-able ReadableColor slot became primitive
    assert ReplayModDriver.renderSettingsShapeError(primitiveForColor) != null
        : "position 12 is passed null, so a primitive there would throw inside newInstance";

    Class<?>[] stringForBool = goodShape();
    stringForBool[21] = String.class; // highPerformance slot became a String
    assert ReplayModDriver.renderSettingsShapeError(stringForBool) != null;
  }

  /**
   * The window, the progress GUI and the main render target are all sized from this before the
   * render starts, and since 26.2 nothing resizes them afterwards. Get it wrong and the first
   * captured frame either reads past its PBO (a hard GL error) or is silently misaligned, so the
   * per-method arithmetic is worth pinning even though the real cross-check happens at render time.
   */
  private static void captureFrameSizeMatchesEachPipeline() {
    assert equal(ReplayModDriver.captureFrameSize("DEFAULT", 640, 360, 360), 640, 360);
    assert equal(ReplayModDriver.captureFrameSize("BLEND", 640, 360, 360), 640, 360);
    assert equal(ReplayModDriver.captureFrameSize("STEREOSCOPIC", 640, 360, 360), 320, 360);
    // Cube faces are square, and the video's height says nothing about them.
    assert equal(ReplayModDriver.captureFrameSize("CUBIC", 1024, 512, 360), 256, 256);
    assert equal(ReplayModDriver.captureFrameSize("EQUIRECTANGULAR", 1024, 512, 360), 256, 256);
    assert equal(ReplayModDriver.captureFrameSize("ODS", 1024, 512, 360), 256, 256);
    // A partial sphere is a crop of a wider panorama, so the faces grow: 180° means a 2048-wide
    // full panorama behind a 1024-wide video.
    assert equal(ReplayModDriver.captureFrameSize("EQUIRECTANGULAR", 1024, 512, 180), 512, 512);
  }

  private static boolean equal(int[] size, int w, int h) {
    return size[0] == w && size[1] == h;
  }

  /** The parameter types the driver's positional fill expects, as a mutable array to perturb. */
  private static Class<?>[] goodShape() {
    return new Class<?>[] {
      SampleEnum.class, SampleEnum.class, int.class, int.class, int.class, int.class, File.class,
      boolean.class, boolean.class, boolean.class, boolean.class, boolean.class, Object.class,
      int.class, int.class, boolean.class, boolean.class, boolean.class, SampleEnum.class,
      String.class, String.class, boolean.class,
    };
  }

  /** Stands in for RenderMethod/EncodingPreset/AntiAliasing, which need ReplayMod to exist. */
  private enum SampleEnum {
    ONLY
  }

  private static JsonObject object(String json) {
    return JsonParser.parseString(json).getAsJsonObject();
  }
}
