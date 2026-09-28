package moe.siki.pawa.rmdriver.mixin;

import java.lang.reflect.Field;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Keep the client's local game mode off SPECTATOR, which is what a replay logs in as.
 *
 * <p>Every element of the player HUD is gated on it: {@code Hud.extractHotbarAndDecorations} sends
 * a spectator to the (empty) spectator menu instead of the item hotbar, {@code canHurtPlayer()}
 * decides whether hearts exist at all, and {@code extractCrosshair} draws nothing unless the
 * spectator happens to be pointing at something. Un-hiding the HUD therefore buys nothing on its
 * own — the frame renders a spectator's HUD, which is empty.
 *
 * <p>The three accessors all read one field, so pinning the field is one edit rather than three
 * overrides, and pinning it on every {@code setLocalMode} means a respawn packet mid-replay cannot
 * quietly take the HUD away again. The camera itself is unaffected: ReplayMod's {@code
 * CameraEntity.isSpectator()} answers from the replay handler, not from this field.
 *
 * <p>Reflection rather than a typed assignment because the driver compiles without a Minecraft jar
 * on the classpath; the constant is resolved once and cached.
 */
@Mixin(targets = "net.minecraft.client.multiplayer.MultiPlayerGameMode")
public class SurvivalModeMixin {
  private static Object rmdriver$survival;

  @Inject(method = "setLocalMode", at = @At("RETURN"))
  @SuppressWarnings({"unchecked", "rawtypes"})
  private void rmdriver$pinSurvival(CallbackInfo ci) {
    try {
      if (rmdriver$survival == null) {
        rmdriver$survival =
            Enum.valueOf((Class) Class.forName("net.minecraft.world.level.GameType"), "SURVIVAL");
      }
      Field mode = this.getClass().getDeclaredField("localPlayerMode");
      mode.setAccessible(true);
      mode.set(this, rmdriver$survival);
    } catch (ReflectiveOperationException e) {
      throw new IllegalStateException("rmdriver: cannot pin the local game mode off SPECTATOR", e);
    }
  }
}
