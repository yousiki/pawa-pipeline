//! Clock-domain newtypes.
//!
//! The corpus carries seven distinct clocks, and two of them are easy to confuse when they share
//! the type `i64` and half a name ("gametime" vs "world_age"). Every time-valued
//! field in this crate therefore carries its clock domain in its type: mixing two domains is a
//! compile error, and converting between them is an explicit function naming the delta it applies.
//!
//! Rule: **no bare integer time fields anywhere in this crate.** A new time field either reuses one
//! of these newtypes or adds a new one with a doc comment saying what drives the clock.

use serde::{Deserialize, Serialize};

macro_rules! clock {
    ($(#[doc = $doc:literal])+ $name:ident) => {
        $(#[doc = $doc])+
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize, schemars::JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl ::core::ops::Sub for $name {
            type Output = i64;
            /// Difference between two instants on the same clock, as a bare count.
            fn sub(self, rhs: Self) -> i64 {
                self.0 - rhs.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

clock! {
    /// Server world age in ticks (`/time query gametime`). Monotonic for the life of a world and
    /// unaffected by `/time set`, which is why it is the authoritative alignment clock: capture
    /// anchors, replay deltas and action rows all resolve to this domain before being compared.
    WorldAgeTick
}

clock! {
    /// Server-side tick counter as observed by a connected client. Distinct from [`WorldAgeTick`]:
    /// it counts ticks the client saw, not the world's age.
    ServerTick
}

clock! {
    /// The bot client's own tick counter, starting when that client process starts.
    ClientTick
}

clock! {
    /// Frame position on the ReplayMod replay timeline, in ticks. Tick 0 is the start of the
    /// recording, not of the world — joining to [`WorldAgeTick`] requires the per-episode delta
    /// chosen by the metadata join.
    ReplayTick
}

clock! {
    /// Position on the ReplayMod replay timeline in milliseconds. The constant-frame-rate grid of a
    /// rendered video: frame `i` sits at `round(i * 1000 / fps)`.
    ReplayTimeMs
}

clock! {
    /// The bot process's monotonic clock, milliseconds since that bot started. Never used for
    /// alignment; exported into the dataset only as the solaris `physicsTime` column.
    PlayerTimeMs
}

clock! {
    /// Wall-clock Unix time in **milliseconds**. The only wall clock in the system; the one
    /// seconds-valued field anywhere is the solaris compatibility column `epochTime`, which is
    /// derived from an `EpochMs` at assembly and never read back.
    EpochMs
}

impl EpochMs {
    /// The current wall-clock time. The one place the system reads the clock — every crate that
    /// needs "now" goes through here instead of hand-rolling the `UNIX_EPOCH` dance.
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        )
    }
}

clock! {
    /// A span of milliseconds — a duration, not an instant. Subtracting two instants of any
    /// millisecond clock produces a plain `i64`; store it as `DurationMs` when it names a length.
    DurationMs
}
