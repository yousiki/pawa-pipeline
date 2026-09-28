package moe.siki.pawa.rmdriver.mixin;

import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/**
 * Hold the item-swap animation at "fully raised" for the duration of one submit, so the held item is
 * actually in frame.
 *
 * <p>{@code submitHandsWithItems} offsets the arm downward by {@code 1 - mainHandHeight}, the equip
 * animation that slides a newly selected item up into view. {@code tick()} drives that height toward
 * {@code player.getItemSwapScale()}, and the camera a replay renders through is ReplayMod's {@code
 * CameraEntity}, which has no item-use state and reports zero forever. The result is an arm that is
 * submitted every frame — with the right item, past every gate — and translated just far enough off
 * the bottom of the frame to be invisible. Measured at {@code inverseArmHeight = 0.992} for the whole
 * of a render.
 *
 * <p>The height is raised at the submit and put back before returning, rather than pinned in {@code
 * tick()}, because vanilla uses ONE field for two jobs. {@code tick()} refreshes the item the hand
 * actually draws, {@code mainHandItem}, only while {@code mainHandHeight < 0.1} — i.e. only while the
 * arm is DOWN, which is the whole point of a swap animation. Pinning the height at 1 in {@code
 * tick()} therefore froze that refresh: the drawn item stayed whatever it had been when the height
 * last passed under the threshold, and the only items that ever got through afterwards were the ones
 * {@code shouldInstantlyReplaceVisibleItem} lets past without an animation. A render then showed a
 * stale block in hand for its whole length while the hotbar showed the sword the player was really
 * holding. Touching the height only around the submit leaves {@code tick()}'s bookkeeping vanilla, so
 * the drawn item tracks the spectated player exactly.
 *
 * <p>Losing the slide-in animation is what a dataset frame wants anyway: the item the player holds at
 * time t, not a lerp of the two items around it. ReplayMod reaches for the same effect in {@code
 * CameraEntity.onRenderHand}, but only once, when the spectated player changes.
 */
@Mixin(targets = "net.minecraft.client.renderer.ItemInHandRenderer")
public class HandEquipMixin {
  @Shadow private float mainHandHeight;
  @Shadow private float oMainHandHeight;
  @Shadow private float offHandHeight;
  @Shadow private float oOffHandHeight;

  private float rmdriver$mainHeight;
  private float rmdriver$oMainHeight;
  private float rmdriver$offHeight;
  private float rmdriver$oOffHeight;

  @Inject(method = "submitHandsWithItems", at = @At("HEAD"))
  private void rmdriver$raise(CallbackInfo ci) {
    this.rmdriver$mainHeight = this.mainHandHeight;
    this.rmdriver$oMainHeight = this.oMainHandHeight;
    this.rmdriver$offHeight = this.offHandHeight;
    this.rmdriver$oOffHeight = this.oOffHandHeight;
    this.mainHandHeight = 1.0F;
    this.oMainHandHeight = 1.0F;
    this.offHandHeight = 1.0F;
    this.oOffHandHeight = 1.0F;
  }

  @Inject(method = "submitHandsWithItems", at = @At("RETURN"))
  private void rmdriver$lower(CallbackInfo ci) {
    this.mainHandHeight = this.rmdriver$mainHeight;
    this.oMainHandHeight = this.rmdriver$oMainHeight;
    this.offHandHeight = this.rmdriver$offHeight;
    this.oOffHandHeight = this.rmdriver$oOffHeight;
  }
}
