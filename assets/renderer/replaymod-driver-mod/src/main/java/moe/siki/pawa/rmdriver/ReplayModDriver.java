package moe.siki.pawa.rmdriver;

import static moe.siki.pawa.rmdriver.Refl.collectionSize;
import static moe.siki.pawa.rmdriver.Refl.ctorWithArity;
import static moe.siki.pawa.rmdriver.Refl.enumValue;
import static moe.siki.pawa.rmdriver.Refl.enumValueOrNull;
import static moe.siki.pawa.rmdriver.Refl.field;
import static moe.siki.pawa.rmdriver.Refl.fieldOrNull;
import static moe.siki.pawa.rmdriver.Refl.invoke;
import static moe.siki.pawa.rmdriver.Refl.invokeFirstNoArg;
import static moe.siki.pawa.rmdriver.Refl.method;
import static moe.siki.pawa.rmdriver.Refl.numberValue;
import static moe.siki.pawa.rmdriver.Refl.staticFieldOrNull;
import static moe.siki.pawa.rmdriver.Refl.staticInvoke;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.io.File;
import java.io.IOException;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import net.fabricmc.api.ClientModInitializer;

/**
 * Unattended ReplayMod render driver (companion Fabric client mod).
 *
 * <p>This is a thin, external companion mod that drives <em>stock, unmodified</em> ReplayMod to
 * render a replay headlessly, instead of building a deep ReplayMod fork. It does NOT compile
 * against ReplayMod or Minecraft internals — it uses reflection at runtime so it tolerates
 * ReplayMod/Minecraft version drift. It compiles against exactly two things: Fabric Loader's {@code
 * ClientModInitializer}, and Gson — which needs no shading or bundling because Minecraft itself
 * declares {@code com.google.code.gson:gson} as a library, so it is already on the game classpath.
 *
 * <p>Every primitive it touches by reflection ({@code ReplayModReplay.startReplay}, {@code
 * ReplaySender.sendPacketsTill}, {@code ReplayModSimplePathing}/{@code SPTimeline} keyframes,
 * {@code EntityPositionTracker}, the 22-arg {@code RenderSettings} constructor, {@code
 * VideoRenderer.renderVideo()}) is stock ReplayMod. Only the batch orchestration around them
 * (open a replay, build its camera timeline, render every projection, end it, move to the next
 * job) lives here, so no patched ReplayMod build is needed.
 *
 * <p>Steps (all on the client/main thread, driven from {@code ClientTickEvents.END_CLIENT_TICK}):
 *
 * <ol>
 *   <li>wait for the main menu + {@code ReplayMod.instance}/{@code ReplayModReplay.instance};
 *   <li>open the replay: {@code core.files.open(path)} then {@code
 *       ReplayModReplay.instance.startReplay(replayFile, false, false)};
 *   <li>pump the world in: {@code ReplayHandler.getReplaySender().sendPacketsTill(t)};
 *   <li>build a camera {@code Timeline} via {@code ReplayModSimplePathing.instance} + {@code
 *       SPTimeline} (+ {@code EntityPositionTracker} for spectator-follow, or a free static camera
 *       if no entity is available);
 *   <li>build {@code RenderSettings} (22-arg ctor, MP4_CUSTOM+ffmpeg or PNG) and run {@code new
 *       VideoRenderer(settings, handler, timeline).renderVideo()} (blocks until done), then {@code
 *       handler.endReplay()};
 *   <li>write a structured status JSON; the {@code pawa} render supervisor
 *       ({@code crates/render/src/client.rs}) polls it and stops the client once it sees the
 *       terminal phase.
 * </ol>
 *
 * <h2>Configuration</h2>
 *
 * <p>ONE environment variable: {@code RM_RENDER_SPEC}, the absolute path of a JSON document. Its
 * presence also activates the driver — unset, this mod is a no-op. The document's schema is
 * {@code crates/schema/src/driver.rs}, which is where the field list and its validators live;
 * the accessors at the bottom of this file read it and every one of them is required, because a
 * default in the reader would be a second place for a value to come from.
 *
 * <p>The spec is written by the {@code pawa} binary ({@code build_spec} in {@code
 * crates/render/src/lib.rs}). One typed document, rather than a set of environment variables, gives
 * the configuration a schema, leaves it on disk for inspection after a run, and turns a misspelled
 * field into a validation error instead of a silent default.
 *
 * <p>Shape, in brief — see {@code DriverSpec} for the authority:
 *
 * <ul>
 *   <li>{@code status_path} — the status JSON this driver writes; the {@code pawa} render
 *       supervisor polls it and stops the client on a terminal phase. An aggregate, with a {@code jobs} array.
 *   <li>{@code fps}, {@code encoding} ({@code mp4} or {@code png}) — shared by all projections.
 *   <li>{@code projections} — each rendered in ONE launch over the same (rebuilt) camera timeline,
 *       writing {@code <name>.mp4}. {@code method} is a RenderMethod constant
 *       (DEFAULT/EQUIRECTANGULAR/CUBIC/ODS/STEREOSCOPIC); {@code encoder} is a codec-and-options
 *       fragment passed through to ffmpeg verbatim, not a bare codec name.
 *   <li>{@code camera} — {@code mode} (auto/spectator/static), {@code allow_static}, and the free
 *       camera's pose and anchor offsets. {@code allow_static} is a debugging opt-in: without it a
 *       job whose camera cannot reach {@code spectator_pov} FAILS as {@code camera_attach_failed}
 *       rather than silently rendering a static video, because every render job is a player POV.
 *   <li>{@code jobs} — ALL of them render in one client session (open → render → endReplay → next),
 *       amortizing the single Minecraft boot. A job names its POV owner with {@code player_id}, or
 *       instead carries {@code camera_path}: an OBSERVER camera-path document ({@code
 *       {"keyframes":[{"time_ms","x","y","z","yaw","pitch","roll"}, ...]}}) holding one keyframe per
 *       rendered frame. Such a job renders a free camera along that path, never attaches to an
 *       entity, and is therefore exempt from {@code camera_attach_failed}. Because a keyframe lands
 *       on every frame time, ReplayMod's interpolation is a no-op at the sampled instants, so the
 *       exported pose equals the requested pose exactly.
 *   <li>{@code spectate_entity_id}, {@code open_delay_ticks}, {@code ready_timeout_ticks},
 *       {@code world_load_ms} — entity override and the three timing bounds.
 * </ul>
 *
 * <p>The {@code failure_class} written to the status file is drawn from the driver's failure
 * taxonomy so the orchestrator can map it straight through.
 */
public final class ReplayModDriver implements ClientModInitializer {

  private static final String LOG_PREFIX = "[rm-driver] ";

  private enum Phase {
    WAIT_MENU,
    OPENING,
    WAIT_READY,
    RENDERING,
    DONE,
    FAILED
  }

  /**
   * One render job: a single {@code .mcpr} → a single output dir. In single-job (RM_REPLAY_PATH)
   * mode the list has exactly one element; in manifest (RM_RENDER_MANIFEST) mode it has one per
   * POV.
   */
  private static final class Job {
    final Path replayPath;
    final Path outputDir;
    // The POV owner's username (or offline UUID string). When present it is the ONLY entity the
    // spectator camera may attach to (matched via its offline-mode UUID against the recording's
    // player spawn packets, with a start-position fallback). Null only in single-job legacy mode.
    final String playerId;
    // The POV owner's first recorded position ([x, y, z] from the capture's action stream), or
    // null. Used as a SECOND-choice camera anchor: when the recording carries no spawn packet for
    // the POV owner's own UUID (ServerReplay does not always emit the recorded player's own
    // spawn), the tracked entity whose first position is nearest to this (within
    // START_POS_MAX_DISTANCE blocks) is spectated instead.
    final double[] startPos;
    // OBSERVER jobs only: the dense camera path this job renders, one entry per rendered frame, as
    // {timeMs, x, y, z, yaw, pitch, roll}. Empty for a player POV. When non-empty the camera is a
    // free camera driven by these keyframes and NO entity is spectated.
    final List<double[]> cameraPath;

    // per-job result, filled in as the job runs:
    String status = "pending"; // pending | complete | failed
    String failureClass = null;
    String failureMessage = null;
    final Map<String, Object> detail = new LinkedHashMap<>();

    Job(Path replayPath, Path outputDir, String playerId, double[] startPos, List<double[]> cameraPath) {
      this.replayPath = replayPath;
      this.outputDir = outputDir;
      this.playerId = playerId;
      this.startPos = startPos;
      this.cameraPath = cameraPath == null ? List.of() : cameraPath;
    }
  }

  /** Max distance (blocks) between a job's start_pos and a tracked entity's first position. */
  private static final double START_POS_MAX_DISTANCE = 8.0;

  /**
   * One render projection (a ReplayMod {@code RenderMethod}). Every projection of a job is rendered
   * in the SAME client launch over the SAME (rebuilt) camera timeline, writing {@code <name>.mp4}
   * into the job's out dir. The set is shared across all jobs and comes from {@code
   * RM_PROJECTIONS}.
   */
  private static final class Projection {
    String name = "default";
    String method =
        "DEFAULT"; // RenderMethod constant: DEFAULT/EQUIRECTANGULAR/CUBIC/ODS/STEREOSCOPIC
    int width = 640;
    int height = 360;
    String encoder = "libx264";
    String antiAliasing = "NONE"; // AntiAliasing constant: NONE/X2/X4/X8
    int sphericalFovX = 360;
    int sphericalFovY = 180;
  }

  private final List<Job> jobs = new ArrayList<>();
  private final List<Projection> projections = new ArrayList<>();
  private int jobIndex = 0; // index of the job currently being processed

  private Phase phase = Phase.WAIT_MENU;
  private int tickCounter = 0;
  private int phaseEnteredTick = 0;
  private boolean menuReady = false; // ReplayMod core seen once → later jobs skip the menu wait

  /**
   * Whether the projection now rendering wants the player HUD in its frames.
   *
   * <p>Read by {@link moe.siki.pawa.rmdriver.mixin.HudUnhideMixin} on the render thread, which cannot
   * be passed a value any other way. Written once per projection, before its {@code VideoRenderer}
   * exists, so no frame ever sees a stale one.
   */
  public static volatile boolean hudInFrame;

  /**
   * The capture kit every recorded bot carries, in hotbar slot order — registry ids without the
   * {@code minecraft:} prefix, and the stack count per slot. From the spec's {@code hud_hotbar};
   * read by {@link HudKit} on the render thread. The protocol never syncs a spectated remote
   * player's container into the replay, so the HUD's hotbar is reconstructed from this instead of
   * showing the one morphing slot the mainhand equipment folds into.
   */
  public static volatile String[] hudHotbarItems;

  /** Stack counts matching {@link #hudHotbarItems} by index. */
  public static volatile int[] hudHotbarCounts;

  // ---- current-job working fields (reset by beginJob) ----
  private Path replayPath;
  private Path outputDir;
  private Path statusPath;
  private int fps;
  private String encoding; // "mp4" | "png"
  private boolean depth;
  private boolean depthPass;
  private int spectateEntityId;

  private String cameraMode; // auto | spectator | static
  // Permit the static-camera fallback when the spectator camera cannot attach. Off by default: every
  // render job is a player POV, and a video whose camera never follows the player is not output,
  // so the job has to fail as camera_attach_failed instead.
  private boolean allowStaticCamera;
  private double camX, camY, camZ;
  private double camDy, camDz; // offset of an anchored static camera from the anchor
  private float camYaw, camPitch;
  private int openDelayTicks;
  private int readyTimeoutTicks;
  private int worldLoadMs;

  private Object replayHandler; // com.replaymod.replay.ReplayHandler
  private int replayDurationMs;
  private int pumpedToMs;
  private int worldReadyTicks;
  private static final int WORLD_SETTLE_TICKS = 20;
  private Object positionTracker; // EntityPositionTracker, sampled for the exported pose trace
  private int cameraEntityId = -1; // spectated entity id (>=0 in spectator_pov)
  // The current job's observer camera path (empty for a player POV). When non-empty it is BOTH the
  // source of the rendered keyframes and the source of the exported pose trace, so the video and the
  // trace can never disagree about where the camera was.
  private List<double[]> cameraPath = List.of();

