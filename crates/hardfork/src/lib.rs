//! Zone-owned protocol hardfork definitions.
//!
//! This crate intentionally does not depend on `zone-chainspec` or Reth, so execution and SDK
//! crates can use [`ZoneHardfork`] without pulling in node integration.

#![no_std]

use alloy_hardforks::hardfork;

/// Defines the Zone hardfork enum and APIs from one ordered variant list.
macro_rules! zone_hardfork {
    (
        $(#[$enum_meta:meta])*
        ZoneHardfork {
            $(#[$z0_meta:meta])* Z0,
            $( $(#[$meta:meta])* $variant:ident ),* $(,)?
        }
    ) => {
        hardfork!(
            $(#[$enum_meta])*
            ZoneHardfork {
                $(#[$z0_meta])* Z0,
                $( $(#[$meta])* $variant ),*
            }
        );

        impl Default for ZoneHardfork {
            fn default() -> Self {
                Self::Z0
            }
        }

        impl ZoneHardfork {
            /// Returns whether Z0 behavior is available.
            pub const fn is_z0(&self) -> bool {
                *self as u8 >= Self::Z0 as u8
            }

            paste::paste! {
                $(
                    #[doc = concat!("Returns whether ", stringify!($variant), " behavior is available.")]
                    pub const fn [<is_ $variant:lower>](&self) -> bool {
                        *self as u8 >= Self::$variant as u8
                    }
                )*
            }
        }

        /// Invokes a callback macro with every post-Z0 Zone hardfork variant.
        ///
        /// Downstream crates can use this to generate exhaustive hardfork-dependent APIs without
        /// depending on Zone chainspec or node integration.
        #[macro_export]
        macro_rules! zone_post_z0_hardforks {
            ($callback:ident) => {
                $callback! { $($variant),* }
            };
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use ZoneHardfork::*;
            use alloy_hardforks::Hardfork;

            #[test]
            fn test_hardfork_name() {
                assert_eq!(Z0.name(), "Z0");
                $(assert_eq!($variant.name(), stringify!($variant));)*
            }

            #[test]
            fn test_hardfork_trait_implementation() {
                for fork in ZoneHardfork::VARIANTS {
                    let _name: &str = Hardfork::name(fork);
                }
            }

            #[test]
            fn test_is_z0() {
                for fork in ZoneHardfork::VARIANTS {
                    assert!(fork.is_z0(), "{fork:?} should satisfy is_z0");
                }
            }

            paste::paste! {
                $(
                    #[test]
                    fn [<test_is_ $variant:lower>]() {
                        let idx = ZoneHardfork::VARIANTS.iter().position(|fork| *fork == $variant)
                            .expect(concat!(stringify!($variant), " missing from VARIANTS"));
                        for (i, fork) in ZoneHardfork::VARIANTS.iter().enumerate() {
                            let active = ZoneHardfork::[<is_ $variant:lower>](fork);
                            if i >= idx {
                                assert!(active, "{fork:?} should satisfy is_{}", stringify!([<$variant:lower>]));
                            } else {
                                assert!(!active, "{fork:?} should not satisfy is_{}", stringify!([<$variant:lower>]));
                            }
                        }
                    }
                )*
            }
        }
    };
}

zone_hardfork!(
    /// Zone protocol revisions, ordered by activation.
    ZoneHardfork {
        /// The original Zone state transition function.
        Z0,
        /// The first independently scheduled Zone transition.
        Z1,
    }
);
