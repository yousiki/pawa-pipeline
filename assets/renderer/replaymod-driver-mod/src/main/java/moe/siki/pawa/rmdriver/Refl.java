package moe.siki.pawa.rmdriver;

import java.lang.reflect.Array;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Collection;
import java.util.Map;

/**
 * Runtime reflection into ReplayMod, ReplayStudio and Minecraft.
 *
 * <p>{@link ReplayModDriver} compiles against none of them, so every call into them goes through
 * here. Lookups take a NAME and an ARITY rather than a signature, and search the whole type
 * hierarchy plus the public interface methods, so they survive the remapping and version drift the
 * driver exists to tolerate.
 *
 * <p>Each lookup comes in two flavours over ONE hierarchy walk: an {@code …OrNull} form for probes
 * that have a fallback, and a throwing form for the calls whose absence means the driver cannot do
 * its job. The distinction is deliberate — a field that exists and holds null is not the same thing
 * as a field that has been renamed, and only the throwing form can tell you which happened.
 */
final class Refl {
  private Refl() {}

  // ---- fields ---------------------------------------------------------------

  /** The first of {@code names} declared anywhere in {@code type}'s hierarchy, or null. */
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

  /** Value of the first of {@code names} that exists, or null when none of them do. */
  static Object fieldOrNull(Object owner, String... names) throws IllegalAccessException {
    if (owner == null) return null;
    Field f = findField(owner.getClass(), names);
    return f == null ? null : f.get(owner);
  }

  /** Value of {@code name}; throws when no such field exists (as opposed to holding null). */
  static Object field(Object owner, String name) throws Exception {
    Field f = findField(owner.getClass(), name);
    if (f == null) throw new NoSuchFieldException(name + " on " + owner.getClass());
    return f.get(owner);
  }

  static Object staticFieldOrNull(String className, String fieldName) {
    try {
      return Class.forName(className).getField(fieldName).get(null);
    } catch (Throwable t) {
      return null;
    }
  }

  // ---- methods --------------------------------------------------------------

  /**
   * The first method named one of {@code names} taking {@code arity} parameters, or null. Walks the
   * declared methods up the hierarchy, then falls back to {@code getMethods()} so public methods
   * inherited from an interface — which are declared on no superclass — are still found.
   */
  private static Method findMethod(Class<?> type, int arity, String... names) {
    for (String name : names) {
      for (Class<?> c = type; c != null; c = c.getSuperclass()) {
        for (Method m : c.getDeclaredMethods()) {
          if (m.getName().equals(name) && m.getParameterCount() == arity) {
            m.setAccessible(true);
            return m;
          }
        }
      }
      for (Method m : type.getMethods()) {
        if (m.getName().equals(name) && m.getParameterCount() == arity) {
          m.setAccessible(true);
          return m;
        }
      }
    }
    return null;
  }

  /** Find a method by name + parameter count; throws when it is absent. */
  static Method method(Class<?> type, String name, int arity) throws NoSuchMethodException {
    Method m = findMethod(type, arity, name);
    if (m == null) throw new NoSuchMethodException(name + "/" + arity + " on " + type.getName());
    return m;
  }

  /** Invoke the first no-arg method named one of {@code names}; null when none of them exists. */
  static Object invokeFirstNoArg(Object owner, String... names) throws Exception {
    if (owner == null) return null;
    Method m = findMethod(owner.getClass(), 0, names);
    return m == null ? null : m.invoke(owner);
  }

  static Object invoke(Object target, Method m, Object... args) throws Exception {
    m.setAccessible(true);
    return m.invoke(target, args);
  }

  static Object staticInvoke(String className, String methodName) throws Exception {
    return method(Class.forName(className), methodName, 0).invoke(null);
  }

  // ---- constructors ---------------------------------------------------------

  static Constructor<?> ctorWithArity(Class<?> cls, int arity) throws NoSuchMethodException {
    for (Constructor<?> ctor : cls.getDeclaredConstructors()) {
      if (ctor.getParameterCount() == arity) {
        ctor.setAccessible(true);
        return ctor;
      }
    }
    throw new NoSuchMethodException("no " + arity + "-arg constructor on " + cls.getName());
  }

  // ---- enums and value coercion --------------------------------------------

  @SuppressWarnings({"unchecked", "rawtypes"})
  static Object enumValue(Class<?> enumCls, String constName) {
    return Enum.valueOf((Class) enumCls, constName);
  }

  static Object enumValueOrNull(Class<?> enumCls, String constName) {
    try {
      return enumValue(enumCls, constName);
    } catch (Throwable t) {
      return null;
    }
  }

  static int numberValue(Object value) {
    return value instanceof Number number ? Math.max(0, number.intValue()) : 0;
  }

  static int collectionSize(Object value) {
    if (value instanceof Collection<?> collection) return collection.size();
    if (value instanceof Map<?, ?> map) return map.size();
    if (value != null && value.getClass().isArray()) return Array.getLength(value);
    return 0;
  }
}