  // The current job's detail map, ALIASED to Job.detail by beginJob rather than copied — every
  // detail.put below therefore lands in the job's own record with nothing to keep in sync. Before
  // the first beginJob (pre-flight config parsing) it is this standalone map, which is what
  // writeStatus falls back to when there are no jobs at all.
  private Map<String, Object> detail = new LinkedHashMap<>();

  // Pre-flight failures have no Job to record against, so they land here. Once jobs exist, fail()
  // writes straight into the Job and this stays null: a given failure lives in exactly one place.
  private String preflightFailureClass = null;
  private String preflightFailureMessage = null;

  private boolean active = false;

  /**
   * The ONE environment variable this mod reads: the path of its {@link DriverSpec}-shaped JSON
   * document. Its presence is also what activates the driver. Kept in step with {@code
   * pawa_schema::driver::DRIVER_SPEC_ENV}.
   */
  private static final String SPEC_ENV = "RM_RENDER_SPEC";

  @Override
  public void onInitializeClient() {
    String specPath = System.getenv(SPEC_ENV);
    if (specPath == null || specPath.isEmpty()) {
      log("disabled (" + SPEC_ENV + " unset); driver is a no-op");
      return;
    }
    active = true;
    try {
      loadSpec(Path.of(specPath));
      for (Job j : jobs) {
        Files.createDirectories(j.outputDir);
      }
    } catch (Throwable t) {
      // Pre-flight config failure: no job context yet, so record at the session level.
      //
      // Throwable, not Exception: this is the first code to touch Gson, and a Gson that is not on the
      // game classpath arrives as NoClassDefFoundError — an Error. Letting that escape would crash
      // mod init with an EMPTY status file, so the orchestrator would see a timeout instead of a
      // cause. Anything that stops the driver before it can render must be diagnosable from the
      // status file, whatever its type.
      fail("client_launch_failed", "driver spec invalid: " + t);
      // Terminal, and quit: leaving the phase at WAIT_MENU would make the orchestrator sit out
      // the SUM of every projection timeout (hours) and then overwrite this failure class with
      // progress_timeout. requestQuit tolerates a client instance that does not exist yet — its
      // JVM backstop is what guarantees the exit this early in boot.
      phase = Phase.FAILED;
      writeTerminalStatus();
      requestQuit();
      return;
    }
    log(
        "enabled; jobs="
            + jobs.size()
            + " fps="
            + fps
            + " encoding="
            + encoding
            + " projections: "
            + projectionSummary());
    beginJob(0);
    writeStatus();
    registerTickCallback();
  }

  /**
   * Read the whole configuration out of one document.
   *
   * <p>Nothing is defaulted here: the producer serializes the typed {@code
   * pawa_schema::driver::DriverSpec} and writes every field, so a missing one is a bug worth failing
   * on rather than papering over — a reader-side default is how a misspelling turns into an episode
   * rendered at the wrong resolution.
   */
  private void loadSpec(Path path) throws Exception {
    JsonObject spec = JsonParser.parseString(Files.readString(path, StandardCharsets.UTF_8)).getAsJsonObject();
    statusPath = Path.of(reqStr(spec, "status_path"));
    fps = reqInt(spec, "fps");
    encoding = reqStr(spec, "encoding").toLowerCase();
    depth = spec.get("depth").getAsBoolean();
    spectateEntityId = reqInt(spec, "spectate_entity_id");
    openDelayTicks = reqInt(spec, "open_delay_ticks");
    readyTimeoutTicks = reqInt(spec, "ready_timeout_ticks");
    worldLoadMs = reqInt(spec, "world_load_ms");
    JsonArray hotbar = reqArray(spec, "hud_hotbar");
    String[] kitItems = new String[hotbar.size()];
    int[] kitCounts = new int[hotbar.size()];
    for (int i = 0; i < hotbar.size(); i++) {
      JsonObject slot = hotbar.get(i).getAsJsonObject();
      kitItems[i] = reqStr(slot, "item");
      kitCounts[i] = reqInt(slot, "count");
    }
    hudHotbarItems = kitItems;
    hudHotbarCounts = kitCounts;
    JsonObject camera = spec.getAsJsonObject("camera");
    if (camera == null) throw new IllegalArgumentException("driver spec has no camera object");
    cameraMode = reqStr(camera, "mode").toLowerCase();
    allowStaticCamera = camera.get("allow_static").getAsBoolean();
    camX = reqDouble(camera, "x");
    camY = reqDouble(camera, "y");
    camZ = reqDouble(camera, "z");
    camDy = reqDouble(camera, "dy");
    camDz = reqDouble(camera, "dz");
    camYaw = (float) reqDouble(camera, "yaw");
    camPitch = (float) reqDouble(camera, "pitch");
    for (JsonElement element : reqArray(spec, "projections")) {
      JsonObject p = element.getAsJsonObject();
      Projection projection = new Projection();
      projection.name = reqStr(p, "name");
      projection.method = reqStr(p, "method").toUpperCase();
      projection.width = reqInt(p, "width");
      projection.height = reqInt(p, "height");
      projection.encoder = reqStr(p, "encoder");
      projection.antiAliasing = reqStr(p, "anti_aliasing").toUpperCase();
      projection.sphericalFovX = reqInt(p, "spherical_fov_x");
      projection.sphericalFovY = reqInt(p, "spherical_fov_y");
      projections.add(projection);
    }
    for (JsonElement element : reqArray(spec, "jobs")) {
      JsonObject entry = element.getAsJsonObject();
      jobs.add(new Job(Path.of(reqStr(entry, "replay_path")), Path.of(reqStr(entry, "out_path")), str(entry, "player_id"), vec3(entry, "start_pos"), loadCameraPath(str(entry, "camera_path"))));
    }
    if (jobs.isEmpty() || projections.isEmpty()) throw new IllegalArgumentException("driver spec has " + jobs.size() + " job(s) and " + projections.size() + " projection(s)");
  }


  /**
   * Load an observer job's dense camera path from its {@code camera_path} document, ascending by
   * time. Unlike {@code start_pos} a malformed camera path is NOT recoverable — silently rendering
   * some other camera is the failure mode this whole feature exists to avoid — so a bad document
   * throws and the pre-flight handler records {@code client_launch_failed}.
   */
  private static List<double[]> loadCameraPath(String pathText) throws Exception {
    if (pathText == null || pathText.isEmpty()) return List.of();
    String text = Files.readString(Path.of(pathText), StandardCharsets.UTF_8);
    // Top-level `keyframes` only: an observer document also carries its `observer.keyframes` count
    // spec, and reading that one instead would render a camera nobody asked for.
    JsonArray keyframes = JsonParser.parseString(text).getAsJsonObject().getAsJsonArray("keyframes");
    if (keyframes == null) {
      throw new IllegalArgumentException(
          "camera path " + pathText + " has no top-level keyframes array");
    }
    List<double[]> path = new ArrayList<>(keyframes.size());
    for (JsonElement element : keyframes) {
      JsonObject row = element.getAsJsonObject();
      path.add(
          new double[] {
            reqDouble(row, "time_ms"),
            reqDouble(row, "x"),
            reqDouble(row, "y"),
            reqDouble(row, "z"),
            reqDouble(row, "yaw"),
            reqDouble(row, "pitch"),
            doubleOr(row, "roll", 0.0)
          });
    }
    if (path.size() < 2) {
      throw new IllegalArgumentException(
          "camera path " + pathText + " has " + path.size() + " keyframe(s); need at least 2");
    }
    path.sort((a, b) -> Double.compare(a[0], b[0]));
    return path;
  }

  /** Optional string field; null when absent or JSON null. */
  private static String str(JsonObject object, String key) {
    JsonElement element = object.get(key);
    return element == null || element.isJsonNull() ? null : element.getAsString();
  }

  private static double reqDouble(JsonObject row, String key) {
    JsonElement element = row.get(key);
    if (element == null || element.isJsonNull()) {
      throw new IllegalArgumentException("camera path keyframe missing " + key);
    }
    return element.getAsDouble();
  }

  private static double doubleOr(JsonObject object, String key, double fallback) {
    JsonElement element = object.get(key);
    return element == null || element.isJsonNull() ? fallback : element.getAsDouble();
  }

  /**
   * Read a manifest {@code start_pos} into a 3-element double array, or null when absent/malformed
   * (a bad start_pos must never fail the job; it only disables the positional fallback).
   */
  static double[] vec3(JsonObject object, String key) {
    JsonElement element = object.get(key);
    if (element == null || !element.isJsonArray()) return null;
    JsonArray array = element.getAsJsonArray();
    if (array.size() != 3) return null;
    try {
      return new double[] {
        array.get(0).getAsDouble(), array.get(1).getAsDouble(), array.get(2).getAsDouble()
      };
    } catch (RuntimeException e) {
      return null;
    }
  }

  private String projectionSummary() {
    StringBuilder sb = new StringBuilder();
    for (Projection p : projections) {
      if (sb.length() > 0) sb.append(", ");
      sb.append(p.name)
          .append('=')
          .append(p.method)
          .append(' ')
          .append(p.width)
          .append('x')
          .append(p.height);
    }
    return sb.toString();
  }

  /**
   * Switch the current-job working fields to job {@code idx} and reset the phase machine for it.
   */
  private void beginJob(int idx) {
    jobIndex = idx;
    Job job = jobs.get(idx);
    replayPath = job.replayPath;
    outputDir = job.outputDir;
    detail = job.detail;
    // reset the per-job render state machine
    replayHandler = null;
    replayDurationMs = 0;
    pumpedToMs = 0;
    worldReadyTicks = 0;
    positionTracker = null;
    cameraEntityId = -1;
    cameraPath = job.cameraPath;
    // Every job re-enters at WAIT_MENU; for later jobs the openDelay + core-init wait are
    // short-circuited by menuReady (the core is already up), so they open immediately.
    phase = Phase.WAIT_MENU;
    phaseEnteredTick = tickCounter;
    detail.put("replay_path", replayPath.toString());
    detail.put("output_dir", outputDir.toString());
    if (job.playerId != null) detail.put("player_id", job.playerId);
    if (!cameraPath.isEmpty()) detail.put("camera_path_keyframes", cameraPath.size());
    detail.put("requested_fps", fps);
    // Per-projection, because a session-level width/height was a fiction: the driver has always
    // rendered whatever each projection asked for, while requested_width/requested_height reported
    // the retired scalar fallback — a value nothing used once RM_PROJECTIONS was always set.
    detail.put("projections", projectionSummary());
    detail.put("encoding", encoding);
    detail.put("camera_requested", cameraMode);
    log(
        "JOB "
            + (idx + 1)
            + "/"
            + jobs.size()
            + (job.playerId != null ? " player=" + job.playerId : "")
            + " replay="
            + replayPath
            + " output="
            + outputDir);
  }

  /**
   * The job being processed, or null before the first {@link #beginJob} — i.e. only while pre-flight
   * config parsing can still fail. The index is clamped because the last job's terminal status is
   * written after the loop has run out of jobs to advance to.
   */
  private Job cur() {
    return jobs.isEmpty() ? null : jobs.get(Math.min(jobIndex, jobs.size() - 1));
  }

  /** The failure class recorded for the current job (or for pre-flight), or null if none yet. */
  private String failureClass() {
    Job job = cur();
    return job != null ? job.failureClass : preflightFailureClass;
  }

  private String failureMessage() {
    Job job = cur();
    return job != null ? job.failureMessage : preflightFailureMessage;
  }

