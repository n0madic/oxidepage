//! Generation-checked ids (slotmap-style): a 32-bit slot index plus a 32-bit
//! generation. A stale id — one whose slot has been freed and reused — fails
//! its generation check and becomes a clean error instead of aliasing another
//! entity.

use std::fmt;
use std::num::NonZeroU32;

/// The generation stored in a fresh slot. Non-zero so `Option<Id>` benefits
/// from niche layout (an id is exactly 8 bytes, optional or not).
pub const FIRST_GENERATION: NonZeroU32 = NonZeroU32::MIN;

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            index: u32,
            generation: NonZeroU32,
        }

        impl $name {
            /// Assembles an id from its raw parts. Only arena-style
            /// allocators should call this.
            #[must_use]
            pub fn from_parts(index: u32, generation: NonZeroU32) -> Self {
                Self { index, generation }
            }

            /// The slot index within the owning arena.
            #[must_use]
            pub fn index(self) -> u32 {
                self.index
            }

            /// The generation the slot had when this id was issued.
            #[must_use]
            pub fn generation(self) -> NonZeroU32 {
                self.generation
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}v{})", stringify!($name), self.index, self.generation)
            }
        }
    };
}

define_id! {
    /// Identifies a node in a [`DomTree`](https://docs.rs/oxidepage-dom) arena.
    NodeId
}

define_id! {
    /// Identifies a stylesheet in the document's stylesheet set.
    StyleSheetId
}

define_id! {
    /// Identifies an in-flight network request.
    RequestId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_id_is_niche_packed() {
        assert_eq!(size_of::<NodeId>(), 8);
        assert_eq!(size_of::<Option<NodeId>>(), 8);
    }

    #[test]
    fn ids_with_different_generations_differ() {
        let g1 = FIRST_GENERATION;
        let g2 = g1.checked_add(1).unwrap();
        assert_ne!(NodeId::from_parts(0, g1), NodeId::from_parts(0, g2));
        assert_eq!(NodeId::from_parts(0, g1), NodeId::from_parts(0, g1));
    }
}
