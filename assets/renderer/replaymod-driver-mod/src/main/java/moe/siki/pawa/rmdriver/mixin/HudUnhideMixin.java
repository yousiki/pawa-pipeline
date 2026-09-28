package moe.siki.pawa.rmdriver.mixin;

import moe.siki.pawa.rmdriver.ReplayModDriver;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Undo the F1 toggle ReplayMod wraps around every frame it renders.
 *
 * <p>{@code EntityRendererHandler} calls {@code hud.toggle()} before the offscreen frame and again
 * after it, so {@code Hud.extractRenderState} runs with {@code isHidden} true. That flag both skips
 * every HUD element and lands in {@code guiRenderState.isHudHidden}, which {@code
 * GameRenderer.renderItemInHand} reads as its own gate — so one flag decides whether the frame has a
 * HUD *and* whether it has a hand. Forcing it false at the point both consumers derive from is the
 * only place a flag-level fix works: after extract returns, the HUD's render nodes are already gone.
 *
 * <p>This is necessary but not sufficient. ReplayMod suppresses the same picture in two further
 * places that no flag reaches, and both are handled by dropping the mixin from its config in
 * `docker/Dockerfile` — see the comment there.
 *
 * <p>Only the flat POV projection wants a HUD: a spherical render draws six cube faces per frame,
 * so the HUD would appear six times, smeared around the panorama. {@link
 * ReplayModDriver#hudInFrame} carries that decision in from the driver; when it is false this leaves
 * ReplayMod's toggle alone and the frames come out as they did before.
 *
 * <p>The target class is named by string because 26.x ships unobfuscated and the driver compiles
 * without a Minecraft jar on the classpath.
 */
@Mixin(targets = "net.minecraft.client.gui.Hud")
public class HudUnhideMixin {
  @Shadow private boolean isHidden;

  @Inject(method = "extractRenderState", at = @At("HEAD"))
  private void rmdriver$forceVisible(CallbackInfo ci) {
    if (ReplayModDriver.hudInFrame) {
      this.isHidden = false;
    }
  }
}