  /** Advance to the next job (resetting the phase machine), or finish the session. */
  private void finishJobAndAdvance(boolean ok) {
    Job job = jobs.get(jobIndex);
    job.status = ok ? "complete" : "failed";
    log(
        "JOB "
            + (jobIndex + 1)
            + "/"
            + jobs.size()
            + " finished: "
            + job.status
            + (job.failureClass != null ? " (" + job.failureClass + ")" : ""));
    if (jobIndex + 1 < jobs.size()) {
      beginJob(jobIndex + 1);
      writeStatus();
      return;
    }
    // All jobs processed → terminal session phase.
    long completed = jobs.stream().filter(j -> "complete".equals(j.status)).count();
    phase = completed > 0 ? Phase.DONE : Phase.FAILED;
    log(
        "all "
            + jobs.size()
            + " job(s) processed; "
            + completed
            + " complete; session phase="
            + phase);
    writeTerminalStatus();
    requestQuit();
  }

  /** Register {@code ClientTickEvents.END_CLIENT_TICK} via reflection (Fabric API). */
  private void registerTickCallback() {
    try {
      Class<?> eventsCls =
          Class.forName("net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents");
      Field endField = eventsCls.getField("END_CLIENT_TICK");
      Object event = endField.get(null);
      Class<?> endTickCls =
          Class.forName(
              "net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents$EndTick");
      Object proxy =
          java.lang.reflect.Proxy.newProxyInstance(
              endTickCls.getClassLoader(),
              new Class<?>[] {endTickCls},
              (p, m, args) -> {
                if ("onEndTick".equals(m.getName())) {
                  onClientTick();
                  return null;
                }
                if ("equals".equals(m.getName())) return p == args[0];
                if ("hashCode".equals(m.getName())) return System.identityHashCode(p);
                if ("toString".equals(m.getName())) return "rm-driver-endtick";
                return null;
              });
      Method register = event.getClass().getMethod("register", Object.class);
      register.setAccessible(true);
      register.invoke(event, endTickCls.cast(proxy));
      log("registered END_CLIENT_TICK callback");
    } catch (Throwable t) {
      fail("client_launch_failed", "could not register tick callback: " + t);
      // Without the tick callback nothing can ever advance the phase machine — same hours-long
      // timeout as a bad spec, so: terminal, and quit.
      phase = Phase.FAILED;
      writeTerminalStatus();
      requestQuit();
    }
  }

  private void onClientTick() {
    if (!active) return;
    tickCounter++;
    try {
      switch (phase) {
        case WAIT_MENU -> stepWaitMenu();
        case OPENING -> {
          /* handled synchronously inside stepWaitMenu */
        }
        case WAIT_READY -> stepWaitReady();
        case RENDERING -> {
          /* handled synchronously inside stepWaitReady */
        }
        case DONE, FAILED -> {
          /* terminal */
        }
      }
    } catch (Throwable t) {
      fail(
          failureClass() == null ? "client_crash" : failureClass(),
          "unhandled driver error in " + phase + ": " + stack(t));
      // Don't kill the whole session for one job's error — record it and move on.
      try {
        if (replayHandler != null) {
          invoke(replayHandler, method(replayHandler.getClass(), "endReplay", 0));
        }
      } catch (Throwable ignored) {
        /* best effort */
      }
      finishJobAndAdvance(false);
    }
  }

  // ---- phase 1: wait for menu + ReplayMod, then open the replay -------------

  private void stepWaitMenu() throws Exception {
    // The open-delay (menu settle) is only paid ONCE per client session, before the first job.
    // Later jobs reuse the already-initialized ReplayMod core and open immediately.
    if (!menuReady && tickCounter - phaseEnteredTick < openDelayTicks) return;
    Object replayMod = staticFieldOrNull("com.replaymod.replay.ReplayModReplay", "instance");
    Object core = staticFieldOrNull("com.replaymod.core.ReplayMod", "instance");
    if (replayMod == null || core == null) {
      if (tickCounter - phaseEnteredTick > readyTimeoutTicks) {
        fail(
            "replaymod_automation_unavailable",
            "ReplayMod core never initialized (ReplayModReplay.instance="
                + (replayMod != null)
                + ", ReplayMod.instance="
                + (core != null)
                + ")");
        finishJobAndAdvance(false);
      }
      return;
    }
    menuReady = true; // core is up; subsequent jobs skip the open-delay
    detail.put("replaymod_instance_seen", true);
    transition(Phase.OPENING);
    log("opening replay: " + replayPath);

    // core.files.open(replayPath) -> ReplayFile ; then startReplay(rf, false, false)
    Object files = field(core, "files");
    Object replayFile = invoke(files, method(files.getClass(), "open", 1), replayPath);
    replayHandler =
        invoke(
            replayMod,
            method(replayMod.getClass(), "startReplay", 3),
            replayFile,
            Boolean.FALSE,
            Boolean.FALSE);
    if (replayHandler == null) {
      fail("replay_load_failed", "ReplayModReplay.startReplay returned null for " + replayPath);
      finishJobAndAdvance(false);
      return;
    }
    // ReplayMod's on-screen replay controls (play/pause, timeline, keyframe track) are a human UI,
    // and nothing suppresses them now that the HUD reaches the exported frames: they would sit
    // across the top of every one. setVisible(false) also unregisters their input handlers.
    Object overlay = invoke(replayHandler, method(replayHandler.getClass(), "getOverlay", 0));
    invoke(overlay, method(overlay.getClass(), "setVisible", 1), Boolean.FALSE);
    replayDurationMs =
        (int) invoke(replayHandler, method(replayHandler.getClass(), "getReplayDuration", 0));
    detail.put("replay_opened", true);
    detail.put("replay_duration_ms", replayDurationMs);
    log("replay opened; durationMs=" + replayDurationMs);
    transition(Phase.WAIT_READY);
    writeStatus();
  }

  // ---- phase 2: pump the replay until the client has renderable terrain -----

  private void stepWaitReady() throws Exception {
    Object sender = invoke(replayHandler, method(replayHandler.getClass(), "getReplaySender", 0));
    int limit = Math.max(replayDurationMs, 1000);
    int target = Math.min(worldLoadMs, limit);
    // sendPacketsTill is synchronous. Advance conservatively and keep ticking after the target so
    // Minecraft can consume chunk packets and build render sections on the client thread.
    //
    // worldLoadMs is a FLOOR, not a horizon. The client builds sections only once the recording has
    // delivered the chunks AROUND THE CAMERA, and how deep into the recording those land depends on
    // how fast the capture server fed chunks to this particular player — a POV whose own chunk
    // arrives at 3075ms against a 3000ms horizon never builds a single section and burns the whole
    // ready timeout. So once the floor is paid, keep pumping while the world is still not
    // renderable; the pump stops the tick terrain appears, and the render rewinds to time 0 anyway.
    if (pumpedToMs < target || (worldReadyTicks == 0 && pumpedToMs < limit)) {
      pumpedToMs = Math.min(pumpedToMs + 1000, limit);
      invoke(sender, method(sender.getClass(), "sendPacketsTill", 1), Integer.valueOf(pumpedToMs));
    }
    detail.put("world_pumped_ms", pumpedToMs);

    WorldReadiness readiness = inspectWorldReadiness();
    detail.put("world_loaded_detected", readiness.worldPresent);
    detail.put("loaded_chunk_count", readiness.loadedChunks);
    detail.put("built_chunk_count", readiness.builtChunks);
    detail.put("world_readiness_probe", readiness.probe);

    boolean strictRenderable =
        pumpedToMs >= target
            && readiness.probeAvailable
            && readiness.worldPresent
            && readiness.loadedChunks > 0
            && readiness.builtChunks > 0;
    // Mapping drift must not deadlock all renders. If either probe in inspectWorldReadiness cannot be
    // resolved (a rename in a newer Minecraft), fall back to requiring the pump to have finished AND
    // a deliberately long client-thread settle period. The settle is counted from PHASE ENTRY, not
    // from pump completion: the pump finishes in a handful of ticks against a settle of
    // ~openDelayTicks, so the two are within a few percent of each other, and phaseEnteredTick is
    // the clock the surrounding timeout already uses.
    int fallbackSettleTicks = Math.max(WORLD_SETTLE_TICKS, openDelayTicks);
    boolean fallbackRenderable =
        pumpedToMs >= target
            && readiness.worldPresent
            && !readiness.probeAvailable
            && tickCounter - phaseEnteredTick >= fallbackSettleTicks;
    worldReadyTicks = strictRenderable ? worldReadyTicks + 1 : 0;
    detail.put("world_ready_settle_ticks", worldReadyTicks);
    detail.put("world_readiness_fallback", fallbackRenderable);
    if (worldReadyTicks >= WORLD_SETTLE_TICKS || fallbackRenderable) {
      transition(Phase.RENDERING);
      writeStatus();
      doRender(); // blocks the main thread for the whole render
      return;
    }
    if (tickCounter - phaseEnteredTick > readyTimeoutTicks) {
      fail(
          "replay_load_failed",
          "world did not become renderable within "
              + readyTimeoutTicks
              + " ticks (pumpedToMs="
              + pumpedToMs
              + ", loadedChunks="
              + readiness.loadedChunks
              + ", builtChunks="
              + readiness.builtChunks
              + ", settleTicks="
              + worldReadyTicks
              + ", probe="
              + readiness.probe
              + ", probeAvailable="
              + readiness.probeAvailable
              + ")");
      // Best-effort close so the next job opens cleanly.
      try {
        if (replayHandler != null) {
          invoke(replayHandler, method(replayHandler.getClass(), "endReplay", 0));
        }
      } catch (Throwable ignored) {
        /* best effort */
      }
      finishJobAndAdvance(false);
    }
  }

  // ---- phase 3: build timeline + settings and render ------------------------

  private void doRender() {
    try {
      Object mc = staticInvoke("com.replaymod.core.versions.MCVer", "getMinecraft");
      tryCloseScreen(mc);

      // Render EACH projection over the camera timeline in this ONE launch — the replay stays open
      // (endReplay only after the last projection). renderVideo() blocks until ffmpeg finishes, so
      // a
      // simple loop suffices. The timeline is rebuilt per projection: it is projection-independent,
      // but a fresh build guarantees no shared pathing state is consumed/mutated between renders.
      Class<?> vrCls = Class.forName("com.replaymod.render.rendering.VideoRenderer");
      Constructor<?> vrCtor = ctorWithArity(vrCls, 3);
      vrCtor.setAccessible(true);

      boolean allOk = true;
      int rendered = 0;
      for (Projection proj : projections) {
        int passes = depth ? 2 : 1;
        for (int pass = 0; pass < passes; pass++) {
          depthPass = pass == 1;
          Object timeline = buildTimeline();
          if (timeline == null) {
            try {
              invoke(replayHandler, method(replayHandler.getClass(), "endReplay", 0));
            } catch (Throwable t) {
              log("endReplay() threw (non-fatal): " + t);
            }
            finishJobAndAdvance(false);
            return;
          }
          Object settings = buildRenderSettings(proj);
          detail.put("render_settings_built", true);
          sizeWindowForCapture(mc, settings, proj);
          Object videoRenderer = vrCtor.newInstance(settings, replayHandler, timeline);
          log("starting VideoRenderer.renderVideo() projection='" + proj.name + "' depth=" + depthPass);
          resizeMainTargetForCapture(mc, videoRenderer, proj);
          Object ret = invoke(videoRenderer, method(vrCls, "renderVideo", 0));
          boolean ok = Boolean.TRUE.equals(ret);
          log("renderVideo() [" + proj.name + "] depth=" + depthPass + " returned " + ok);
          allOk = allOk && ok;
          if (ok && !depthPass) rendered++;
          waitForOutputPresent(proj);
        }
      }
      detail.put("projections_rendered", rendered);
      detail.put("render_video_returned", allOk);

      try {
        invoke(replayHandler, method(replayHandler.getClass(), "endReplay", 0));
      } catch (Throwable t) {
        log("endReplay() threw (non-fatal): " + t);
      }

      boolean hasOutput = outputPresent();
      detail.put("output_present", hasOutput);
      boolean jobOk;
      if (!hasOutput) {
        // Pipeline ran but produced nothing — most likely the replay has no
        // followable entity / camera path anchor (e.g. a chunk-area capture).
        fail(
            "render_output_missing",
            "render pipeline ran but produced no output media in "
                + outputDir
                + " (renderVideo allOk="
                + allOk
                + ")");
        jobOk = false;
      } else if (!allOk) {
        fail(
            "render_start_failed",
            "VideoRenderer.renderVideo() returned false for a projection (aborted)");
        jobOk = false;
      } else {
        writePoseTrace(); // the pose half of the timeline; pawa assembles frames.jsonl from it
        jobOk = true;
      }
      finishJobAndAdvance(jobOk);
    } catch (Throwable t) {
      fail(failureClass() == null ? "client_crash" : failureClass(), "render failed: " + stack(t));
      try {
        if (replayHandler != null) {
          invoke(replayHandler, method(replayHandler.getClass(), "endReplay", 0));
        }
      } catch (Throwable ignored) {
        /* best effort */
      }
      finishJobAndAdvance(false);
    }
  }

