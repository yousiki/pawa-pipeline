package moe.siki.pawa.rmdriver;

import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * Paint the capture kit into the spectated player's hotbar, every HUD frame.
 *
 * <p>The vanilla protocol never tells one client what another player's container holds: a remote
 * player's inventory is populated only by {@code SetEquipment} packets, which vanilla folds into
 * whatever {@code Inventory.selected} happens to be — slot 0 forever, because the serverbound
 * hotbar switches the recorded bot made are not in a clientbound recording. A replay spectated
 * through such a player therefore renders a hotbar with ONE filled slot whose item mutates in
 * place on every real switch, which is exactly the unlearnable footage the 2026-08 review flagged.
 *
 * <p>The capture side guarantees the fix is sound: the coordinator stocks every bot with one fixed
 * kit of nine DISTINCT items ({@code catalog::hotbar_kit}, carried in on the driver spec's
 * {@code hud_hotbar}), keeps it stable for the whole episode ({@code keepInventory}, idempotent
 * restock before {@code replay start}), and bots only ever change hands by switching slots. So the
 * true hotbar contents are a constant, and the true selected slot is a function of the mainhand
 * item — which IS synced, via the equipment packets ReplayMod applies to the entity. Painting the
 * constant and inverting that function reconstructs the player's real HUD state.
 *
 * <p>Counts are painted as the kit's full stacks: the recording carries no container updates to
 * decrement from, and the action stream (frames.jsonl) records no counts either, so a static count
 * is the representation the rest of the dataset already uses.
 *
 * <p>Reflection throughout, because the driver compiles without a Minecraft jar; resolution
 * happens once and any failure throws — a silently wrong hotbar is the defect this class exists to
 * remove, so drift must fail the render loudly rather than degrade it quietly.
 */
public final class HudKit {
  private HudKit() {}

  private static Object[] kitStacks; // ItemStack per hotbar slot
  private static Object[] kitItems; // Item per hotbar slot, for mainhand matching
  private static Method setItem; // Inventory.setItem(int, ItemStack)
  private static Method setSelected; // Inventory.setSelectedSlot(int), when it exists
  private static Field selectedField; // Inventory.selected*, otherwise

  /** Called from HudKitMixin at every {@code Hud.extractRenderState}, flat projections only. */
  public static void paint() {
    String[] items = ReplayModDriver.hudHotbarItems;
    int[] counts = ReplayModDriver.hudHotbarCounts;
    if (items == null || counts == null) {
      return; // driver spec not loaded yet; nothing is being rendered either
    }
    try {
      Object mc = Refl.staticInvoke("net.minecraft.client.Minecraft", "getInstance");
      Object cam = Refl.invokeFirstNoArg(mc, "getCameraEntity");
      Object localPlayer = Refl.fieldOrNull(mc, "player");
      // Painting applies only to a spectated OTHER player. The free camera (mc.player itself) has
      // no real inventory to reconstruct, and a null camera means no world yet.
      if (cam == null || cam == localPlayer) {
        return;
      }
      Object inventory = Refl.invokeFirstNoArg(cam, "getInventory");
      if (inventory == null) {
        return; // camera attached to a non-player entity
      }
      ensureResolved(items, counts, inventory);

      // Selected slot = the kit slot holding the (equipment-synced) mainhand item. Item instances
      // are registry singletons, so reference equality is the exact comparison. An empty or
      // unknown hand leaves the previous selection alone.
      Object mainhand = Refl.invokeFirstNoArg(cam, "getMainHandItem");
      Object mainItem = mainhand == null ? null : Refl.invokeFirstNoArg(mainhand, "getItem");
      for (int i = 0; i < kitItems.length; i++) {
        if (kitItems[i] == mainItem) {
          if (setSelected != null) {
            setSelected.invoke(inventory, i);
          } else {
            selectedField.set(inventory, i);
          }
          break;
        }
      }
      for (int i = 0; i < kitStacks.length; i++) {
        setItem.invoke(inventory, i, kitStacks[i]);
      }
    } catch (Throwable t) {
      throw new IllegalStateException(
          "rmdriver: cannot paint the capture kit into the spectated hotbar", t);
    }
  }

  /** One-time resolution of the vanilla handles and construction of the kit stacks. */
  private static void ensureResolved(String[] items, int[] counts, Object inventory)
      throws Exception {
    if (kitStacks != null) {
      return;
    }
    Object[] resolvedItems = new Object[items.length];
    Object[] resolvedStacks = new Object[items.length];
    for (int i = 0; i < items.length; i++) {
      // Items exposes every vanilla item as a constant named like its registry id — a lookup that
      // needs no registry, no ResourceLocation and no Optional-vs-Holder drift handling.
      Object item =
          Refl.staticFieldOrNull(
              "net.minecraft.world.item.Items", items[i].toUpperCase(java.util.Locale.ROOT));
      if (item == null) {
        throw new IllegalStateException("no vanilla item constant for kit entry " + items[i]);
      }
      Object stack = Refl.invokeFirstNoArg(item, "getDefaultInstance");
      Refl.method(stack.getClass(), "setCount", 1).invoke(stack, counts[i]);
      resolvedItems[i] = item;
      resolvedStacks[i] = stack;
    }
    setItem = Refl.method(inventory.getClass(), "setItem", 2);
    setSelected = methodOrNull(inventory.getClass(), "setSelectedSlot");
    if (setSelected == null) {
      selectedField = findField(inventory.getClass(), "selected", "selectedSlot");
      if (selectedField == null) {
        throw new IllegalStateException(
            "Inventory has neither setSelectedSlot(int) nor a selected/selectedSlot field");
      }
    }
    kitItems = resolvedItems;
    kitStacks = resolvedStacks;
  }

  private static Method methodOrNull(Class<?> type, String name) {
    try {
      return Refl.method(type, name, 1);
    } catch (NoSuchMethodException absent) {
      return null;
    }
  }

  private static Field findField(Class<?> type, String... names) {
    for (String name : names) {
      for (Class<?> c = type; c != null; c = c.getSuperclass()) {
        try {
          Field f = c.getDeclaredField(name);
          f.setAccessible(true);
          return f;
        } catch (NoSuchFieldException ignored) {
          // not on this class; try its superclass
        }
      }
    }
    return null;
  }
}
