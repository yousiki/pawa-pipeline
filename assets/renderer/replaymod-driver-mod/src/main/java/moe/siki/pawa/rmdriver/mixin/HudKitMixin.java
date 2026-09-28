package moe.siki.pawa.rmdriver.mixin;

import moe.siki.pawa.rmdriver.ReplayModDriver;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Reconstruct the spectated player's hotbar before the HUD extracts its render state.
 *
 * <p>Same injection point as {@link HudUnhideMixin}, for the same reason: after {@code
 * extractRenderState} returns, the hotbar's render nodes are already built from whatever the
 * inventory held. The work itself — painting the capture kit into the spectated remote player's
 * inventory and deriving the selected slot from its synced mainhand — lives in {@code
 * moe.siki.pawa.rmdriver.HudKit}, which explains why a remote player's hotbar is otherwise one
 * morphing slot.
 *
 * <p>Gated on {@link ReplayModDriver#hudInFrame} exactly like the un-hide: only the flat POV
 * projection draws a HUD, so spherical frames never pay for the reflection.
 *
 * <p>The target class is named by string because 26.x ships unobfuscated and the driver compiles
 * without a Minecraft jar on the classpath.
 */
@Mixin(targets = "net.minecraft.client.gui.Hud")
public class HudKitMixin {
  @Inject(method = "extractRenderState", at = @At("HEAD"))
  private void rmdriver$paintKit(CallbackInfo ci) {
    if (ReplayModDriver.hudInFrame) {
      moe.siki.pawa.rmdriver.HudKit.paint();
    }
  }
}