  /** {@link #captureFrameSize} over the three settings values ReplayMod's pipelines read. */
  private static int[] captureFrameSize(Object settings, Projection proj) throws Exception {
    // getVideoWidth/Height, not the requested width/height: they already carry the anti-aliasing
    // multiplier, and the pipelines are built from these too.
    return captureFrameSize(
        proj.method,
        ((Number) invokeFirstNoArg(settings, "getVideoWidth")).intValue(),
        ((Number) invokeFirstNoArg(settings, "getVideoHeight")).intValue(),
        ((Number) invokeFirstNoArg(settings, "getSphericalFovX")).intValue());
  }

  /**
   * The size ReplayMod's capturer will read back per frame, mirroring {@code Pipelines.newPipeline}.
   *
   * <p>Every cube-face pipeline captures square frames rather than the video's aspect, so the video
   * size alone is not the frame size. {@link #resizeMainTargetForCapture} cross-checks the result
   * against the capturer that actually gets built and complains if this ever drifts.
   */
  static int[] captureFrameSize(String method, int videoWidth, int videoHeight, int sphericalFovX) {
    switch (method) {
      case "CUBIC":
        return new int[] {videoWidth / 4, videoWidth / 4};
      case "EQUIRECTANGULAR":
      case "ODS":
        // EquirectangularToBitmapProcessor: the sphere is `fovX` degrees wide, so the full 360°
        // panorama is wider than the video; one cube face is a quarter of that.
        int full =
            sphericalFovX < 360 ? Math.round(videoWidth * 360f / sphericalFovX) : videoWidth;
        return new int[] {full / 4, full / 4};
      case "STEREOSCOPIC":
        return new int[] {videoWidth / 2, videoHeight};
      default:
        return new int[] {videoWidth, videoHeight};
    }
  }

  /**
   * Put the window at the capture size before ReplayMod reads it.
   *
   * <p>{@code VideoRenderer}'s constructor builds the {@code VirtualWindow} (and with it the
   * progress GUI's framebuffer and layout) from the window's current size, and every GUI frame it
   * draws then resizes the main render target back to that size — which is what makes the capture
   * size the only size everything downstream may be built at. Anything else ends in either a
   * scissor out of bounds from the GUI or an out-of-bounds PBO read from the capturer.
   */
  private void sizeWindowForCapture(Object mc, Object settings, Projection proj) throws Exception {
    int[] frame = captureFrameSize(settings, proj);
    Class<?> mcVer = Class.forName("com.replaymod.core.versions.MCVer");
    invoke(null, method(mcVer, "resizeMainWindow", 3), mc, frame[0], frame[1]);
  }

  /**
   * Give the main render target the frame size the pipeline's capturer is about to read back.
   *
   * <p>26.2 moved the target's resize into {@code GameRenderer.render()}, which ReplayMod's capture
   * loop never calls: {@code OpenGlFrameCapturer.renderFrame} only rewrites {@code Window}'s
   * framebuffer fields ({@code MCVer.resizeMainWindow}) and then renders the world straight into
   * {@code gameRenderer.mainRenderTarget()}. Until a GUI frame happens to resize it, the target
   * therefore keeps the size it had when rendering started, so the very first {@code
   * captureFrame()} copies a whole 854x480 texture into a buffer sized for the frame —
   * "GL_INVALID_OPERATION ... out of bounds PBO access" — or, when the frame is larger, silently
   * misaligns that frame.
   */
  private void resizeMainTargetForCapture(Object mc, Object videoRenderer, Projection proj)
      throws Exception {
    Object capturer = field(field(videoRenderer, "renderingPipeline"), "capturer");
    int w = ((Number) invokeFirstNoArg(capturer, "getFrameWidth")).intValue();
    int h = ((Number) invokeFirstNoArg(capturer, "getFrameHeight")).intValue();
    int[] assumed = captureFrameSize(field(videoRenderer, "settings"), proj);
    if (assumed[0] != w || assumed[1] != h) {
      log(
          "WARNING capture frame is "
              + w
              + "x"
              + h
              + " but the window was sized "
              + assumed[0]
              + "x"
              + assumed[1]
              + " — captureFrameSize() no longer matches ReplayMod's pipeline");
    }
    Object target = invokeFirstNoArg(field(mc, "gameRenderer"), "mainRenderTarget");
    invoke(target, method(target.getClass(), "resize", 2), w, h);
    log("main render target sized " + w + "x" + h + " for projection '" + proj.name + "'");
  }

  /**
   * Build a spectator camera {@link Timeline}-equivalent from stock ReplayMod primitives. When the job names its POV owner ({@code player_id}), the spectator camera attaches ONLY
   * to that player's entity: first by matching its offline-mode UUID against the recording's spawn
   * packets, then (when the recording lacks the POV owner's own spawn packet) by picking the
   * tracked entity whose first position is nearest the job's {@code start_pos} (within {@link
   * #START_POS_MAX_DISTANCE} blocks). Failure to attach records {@code camera_attach_failed} for
   * THIS job and returns null (the caller fails only this job and continues with the next one). The
   * static free-camera fallback survives only behind an explicit opt-in ({@code RM_CAMERA=static}
   * or {@code RM_ALLOW_STATIC_CAMERA=1}) for debugging.
   */
  private Object buildTimeline() throws Exception {
    Object pathing =
        staticFieldOrNull("com.replaymod.simplepathing.ReplayModSimplePathing", "instance");
    if (pathing == null) {
      throw new IllegalStateException(
          "ReplayModSimplePathing.instance is null (module not loaded)");
    }
    invoke(pathing, method(pathing.getClass(), "clearCurrentTimeline", 0));
    Object sp = invoke(pathing, method(pathing.getClass(), "getCurrentTimeline", 0));

    Object replayFile = invoke(replayHandler, method(replayHandler.getClass(), "getReplayFile", 0));
    // Drop ReplayMod's persisted entity-position cache BEFORE the tracker loads, so load() always
    // parses THIS recording. The cache lives in <gamedir>/.replay_cache/<escaped replay path> —
    // keyed purely on the replay's PATH — and our staged paths (render-job-<idx>.mcpr) are REUSED
    // across episodes within one long-lived worker container. ZipReplayFile's freshness check
    // compares the recording.tmcpr.crc32 entry of cache vs replay, but capture-produced .mcpr
    // files all carry the literal placeholder "invalid" there, so a stale cache can NEVER be
    // invalidated: the tracker then silently loads the PREVIOUS episode's entity positions, the
    // POV's (correctly uuid-resolved) entity id has no tracked positions, the start_pos fallback
    // measures against the previous episode's world coordinates, and the job dies
    // camera_attach_failed. (Evidence: straight_line_walk-8p-s41006 rendered after
    // group_orbit-8p-s41004 in one container fails 8/8 with "resolved to entity id N but has no
    // tracked positions ... entity_count=74..92" and "Loaded entity tracker in 15ms" — a cache
    // hit; rendered first in a fresh container it completes 8/8.)
    try {
      invoke(replayFile, method(replayFile.getClass(), "removeCache", 1), "entity_positions.bin");
      detail.put("entity_tracker_cache_invalidated", true);
    } catch (Throwable t) {
      detail.put("entity_tracker_cache_invalidated", false);
      log("removeCache(entity_positions.bin) failed (non-fatal): " + t);
    }
    Class<?> trackerCls = Class.forName("com.replaymod.replaystudio.util.EntityPositionTracker");
    Object tracker = ctorWithArity(trackerCls, 1).newInstance(replayFile);
    Consumer<Double> noop = d -> {};
    invoke(tracker, method(trackerCls, "load", 1), noop);
    invoke(sp, method(sp.getClass(), "setEntityTracker", 1), tracker);
    this.positionTracker = tracker; // A.1: keep for per-frame pose export

    int duration = replayDurationMs > 0 ? replayDurationMs : 1000;
    Map<?, ?> positions = positionsMap(tracker);
    int entityCount = positions == null ? 0 : positions.size();
    detail.put("entity_count", entityCount);

    // OBSERVER: a free camera driven by this job's dense camera path. Resolved BEFORE any spectator
    // lookup, so an observer is never failed for having no entity to follow — it has no POV owner
    // by definition.
    if (!cameraPath.isEmpty()) {
      return buildCameraPathTimeline(sp, duration);
    }

    boolean allowStatic = allowStaticCamera;
    String jobPlayerId = jobs.get(jobIndex).playerId;

    int entityId = -1;
    if (!"static".equals(cameraMode)) {
      if (spectateEntityId >= 0) {
        // Explicit entity id from the environment (debug override).
        entityId = spectateEntityId;
        detail.put("entity_id_source", "env");
      } else if (jobPlayerId != null && !jobPlayerId.isEmpty()) {
        // The job names its POV owner: attach ONLY to that player's entity, found by matching
        // its offline-mode UUID (UUID.nameUUIDFromBytes("OfflinePlayer:"+name), exactly how the
        // offline server assigns UUIDs) against the recording's player spawn packets. Never fall
        // back to a heuristic entity — in a multi-bot recording that silently renders the wrong
        // (or a static) POV.
        java.util.UUID targetUuid = resolveTargetUuid(jobPlayerId);
        detail.put("target_player", jobPlayerId);
        detail.put("target_uuid", targetUuid.toString());
        int playerEid = findPlayerEntityId(replayFile, targetUuid);
        if (playerEid >= 0) {
          detail.put("player_entity_id", playerEid);
          Map<?, ?> pim = innerMap(positions, playerEid);
          if (pim != null && !pim.isEmpty()) {
            entityId = playerEid;
            detail.put("entity_id_source", "job_player_uuid");
          } else {
            detail.put("player_entity_untracked", true);
          }
        }
        if (entityId < 0) {
          // SECOND choice: the recording carried no usable spawn packet for the POV owner's own
          // UUID (ServerReplay does not always emit the recorded player's own spawn). Fall back
          // to the tracked entity whose FIRST position is nearest to the job's start_pos (the
          // POV's first recorded action-stream position) — the recorded player itself stands
          // there when recording begins, so a tight radius keeps this unambiguous.
          entityId = findEntityIdByStartPos(jobs.get(jobIndex).startPos, positions);
        }
        if (entityId < 0 && !allowStatic) {
          double[] jobStartPos = jobs.get(jobIndex).startPos;
          cameraAttachFailure(
              "POV player '"
                  + jobPlayerId
                  + "' (uuid "
                  + targetUuid
                  + ") "
                  + (playerEid >= 0
                      ? "resolved to entity id " + playerEid + " but has no tracked positions"
                      : "matched no player spawn packet in the recording")
                  + (jobStartPos == null
                      ? " and the job carries no start_pos for the positional fallback"
                      : " and no tracked entity starts within "
                          + START_POS_MAX_DISTANCE
                          + " blocks of start_pos "
                          + round1(jobStartPos[0])
                          + "/"
                          + round1(jobStartPos[1])
                          + "/"
                          + round1(jobStartPos[2]))
                  + "; tracked entity_count="
                  + entityCount);
          return null; // fails ONLY this job; doRender skips it and moves on
        }
      } else {
        // Legacy single-job mode without a player id: prefer any recorded player's UUID →
        // spawn-packet entity id over the activity heuristic, as before.
        entityId = chooseEntityId(positions, duration);
        int playerEid = findPlayerEntityId(replayFile, null);
        if (playerEid >= 0 && positions != null) {
          detail.put("player_entity_id", playerEid);
          Map<?, ?> pim = innerMap(positions, playerEid);
          if (pim != null && !pim.isEmpty()) {
            entityId = playerEid;
            detail.put("entity_id_source", "player_uuid");
          } else {
            detail.put("player_entity_untracked", true);
          }
        }
      }
    }
    if (entityId >= 0 && positions != null) {
      Map<?, ?> im = innerMap(positions, entityId);
      detail.put("subject_samples", im == null ? 0 : im.size());
      detail.put("subject_span_ms", spanMs(im));
    }
    Object startLoc = entityId >= 0 ? sampleLocation(innerMap(positions, entityId), false) : null;

    Method addPos = method(sp.getClass(), "addPositionKeyframe", 8);
    Method addTime = method(sp.getClass(), "addTimeKeyframe", 2);

    // PRIMARY: first-person player POV. Spectator keyframes (entity id) + the
    // entity tracker make ReplayMod render from the entity's view and follow it.
    if (!"static".equals(cameraMode) && entityId >= 0 && startLoc != null) {
      Object endLoc = sampleLocation(innerMap(positions, entityId), true);
      if (endLoc == null) endLoc = startLoc;
      addPositionKeyframe(sp, addPos, 0L, startLoc, entityId);
      invoke(sp, addTime, Long.valueOf(0L), Integer.valueOf(0));
      addPositionKeyframe(sp, addPos, (long) duration, endLoc, entityId);
      invoke(sp, addTime, Long.valueOf(duration), Integer.valueOf(duration));
      detail.put("camera_mode", "spectator_pov");
      detail.put("entity_id_used", entityId);
      detail.put("keyframes_added", 2);
      this.cameraEntityId = entityId; // A.1: per-frame pose follows this entity
      // Park the live camera at the subject's first tracked position so frames rendered before
      // the spectator attach resolves aren't shot from wherever the camera happened to spawn.
      seedCameraAt(startLoc);
      return invoke(sp, method(sp.getClass(), "getTimeline", 0));
    }

    // Static fallbacks are NEVER acceptable pipeline output (every job is a player POV): unless
    // the operator explicitly requested a static camera (RM_CAMERA=static) or opted into the
    // debug fallback (RM_ALLOW_STATIC_CAMERA=1), fail the job loudly instead of silently
    // rendering a video whose camera never follows the player.
    if (!"static".equals(cameraMode) && !allowStatic) {
      cameraAttachFailure(
          "no followable entity for spectator camera (entity_id="
              + entityId
              + ", tracked_positions="
              + (startLoc != null)
              + ", entity_count="
              + entityCount
              + ")");
      return null; // fails ONLY this job; doRender skips it and moves on
    }

    // FALLBACK: a free (non-spectator) camera. Anchor it to a REAL recorded
    // position when one is available so even a chunk-area replay (no followable
    // player) frames real content instead of empty sky.
    Object anchor = startLoc;
    if (anchor == null && positions != null) {
      int subject = pickSubjectEntity(positions, duration);
      anchor = subject >= 0 ? sampleLocation(innerMap(positions, subject), false) : null;
    }
    double cx, cy, cz;
    float cyaw, cpitch;
    if (anchor != null) {
      cx = locD(anchor, "getX");
      cy = locD(anchor, "getY") + camDy;
      cz = locD(anchor, "getZ") + camDz;
      cyaw = 180.0f; // camera sits +Z of the anchor → look north (-Z) toward it
      cpitch = 20.0f; // tilt slightly down onto the anchor
      detail.put("camera_mode", "static_anchored");
      detail.put("anchor", round1(cx) + "/" + round1(cy) + "/" + round1(cz));
    } else {
      cx = camX;
      cy = camY;
      cz = camZ;
      cyaw = camYaw;
      cpitch = camPitch;
      detail.put("camera_mode", "static_fixed");
      detail.put("free_camera", camX + "/" + camY + "/" + camZ);
    }
    invoke(sp, addPos, Long.valueOf(0L), cx, cy, cz, cyaw, cpitch, 0.0f, Integer.valueOf(-1));
    invoke(sp, addTime, Long.valueOf(0L), Integer.valueOf(0));
    invoke(sp, addPos, Long.valueOf(duration), cx, cy, cz, cyaw, cpitch, 0.0f, Integer.valueOf(-1));
    invoke(sp, addTime, Long.valueOf(duration), Integer.valueOf(duration));
    detail.put("keyframes_added", 2);
    return invoke(sp, method(sp.getClass(), "getTimeline", 0));
  }

  /**
   * Build the observer timeline: one free-camera position keyframe per rendered frame, clamped to
   * the replay's real duration.
   *
   * <p>Dense keyframes are deliberate. ReplayMod interpolates between position keyframes, and every
   * interpolator it may use passes through its own control points, so placing a keyframe at every
   * frame time makes the rendered pose equal the requested pose at exactly the instants that get
   * encoded — no need to reproduce ReplayMod's spline anywhere else in the pipeline.
   *
   * <p>{@code VideoRenderer} renders the TIMELINE, not the replay, so the final pose is held out to
   * {@code duration}: without that a camera path shorter than the recording would silently produce
   * a shorter video than the player POVs of the same episode. Keyframes past {@code duration} are
   * dropped for the mirror-image reason.
   */
  private Object buildCameraPathTimeline(Object sp, int duration) throws Exception {
    Method addPos = method(sp.getClass(), "addPositionKeyframe", 8);
    Method addTime = method(sp.getClass(), "addTimeKeyframe", 2);
    long firstTime = Math.round(cameraPath.get(0)[0]);
    if (firstTime != 0L) {
      detail.put("camera_mode", "camera_path_invalid");
      fail(
          "camera_path_invalid",
          "camera path must start at time_ms=0 so the video starts at replay tick 0; got "
              + firstTime);
      return null;
    }
    int kept = 0;
    double[] last = null;
    for (double[] key : cameraPath) {
      long time = Math.round(key[0]);
      if (time > duration) break;
      invoke(
          sp,
          addPos,
          Long.valueOf(time),
          key[1],
          key[2],
          key[3],
          (float) key[4],
          (float) key[5],
          (float) key[6],
          Integer.valueOf(-1));
      last = key;
      kept++;
    }
    if (last == null) {
      detail.put("camera_mode", "camera_path_invalid");
      fail(
          "camera_path_invalid",
          "no camera-path keyframe falls within the replay duration " + duration + " ms");
      return null;
    }
    if (Math.round(last[0]) < duration) {
      invoke(
          sp,
          addPos,
          Long.valueOf(duration),
          last[1],
          last[2],
          last[3],
          (float) last[4],
          (float) last[5],
          (float) last[6],
          Integer.valueOf(-1));
      kept++;
    }
    invoke(sp, addTime, Long.valueOf(0L), Integer.valueOf(0));
    invoke(sp, addTime, Long.valueOf(duration), Integer.valueOf(duration));
    detail.put("camera_mode", "camera_path");
    detail.put("camera_path_clamped_keyframes", kept);
    detail.put("keyframes_added", kept);
    seedCameraAtPose(cameraPath.get(0));
    log("camera path: " + kept + " keyframe(s) over " + duration + " ms replay");
    return invoke(sp, method(sp.getClass(), "getTimeline", 0));
  }

  /**
   * Best-effort: park ReplayMod's live camera at the observer path's first pose before rendering
   * starts, the free-camera counterpart of {@link #seedCameraAt}. Never fails the render.
   */
  private void seedCameraAtPose(double[] key) {
    if (replayHandler == null) return;
    try {
      Object cam = invoke(replayHandler, method(replayHandler.getClass(), "getCameraEntity", 0));
      if (cam == null) {
        detail.put("camera_seeded", false);
        return;
      }
      invoke(cam, method(cam.getClass(), "setCameraPosition", 3), key[1], key[2], key[3]);
      invoke(
          cam,
          method(cam.getClass(), "setCameraRotation", 3),
          Float.valueOf((float) key[4]),
          Float.valueOf((float) key[5]),
          Float.valueOf((float) key[6]));
      detail.put("camera_seeded", true);
    } catch (Throwable t) {
      detail.put("camera_seeded", false);
      log("seedCameraAtPose failed (non-fatal): " + t);
    }
  }

  /**
   * Record a {@code camera_attach_failed} failure (the distinct class the orchestrator maps to a
   * failed render-manifest) against the CURRENT job only. {@link #buildTimeline()} then returns
   * null and {@link #doRender()} finishes just this job as failed and moves on to the next one — an
   * attach failure on one POV must never abort the whole multi-job session.
   */
  private void cameraAttachFailure(String msg) {
    detail.put("camera_mode", "attach_failed");
    fail(
        "camera_attach_failed",
        "cannot attach spectator camera: "
            + msg
            + " (set camera.allow_static in the driver spec to permit the static fallback while"
            + " debugging)");
  }

  /**
   * Positional fallback for camera-entity resolution: return the tracked entity whose FIRST
   * recorded position is nearest to {@code startPos} (the POV's first action-stream position),
   * provided it lies within {@link #START_POS_MAX_DISTANCE} blocks — the recorded player stands at
   * that exact spot when its recording starts. Records {@code entity_id_source=start_position} and
   * the match distance in the job detail. Returns -1 when start_pos is absent or nothing is close
   * enough.
   */
  private int findEntityIdByStartPos(double[] startPos, Map<?, ?> positions) {
    if (startPos == null || positions == null || positions.isEmpty()) return -1;
    detail.put(
        "start_pos", round1(startPos[0]) + "/" + round1(startPos[1]) + "/" + round1(startPos[2]));
    int nearestId = -1;
    double nearestDist = Double.MAX_VALUE;
    for (Map.Entry<?, ?> e : positions.entrySet()) {
      if (!(e.getKey() instanceof Integer) || !(e.getValue() instanceof Map)) continue;
      Object first = sampleLocation((Map<?, ?>) e.getValue(), false);
      if (first == null) continue;
      try {
        double dx = locD(first, "getX") - startPos[0];
        double dy = locD(first, "getY") - startPos[1];
        double dz = locD(first, "getZ") - startPos[2];
        double dist = Math.sqrt(dx * dx + dy * dy + dz * dz);
        if (dist < nearestDist) {
          nearestDist = dist;
          nearestId = (Integer) e.getKey();
        }
      } catch (Throwable t) {
        // unreadable location sample — skip this entity
      }
    }
    if (nearestId < 0) return -1;
    detail.put("start_pos_nearest_entity_id", nearestId);
    detail.put("start_pos_nearest_distance", round1(nearestDist));
    if (nearestDist > START_POS_MAX_DISTANCE) {
      log(
          "start-position fallback: nearest tracked entity "
              + nearestId
              + " is "
              + round1(nearestDist)
              + " blocks from start_pos (> "
              + START_POS_MAX_DISTANCE
              + "); not attaching");
      return -1;
    }
    detail.put("entity_id_source", "start_position");
    detail.put("start_pos_distance", round1(nearestDist));
    log(
        "start-position fallback: attaching to entity "
            + nearestId
            + " ("
            + round1(nearestDist)
            + " blocks from start_pos)");
    return nearestId;
  }

  /**
   * Resolve the job's {@code player_id} to the UUID the server assigned that player. It is either
   * already a UUID string, or a username whose offline-mode UUID is derived exactly like the
   * offline server does: {@code UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))}.
   */
  static java.util.UUID resolveTargetUuid(String usernameOrUuid) {
    String s = usernameOrUuid.trim();
    if (s.matches("[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")) {
      return java.util.UUID.fromString(s);
    }
    return java.util.UUID.nameUUIDFromBytes(
        ("OfflinePlayer:" + s).getBytes(StandardCharsets.UTF_8));
  }

  /**
   * Best-effort: move ReplayMod's live camera entity to the spectated entity's first tracked
   * position before rendering starts, so any frames rendered before the spectator attach resolves
   * aren't parked at the camera's spawn pose. Never fails the render.
   */
  private void seedCameraAt(Object loc) {
    if (loc == null || replayHandler == null) return;
    try {
      Object cam = invoke(replayHandler, method(replayHandler.getClass(), "getCameraEntity", 0));
      if (cam == null) {
        detail.put("camera_seeded", false);
        return;
      }
      invoke(
          cam,
          method(cam.getClass(), "setCameraPosition", 3),
          locD(loc, "getX"),
          locD(loc, "getY"),
          locD(loc, "getZ"));
      float yaw = (float) (Float) invoke(loc, method(loc.getClass(), "getYaw", 0));
      float pitch = (float) (Float) invoke(loc, method(loc.getClass(), "getPitch", 0));
      invoke(
          cam,
          method(cam.getClass(), "setCameraRotation", 3),
          Float.valueOf(yaw),
          Float.valueOf(pitch),
          Float.valueOf(0.0f));
      detail.put("camera_seeded", true);
    } catch (Throwable t) {
      detail.put("camera_seeded", false);
      log("seedCameraAt failed (non-fatal): " + t);
    }
  }

  /** The pose trace this driver writes; pawa merges it into the frame timeline it assembles. */
  private static final String POSE_TRACE_NAME = "poses.json";

  /**
   * Write the pose trace: where the camera actually was, at the instants this render sampled it.
   *
   * <p>This driver deliberately does NOT write {@code frames.jsonl}. A row of that file is a {@code
   * pawa_schema::dataset::FrameRow}, and the row count is the encoded video's frame count, which the
   * {@code pawa} binary measures with {@code ffprobe} after the render ({@code
   * crates/render/src/media.rs}). The pose is the only per-row input that is not arithmetic over
   * {@code fps}, that count and the capture's action stream. So {@code pawa} assembles the rows
   * ({@code assemble_frames} in {@code crates/align/src/frames.rs}, called from {@code
   * crates/pawa/src/units.rs}), and this driver contributes the one thing it alone can know: the
   * pose ReplayMod actually rendered from. One contract, one author.
   *
   * <p>The document is the SAME shape {@link #loadCameraPath} reads, so an observer's INPUT camera
   * path and a POV's OUTPUT trace are one format rather than two.
   *
   * <p>Poses are keyed by {@code time_ms}, never by frame index. {@code pawa} derives frame times
   * from {@code fps} with half-even rounding, and the sampling below uses Java's half-up; matching
   * by nearest time (within {@code MATCH_TOLERANCE_MS}) means those two roundings cannot disagree
   * about which frame a pose belongs to. Keying by index would turn a 1 ms difference into a
   * whole-frame error.
   *
   * <p>Best-effort: never fails the render.
   */
  private void writePoseTrace() {
    if (outputDir == null || fps <= 0) return;
    int durationMs = replayDurationMs > 0 ? replayDurationMs : 0;
    int frames = (int) Math.floor(durationMs * (double) fps / 1000.0);
    if (frames <= 0) frames = 1;

    JsonArray keyframes = new JsonArray();
    String source;
    if (!cameraPath.isEmpty()) {
      // OBSERVER: emit the very keyframes that were rendered, clamped to the replay the way
      // buildCameraPathTimeline clamped them, so the video and the trace are two views of one array.
      source = "camera_path";
      for (double[] key : cameraPath) {
        if (Math.round(key[0]) > durationMs && durationMs > 0) break;
        keyframes.add(pose(Math.round(key[0]), key[1], key[2], key[3], key[4], key[5], key[6]));
      }
    } else {
      source = "entity_tracker";
      Method posAt = null;
      if (cameraEntityId >= 0 && positionTracker != null) {
        try {
          posAt = method(positionTracker.getClass(), "getEntityPositionAtTimestamp", 2);
        } catch (Throwable t) {
          log(POSE_TRACE_NAME + ": getEntityPositionAtTimestamp unavailable (" + t + ")");
        }
      }
      if (posAt != null) {
        // SPECTATOR POV: sample at each frame instant, which is the pose ReplayMod interpolated to.
        for (int i = 0; i < frames; i++) {
          long timeMs = Math.round(i * 1000.0 / fps);
          try {
            Object loc =
                invoke(
                    positionTracker, posAt, Integer.valueOf(cameraEntityId), Long.valueOf(timeMs));
            if (loc == null) continue;
            // All six components as doubles, so one pose carries one precision (CameraPose is
            // all f64 on the Rust side).
            keyframes.add(
                pose(
                    timeMs,
                    locD(loc, "getX"),
                    locD(loc, "getY"),
                    locD(loc, "getZ"),
                    (Float) invoke(loc, method(loc.getClass(), "getYaw", 0)),
                    (Float) invoke(loc, method(loc.getClass(), "getPitch", 0)),
                    0.0)); // an entity pose has no roll
          } catch (Throwable t) {
            // omit this instant (non-fatal); pawa reports the resulting pose gap
          }
        }
      }
    }

    JsonObject root = new JsonObject();
    root.addProperty("fps", fps);
    root.addProperty("replay_duration_ms", durationMs);
    root.addProperty("source", source);
    root.add("keyframes", keyframes);
    try {
      Files.writeString(
          outputDir.resolve(POSE_TRACE_NAME), root.toString(), StandardCharsets.UTF_8);
      detail.put("pose_trace_written", true);
      detail.put("pose_trace_keyframes", keyframes.size());
      detail.put("pose_trace_source", source);
      detail.put("pose_trace_camera_entity", cameraEntityId);
      log("wrote " + POSE_TRACE_NAME + ": " + keyframes.size() + " pose(s) from " + source);
    } catch (IOException e) {
      detail.put("pose_trace_written", false);
      detail.put("pose_trace_error", e.toString());
      log("could not write " + POSE_TRACE_NAME + ": " + e);
    }
  }

  /** One pose-trace keyframe, in the same field order the camera-path document uses. */
  private static JsonObject pose(
      long timeMs, double x, double y, double z, double yaw, double pitch, double roll) {
    JsonObject o = new JsonObject();
    o.addProperty("time_ms", timeMs);
    o.addProperty("x", x);
    o.addProperty("y", y);
    o.addProperty("z", z);
    o.addProperty("yaw", yaw);
    o.addProperty("pitch", pitch);
    o.addProperty("roll", roll);
    return o;
  }

  private void addPositionKeyframe(Object sp, Method addPos, long time, Object loc, int entityId)
      throws Exception {
    double x = (double) (Double) invoke(loc, method(loc.getClass(), "getX", 0));
    double y = (double) (Double) invoke(loc, method(loc.getClass(), "getY", 0));
    double z = (double) (Double) invoke(loc, method(loc.getClass(), "getZ", 0));
    float yaw = (float) (Float) invoke(loc, method(loc.getClass(), "getYaw", 0));
    float pitch = (float) (Float) invoke(loc, method(loc.getClass(), "getPitch", 0));
    invoke(sp, addPos, Long.valueOf(time), x, y, z, yaw, pitch, 0.0f, Integer.valueOf(entityId));
  }

  /**
   * Read EntityPositionTracker.entityPositions (Map&lt;entityId, NavigableMap&lt;time,
   * Location&gt;&gt;).
   */
  private Map<?, ?> positionsMap(Object tracker) {
    try {
      Field f = tracker.getClass().getDeclaredField("entityPositions");
      f.setAccessible(true);
      Object m = f.get(tracker);
      return (m instanceof Map) ? (Map<?, ?>) m : null;
    } catch (Throwable t) {
      log("could not read entityPositions: " + t);
      return null;
    }
  }

  private static Map<?, ?> innerMap(Map<?, ?> positions, int entityId) {
    if (positions == null) return null;
    Object v = positions.get(entityId);
    return (v instanceof Map) ? (Map<?, ?>) v : null;
  }

  private static long spanMs(Map<?, ?> inner) {
    if (inner instanceof java.util.NavigableMap<?, ?> nm && !nm.isEmpty()) {
      return ((Number) nm.lastKey()).longValue() - ((Number) nm.firstKey()).longValue();
    }
    return 0L;
  }

  /**
   * Choose the camera subject. Among entities present for at least half the recording (long-lived →
   * the recorded player, not a transient mob the player glanced at), pick the most active (most
   * position samples → moved the most). Falls back to the widest-span entity when nothing is
   * long-lived (e.g. a chunk-area capture whose entities are all transient/ambient).
   */
  private static int pickSubjectEntity(Map<?, ?> positions, int durationMs) {
    long minSpan = Math.max(1L, durationMs / 2L);
    int best = -1, bestSamples = -1;
    int widest = -1;
    long widestSpan = -1L;
    for (Map.Entry<?, ?> e : positions.entrySet()) {
      if (!(e.getKey() instanceof Integer) || !(e.getValue() instanceof Map)) continue;
      int id = (Integer) e.getKey();
      Map<?, ?> inner = (Map<?, ?>) e.getValue();
      int samples = inner.size();
      long span = spanMs(inner);
      if (span >= minSpan && samples > bestSamples) {
        bestSamples = samples;
        best = id;
      }
      if (span > widestSpan) {
        widestSpan = span;
        widest = id;
      }
    }
    return best != -1 ? best : widest;
  }

  /**
   * The activity heuristic, for legacy single-job mode only. Reached only when {@code
   * spectateEntityId < 0} — {@link #buildTimeline()} takes the explicit-id branch before this — so
   * there is no env override to re-check here.
   */
  private int chooseEntityId(Map<?, ?> positions, int durationMs) {
    if (positions == null) return -1;
    detail.put("entity_id_source", "auto");
    return pickSubjectEntity(positions, durationMs);
  }

  /** A concrete recorded Location sample (first or last) from an entity's position map. */
  private static Object sampleLocation(Map<?, ?> inner, boolean last) {
    if (inner == null || inner.isEmpty()) return null;
    if (inner instanceof java.util.NavigableMap<?, ?> nm) {
      return last ? nm.lastEntry().getValue() : nm.firstEntry().getValue();
    }
    Object v = null;
    for (Object x : inner.values()) {
      v = x;
      if (!last) break;
    }
    return v;
  }

  private double locD(Object loc, String getter) throws Exception {
    return (double) (Double) invoke(loc, method(loc.getClass(), getter, 0));
  }

  private static double round1(double v) {
    return Math.round(v * 10.0) / 10.0;
  }

  /**
   * The parameter types the positional fill in {@link #buildRenderSettings} assumes, in order.
   *
   * <p>Arity alone does not pin the contract down. A reordered or newly inserted parameter keeps the
   * count at 22 and is otherwise invisible — the {@code int} and {@code boolean} runs are freely
   * interchangeable, so swapping {@code depthMap} for {@code cameraPathExport} would render happily
   * and hand back the wrong artifact. Checking the types turns that into a named failure.
   *
   * <p>{@code "enum"} matches any enum, because the constants are resolved from the parameter type
   * itself and the enum's class name and package may drift freely. {@code "ref"} matches any
   * reference type (position 12 is a ReadableColor we always pass null for).
   */
  private static final String[] RENDER_SETTINGS_SHAPE = {
    "enum", "enum", "int", "int", "int", "int", "java.io.File", "boolean", "boolean", "boolean",
    "boolean", "boolean", "ref", "int", "int", "boolean", "boolean", "boolean", "enum",
    "java.lang.String", "java.lang.String", "boolean",
  };

  /**
   * Describe how {@code pt} departs from {@link #RENDER_SETTINGS_SHAPE}, or null when it matches.
   * Without this check a drifted constructor either throws deep inside {@code newInstance} as an
   * unreadable {@code client_crash}, or accepts the arguments and renders the wrong settings.
   */
  static String renderSettingsShapeError(Class<?>[] pt) {
    for (int i = 0; i < RENDER_SETTINGS_SHAPE.length; i++) {
      String want = RENDER_SETTINGS_SHAPE[i];
      Class<?> got = pt[i];
      boolean ok =
          switch (want) {
            case "enum" -> got.isEnum();
            case "ref" -> !got.isPrimitive();
            default -> want.equals(got.getName());
          };
      if (!ok) {
        return "RenderSettings constructor parameter "
            + i
            + " is "
            + got.getName()
            + ", expected "
            + want
            + "; ReplayMod's constructor has drifted, so filling it positionally would mean"
            + " something other than what this driver intends";
      }
    }
    return null;
  }

  private Object buildRenderSettings(Projection proj) throws Exception {
    Class<?> cls = Class.forName("com.replaymod.render.RenderSettings");
    Constructor<?> ctor = ctorWithArity(cls, 22);
    Class<?>[] pt = ctor.getParameterTypes();
    String drift = renderSettingsShapeError(pt);
    if (drift != null) {
      // fail() keeps the FIRST failure, and doRender's catch preserves an already-set class, so
      // this surfaces as replaymod_api_drift rather than a generic client_crash.
      fail("replaymod_api_drift", drift);
      throw new IllegalStateException(drift);
    }

    Object renderMethodEnum = enumValue(pt[0], proj.method);
    boolean spherical = isSphericalMethod(renderMethodEnum, proj.method);
    // A spherical projection renders six cube faces per frame, so a HUD drawn into it is drawn six
    // times and smeared around the panorama. The HUD belongs to the flat POV render only, and
    // HudUnhideMixin reads this to decide whether to undo ReplayMod's F1 toggle for these frames.
    hudInFrame = !spherical;
    String preset = depthPass ? "PNG" : ("png".equals(encoding) ? "PNG" : "MP4_CUSTOM");
    File outputFile =
        outputDir.resolve(depthPass ? proj.name : ("png".equals(encoding) ? proj.name : proj.name + ".mp4")).toFile();
    outputFile.getParentFile().mkdirs();
    String exportArguments =
        depthPass || "png".equals(encoding)
            ? null
            : "-y -f rawvideo -pix_fmt bgra -s %WIDTH%x%HEIGHT% -r %FPS% -i - %FILTERS%"
                + " -an -c:v "
                + proj.encoder
                + " -pix_fmt yuv420p \"%FILENAME%\"";
    detail.put("output_file", outputFile.toString());


    // Positional fill matching the stock 22-arg constructor; enums are resolved
    // from the constructor's own parameter types so we never hard-code enum
    // class names (robust to package/version drift).
    Object[] a = new Object[22];
    a[0] = renderMethodEnum; // RenderMethod
    a[1] = enumValue(pt[1], preset); // EncodingPreset
    a[2] = proj.width;
    a[3] = proj.height;
    a[4] = fps;
    a[5] = 0; // ignored by MP4_CUSTOM because exportArguments selects CRF mode
    a[6] = outputFile;
    a[7] = false; // renderNameTags
    a[8] = depthPass; // includeAlphaChannel; depth stores float32 byte 3 in PNG alpha
    // Spherical projections lock the camera orientation entirely: a 360 frame's reference
    // direction must not wobble with the player's view (the pose stream carries the view;
    // the panorama carries the world). Flat projections keep following the camera entity.
    a[9] = spherical; // stabilizeYaw
    a[10] = spherical; // stabilizePitch
    a[11] = spherical; // stabilizeRoll
    a[12] = null; // chromaKeyingColor (ReadableColor)
    a[13] = proj.sphericalFovX; // sphericalFovX (spherical methods only)
    a[14] = proj.sphericalFovY; // sphericalFovY (spherical methods only)
    a[15] = spherical; // injectSphericalMetadata (EQUIRECTANGULAR/ODS)
    a[16] = depthPass; // depthMap
    a[17] = false; // cameraPathExport
    a[18] = enumValue(pt[18], proj.antiAliasing); // AntiAliasing
    a[19] = ""; // exportCommand
    a[20] = exportArguments; // exportArguments (null for PNG)
    a[21] = true; // highPerformance
    ctor.setAccessible(true);
    return ctor.newInstance(a);
  }

  /**
   * True when ReplayMod treats {@code method} as spherical (360) → inject spherical metadata.
   * Prefers the enum's own {@code isSpherical()} (version-proof); falls back to the known set.
   */
  private static boolean isSphericalMethod(Object renderMethodEnum, String name) {
    try {
      Object r = invoke(renderMethodEnum, method(renderMethodEnum.getClass(), "isSpherical", 0));
      if (r instanceof Boolean b) {
        return b;
      }
    } catch (Throwable ignored) {
      // older / renamed API: fall back to the static set
    }
    return "EQUIRECTANGULAR".equals(name) || "ODS".equals(name);
  }

  // ---- output / world detection --------------------------------------------

  /**
   * Wait for THIS projection's media to appear and stop growing.
   *
   * <p>Measuring the whole output dir would fold in the projections already rendered for this job.
   * Their bytes are constant, so a total that has not moved for two polls reads as "stable" even
   * while ffmpeg has yet to write the first byte of the current projection.
   */
  private boolean waitForOutputPresent(Projection proj) {
    long deadline = System.currentTimeMillis() + 10_000L;
    long lastSize = -1L;
    int stablePolls = 0;
    while (System.currentTimeMillis() <= deadline) {
      long size = projectionBytes(proj);
      if (size > 0) {
        if (size == lastSize) {
          stablePolls++;
          if (stablePolls >= 2) {
            detail.put(
                "output_wait_ms_" + proj.name,
                10_000L - Math.max(0L, deadline - System.currentTimeMillis()));
            detail.put("output_bytes_" + proj.name, size);
            detail.put("output_size_stable_" + proj.name, true);
            return true;
          }
        } else {
          stablePolls = 0;
          lastSize = size;
        }
      }
      try {
        Thread.sleep(500L);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
        break;
      }
    }
    detail.put("output_wait_ms_" + proj.name, 10_000L);
    detail.put("output_bytes_" + proj.name, Math.max(0L, lastSize));
    detail.put("output_size_stable_" + proj.name, false);
    return projectionBytes(proj) > 0L;
  }

  /**
   * Bytes of one projection's own output — its {@code <name>.mp4}, or the {@code <name>} directory
   * ReplayMod fills with PNGs.
   */
  private long projectionBytes(Projection proj) {
    Path target = outputDir.resolve(depthPass ? proj.name : ("png".equals(encoding) ? proj.name : proj.name + ".mp4"));
    try {
      if (Files.isRegularFile(target)) return Files.size(target);
      if (!Files.isDirectory(target)) return 0L;
      try (var s = Files.walk(target)) {
        return s.filter(Files::isRegularFile).mapToLong(p -> p.toFile().length()).sum();
      }
    } catch (IOException e) {
      return 0L;
    }
  }

  /** Bytes of every media file under the job's output dir; the job-level "produced anything" gate. */
  private long outputBytes() {
    try {
      if (!Files.isDirectory(outputDir)) return 0L;
      try (var s = Files.walk(outputDir)) {
        return s.filter(
                p -> {
                  String n = p.getFileName().toString().toLowerCase();
                  return (n.endsWith(".png") || n.endsWith(".mp4")) && p.toFile().isFile();
                })
            .mapToLong(p -> p.toFile().length())
            .sum();
      }
    } catch (IOException e) {
      return 0L;
    }
  }

  private boolean outputPresent() {
    return outputBytes() > 0L;
  }

  /** Client-world readiness strong enough to reject the replay-opened-but-still-void state. */
  private WorldReadiness inspectWorldReadiness() {
    try {
      Object mc = staticInvoke("com.replaymod.core.versions.MCVer", "getMinecraft");
      if (mc == null) return new WorldReadiness(false, false, 0, 0, "minecraft_null");
      Object world = fieldOrNull(mc, "level", "world");
      if (world == null) return new WorldReadiness(true, false, 0, 0, "world_null");

      Object chunkSource = invokeFirstNoArg(world, "getChunkSource");
      Object loadedValue = invokeFirstNoArg(chunkSource, "getLoadedChunksCount");
      Object levelRenderer = fieldOrNull(mc, "levelRenderer");
      Object built = invokeFirstNoArg(levelRenderer, "visibleSections");
      boolean available = loadedValue instanceof Number && built != null;
      return new WorldReadiness(
          available,
          true,
          numberValue(loadedValue),
          collectionSize(built),
          available ? "chunk_source+level_renderer" : "mapping_unresolved");
    } catch (Throwable t) {
      return new WorldReadiness(false, false, 0, 0, "probe_error:" + t.getClass().getSimpleName());
    }
  }

  private record WorldReadiness(
      boolean probeAvailable, boolean worldPresent, int loadedChunks, int builtChunks, String probe) {}


  /**
   * Close any open screen before rendering ({@code mc.setScreen(null)}). {@code
   * setScreen} is remapped at runtime, so try a few candidate names and swallow failure — rendering
   * may still work if no screen is open.
   */
  private void tryCloseScreen(Object mc) {
    if (mc == null) return;
    for (Method m : mc.getClass().getMethods()) {
      if (m.getReturnType() == void.class
          && m.getParameterCount() == 1
          && (m.getName().equals("setScreen")
              || m.getName().equals("openScreen")
              || m.getName().equals("method_1507"))) {
        try {
          m.setAccessible(true);
          m.invoke(mc, new Object[] {null});
          detail.put("closed_screen_via", m.getName());
          return;
        } catch (Throwable ignored) {
          /* try next candidate */
        }
      }
    }
  }

  /**
   * Stop the game once the session is terminal. Everything that matters is already on disk — the
   * status file is written and every output was confirmed stable before DONE/FAILED — so every
   * further second the client lives is render-lane time wasted: the orchestrator waits out its
   * full 120 s shutdown grace before SIGTERMing a client that never exits (measured on every
   * render of every fleet).
   *
   * <p>Two layers. A clean {@code MinecraftClient.scheduleStop()} via reflection lets the game
   * loop wind down normally; the name candidates survive remapping. The daemon backstop halts the
   * JVM if the process is somehow still alive shortly after — the reflection missed, the shutdown
   * hung, or a non-daemon thread (HeadlessMC's console) outlived the game loop. halt() rather
   * than exit(): nothing after this point is worth blocking on a shutdown hook for.
   */
  private void requestQuit() {
    log("terminal phase reached (" + phase + "); stopping the client");
    active = false;
    try {
      Object mc = staticInvoke("com.replaymod.core.versions.MCVer", "getMinecraft");
      // This image launches with Mojang official mappings (the runtime class is
      // net.minecraft.client.Minecraft), where the schedule-a-clean-stop method is `stop()`.
      // The order matters: in a yarn-named runtime `stop()` is the TEARDOWN (mojmap `destroy`),
      // so the unambiguous names go first and `stop` is the last resort — under mojmap the first
      // two simply miss and `stop` is the running=false setter we want.
      Method stop = null;
      for (String name : new String[] {"scheduleStop", "method_1592", "stop"}) {
        try {
          stop = method(mc.getClass(), name, 0);
          break;
        } catch (NoSuchMethodException ignored) {
          // try the next mapping candidate
        }
      }
      if (stop != null) {
        invoke(mc, stop);
        log("clean stop scheduled via " + stop.getName());
      } else {
        log("no scheduleStop candidate on " + mc.getClass().getName() + "; JVM backstop will halt");
      }
    } catch (Throwable t) {
      log("scheduleStop failed (" + t + "); JVM backstop will halt");
    }
    Thread backstop =
        new Thread(
            () -> {
              try {
                Thread.sleep(20_000);
              } catch (InterruptedException ignored) {
                return;
              }
              Runtime.getRuntime().halt(0);
            },
            "rm-driver-quit-backstop");
    backstop.setDaemon(true);
    backstop.start();
  }

  /**
   * Find the recorded player's entity id by matching player UUIDs against the player spawn packets
   * in the recording, using stock ReplayStudio.
   * When {@code targetUuid} is non-null ONLY that UUID matches (the job's own POV owner: in a
   * multi-bot recording metaData lists every player, and first-match would attach the camera to the
   * wrong bot). When null (legacy, no player id) any UUID from {@code metaData.getPlayers()}
   * matches, as before. Returns -1 on any failure.
   */
  private static int findPlayerEntityId(Object replayFile, java.util.UUID targetUuid) {
    Object in = null;
    try {
      Object metaData = invoke(replayFile, method(replayFile.getClass(), "getMetaData", 0));
      java.util.Set<String> want = new java.util.HashSet<>();
      if (targetUuid != null) {
        want.add(targetUuid.toString().toLowerCase().replace("-", ""));
      } else {
        Object playersArr = invoke(metaData, method(metaData.getClass(), "getPlayers", 0));
        if (!(playersArr instanceof String[])) return -1;
        String[] players = (String[]) playersArr;
        if (players.length == 0) return -1;
        for (String p : players) want.add(p.toLowerCase().replace("-", ""));
      }
      // getProtocolVersion() returns a ViaVersion ProtocolVersion object (not int);
      // PacketTypeRegistry.get(ProtocolVersion, State) takes it directly.
      Object protocolVersion =
          invoke(metaData, method(metaData.getClass(), "getProtocolVersion", 0));

      Class<?> stateCls =
          Class.forName("com.replaymod.replaystudio.lib.viaversion.api.protocol.packet.State");
      Object loginState = enumValue(stateCls, "LOGIN");
      Class<?> registryCls =
          Class.forName("com.replaymod.replaystudio.protocol.PacketTypeRegistry");
      Method getMethod = null;
      for (Method m : registryCls.getMethods()) {
        if (m.getName().equals("get")
            && m.getParameterCount() == 2
            && m.getParameterTypes()[0].isAssignableFrom(protocolVersion.getClass())) {
          getMethod = m;
          break;
        }
      }
      if (getMethod == null) return -1;
      Object registry = getMethod.invoke(null, protocolVersion, loginState);

      Class<?> ptCls = Class.forName("com.replaymod.replaystudio.protocol.PacketType");
      Object spawnPlayer = enumValueOrNull(ptCls, "SpawnPlayer");
      Object spawnObject = enumValueOrNull(ptCls, "SpawnObject");
      Object v1202 =
          Class.forName(
                  "com.replaymod.replaystudio.lib.viaversion.api.protocol.version.ProtocolVersion")
              .getField("v1_20_2")
              .get(null);
      Class<?> putilCls = Class.forName("com.replaymod.replaystudio.util.PacketUtils");
      Method getEntityId = method(putilCls, "getEntityId", 1);
      Class<?> pspCls =
          Class.forName("com.replaymod.replaystudio.protocol.packets.PacketSpawnPlayer");
      Method getPlayerListEntryId = method(pspCls, "getPlayerListEntryId", 1);

      in = invoke(replayFile, method(replayFile.getClass(), "getPacketData", 1), registry);
      Method readPacket = method(in.getClass(), "readPacket", 0);
      while (true) {
        Object pd = readPacket.invoke(in);
        if (pd == null) break;
        Object packet = invoke(pd, method(pd.getClass(), "getPacket", 0));
        try {
          Object type = invoke(packet, method(packet.getClass(), "getType", 0));
          boolean isSpawnPlayer = spawnPlayer != null && type == spawnPlayer;
          boolean isSpawnObject =
              spawnObject != null
                  && type == spawnObject
                  && (boolean)
                      (Boolean) invoke(packet, method(packet.getClass(), "atLeast", 1), v1202);
          if (isSpawnPlayer || isSpawnObject) {
            Object eid = getEntityId.invoke(null, packet);
            Object uuid = getPlayerListEntryId.invoke(null, packet);
            if (eid != null
                && uuid != null
                && want.contains(uuid.toString().toLowerCase().replace("-", ""))) {
              return (int) (Integer) eid;
            }
          }
        } finally {
          try {
            invoke(packet, method(packet.getClass(), "release", 0));
          } catch (Throwable ignored) {
            /* best effort */
          }
        }
      }
    } catch (Throwable t) {
      log("findPlayerEntityId failed (falling back to heuristic): " + t);
    } finally {
      if (in != null) {
        try {
          invoke(in, method(in.getClass(), "close", 0));
        } catch (Throwable ignored) {
          /* best effort */
        }
      }
    }
    return -1;
  }

  // ---- status + logging -----------------------------------------------------

  private void transition(Phase next) {
    log("phase " + phase + " -> " + next + " @tick " + tickCounter);
    phase = next;
    phaseEnteredTick = tickCounter;
  }

  /** Record the FIRST failure for the current job (later ones are logged but never overwrite it). */
  private void fail(String cls, String msg) {
    Job job = cur();
    if (job == null) {
      if (preflightFailureClass == null) {
        preflightFailureClass = cls;
        preflightFailureMessage = msg;
      }
    } else if (job.failureClass == null) {
      job.failureClass = cls;
      job.failureMessage = msg;
    }
    log("FAILURE[" + cls + "]: " + msg);
  }

  /**
   * Write the aggregate session status. {@code phase} is the SESSION phase (reaches DONE/FAILED
   * only once ALL jobs are processed). Top-level {@code failure_class}/{@code failure_message}/
   * {@code detail} mirror the CURRENT (or last-processed) job for single-job back-compat, and a
   * {@code jobs} array carries each job's per-POV status so the orchestrator can map outputs.
   */
  private boolean writeStatus() {
    if (statusPath == null) {
      // A spec that failed to parse may not have yielded a status path; the log is all there is.
      log("no status path; status not written");
      return false;
    }
    Job current = cur();
    String cls = failureClass();
    String msg = failureMessage();
    StringBuilder sb = new StringBuilder();
    sb.append("{\n");
    sb.append("  \"phase\": ").append(quote(phase.name())).append(",\n");
    sb.append("  \"tick\": ").append(tickCounter).append(",\n");
    sb.append("  \"job_count\": ").append(jobs.size()).append(",\n");
    sb.append("  \"job_index\": ").append(jobIndex).append(",\n");
    sb.append("  \"failure_class\": ").append(cls == null ? "null" : quote(cls)).append(",\n");
    sb.append("  \"failure_message\": ").append(msg == null ? "null" : quote(msg)).append(",\n");
    appendDetail(sb, "  ", current != null ? current.detail : detail);
    sb.append(",\n  \"jobs\": [");
    for (int i = 0; i < jobs.size(); i++) {
      Job j = jobs.get(i);
      if (i > 0) sb.append(",");
      sb.append("\n    {\n");
      sb.append("      \"index\": ").append(i).append(",\n");
      sb.append("      \"player_id\": ")
          .append(j.playerId == null ? "null" : quote(j.playerId))
          .append(",\n");
      sb.append("      \"replay_path\": ").append(quote(j.replayPath.toString())).append(",\n");
      sb.append("      \"out_path\": ").append(quote(j.outputDir.toString())).append(",\n");
      sb.append("      \"status\": ").append(quote(j.status)).append(",\n");
      sb.append("      \"failure_class\": ")
          .append(j.failureClass == null ? "null" : quote(j.failureClass))
          .append(",\n");
      sb.append("      \"failure_message\": ")
          .append(j.failureMessage == null ? "null" : quote(j.failureMessage))
          .append(",\n");
      // Per-job detail (camera_mode/entity_id_used/entity_id_source/...): the orchestrator reads
      // it from THIS jobs array (the top-level detail only mirrors the last-processed job).
      appendDetail(sb, "      ", j.detail);
      sb.append("\n    }");
    }
    sb.append(jobs.isEmpty() ? "]\n" : "\n  ]\n");
    sb.append("}\n");
    try {
      Files.writeString(statusPath, sb.toString(), StandardCharsets.UTF_8);
      return true;
    } catch (IOException e) {
      log("could not write status file: " + e);
      return false;
    }
  }

  /**
   * The terminal status write is the one the orchestrator's verdict hangs on — a swallowed
   * IOException here (disk full, FUSE hiccup) followed by a prompt quit would read as
   * client_crash with the real cause lost. Retry briefly; on persistent failure quit anyway,
   * explicitly — nothing running on a broken disk gets healthier by waiting.
   */
  private void writeTerminalStatus() {
    for (int attempt = 0; attempt < 3; attempt++) {
      if (writeStatus()) {
        return;
      }
      try {
        Thread.sleep(500);
      } catch (InterruptedException e) {
        Thread.currentThread().interrupt();
        return;
      }
    }
    log("terminal status write failed after retries; quitting regardless");
  }

  private static void appendDetail(StringBuilder sb, String indent, Map<String, Object> detail) {
    sb.append(indent).append("\"detail\": {");
    boolean first = true;
    for (Map.Entry<String, Object> e : detail.entrySet()) {
      if (!first) sb.append(",");
      first = false;
      sb.append("\n")
          .append(indent)
          .append("  ")
          .append(quote(e.getKey()))
          .append(": ")
          .append(jsonValue(e.getValue()));
    }
    sb.append(detail.isEmpty() ? "}" : "\n" + indent + "}");
  }

  private static String jsonValue(Object v) {
    if (v == null) return "null";
    if (v instanceof Number || v instanceof Boolean) return v.toString();
    return quote(String.valueOf(v));
  }

  static String quote(String s) {
    StringBuilder sb = new StringBuilder("\"");
    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      switch (c) {
        case '"' -> sb.append("\\\"");
        case '\\' -> sb.append("\\\\");
        case '\n' -> sb.append("\\n");
        case '\r' -> sb.append("\\r");
        case '\t' -> sb.append("\\t");
        default -> {
          if (c < 0x20) sb.append(String.format("\\u%04x", (int) c));
          else sb.append(c);
        }
      }
    }
    return sb.append("\"").toString();
  }

  /**
   * The throwable and its whole cause chain. Every reflective call arrives wrapped in
   * {@link java.lang.reflect.InvocationTargetException}, whose own trace stops at
   * {@code Method.invoke} — without the chain the actual failure is invisible.
   */
  private static String stack(Throwable t) {
    StringBuilder sb = new StringBuilder();
    for (Throwable c = t; c != null && sb.length() <= 3600; c = c.getCause()) {
      sb.append(sb.length() == 0 ? "" : " ;; caused by ").append(c);
      for (StackTraceElement el : c.getStackTrace()) {
        sb.append(" | ").append(el.toString());
        if (sb.length() > 3600) break;
      }
    }
    return sb.toString();
  }

  private static void log(String msg) {
    System.out.println(LOG_PREFIX + msg);
    System.out.flush();
  }

  // ---- driver-spec accessors ------------------------------------------------
  //
  // All required, all throwing. The producer (pawa, serializing pawa_schema::driver::DriverSpec)
  // writes every field, so an absent one means the two ends have diverged. There is no `…OrDefault`
  // here on purpose: a default in the reader is a second place for a value to come from.

  private static String reqStr(JsonObject object, String key) {
    JsonElement element = object.get(key);
    if (element == null || element.isJsonNull()) {
      throw new IllegalArgumentException("driver spec missing " + key);
    }
    return element.getAsString();
  }

  private static int reqInt(JsonObject object, String key) {
    JsonElement element = object.get(key);
    if (element == null || element.isJsonNull()) {
      throw new IllegalArgumentException("driver spec missing " + key);
    }
    return element.getAsInt();
  }

  private static JsonArray reqArray(JsonObject object, String key) {
    JsonArray array = object.getAsJsonArray(key);
    if (array == null) throw new IllegalArgumentException("driver spec missing " + key + " array");
    return array;
  }
}
