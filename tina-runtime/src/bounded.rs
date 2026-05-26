//! Service-owned bounds for request-sized work.
//!
//! These helpers are the boring Tina path for "this request names many things,
//! but the service owns the maximum." They do not ban loops or vectors. They
//! make the copied path require an explicit cap before many items become many
//! runtime effects.

use std::error::Error;
use std::fmt;

use tina::{Effect, Isolate};

/// Error while collecting a service-bounded item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedItemsError {
    /// The configured cap was zero.
    ZeroMaxItems,
    /// More items were observed than the configured cap.
    TooManyItems {
        /// Service-owned maximum item count.
        max: usize,
        /// Count observed before construction stopped.
        observed: usize,
    },
}

impl fmt::Display for BoundedItemsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaxItems => f.write_str("max_items must be positive"),
            Self::TooManyItems { max, observed } => {
                write!(f, "observed {observed} items, exceeding max_items {max}")
            }
        }
    }
}

impl Error for BoundedItemsError {}

/// A list that has passed through a service-owned item cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedItems<T> {
    max_items: usize,
    items: Vec<T>,
}

impl<T> BoundedItems<T> {
    /// Collect items up to `max_items`.
    ///
    /// Stops at the first over-cap item. It does not keep walking an
    /// untrusted iterator just to count every extra item.
    pub fn try_from_iter(
        max_items: usize,
        iter: impl IntoIterator<Item = T>,
    ) -> Result<Self, BoundedItemsError> {
        if max_items == 0 {
            return Err(BoundedItemsError::ZeroMaxItems);
        }
        let mut items = Vec::with_capacity(max_items);
        for item in iter {
            if items.len() == max_items {
                return Err(BoundedItemsError::TooManyItems {
                    max: max_items,
                    observed: max_items + 1,
                });
            }
            items.push(item);
        }
        Ok(Self { max_items, items })
    }

    /// Service-owned maximum item count.
    pub fn max_items(&self) -> usize {
        self.max_items
    }

    /// Admitted item count.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True if no items were admitted.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Iterate over admitted items.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.iter()
    }

    /// Borrow admitted items as a slice.
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// Consume into the admitted items.
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    /// Map admitted items into runtime effects after the item list has already
    /// passed the service-owned cap.
    ///
    /// Prefer this over building effects from a raw request iterator: the
    /// over-cap item is rejected before any corresponding effect can exist.
    pub fn map_effects<I, F>(self, mut f: F) -> BoundedEffects<I>
    where
        I: Isolate,
        F: FnMut(T) -> Effect<I>,
    {
        BoundedEffects {
            max_effects: self.max_items,
            effects: self.items.into_iter().map(&mut f).collect(),
        }
    }
}

/// Error while collecting a service-bounded effect list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedEffectsError {
    /// The configured cap was zero.
    ZeroMaxEffects,
    /// More effects were observed than the configured cap.
    TooManyEffects {
        /// Service-owned maximum effect count.
        max: usize,
        /// Count observed before construction stopped.
        observed: usize,
    },
}

impl fmt::Display for BoundedEffectsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaxEffects => f.write_str("max_effects must be positive"),
            Self::TooManyEffects { max, observed } => {
                write!(
                    f,
                    "observed {observed} effects, exceeding max_effects {max}"
                )
            }
        }
    }
}

impl Error for BoundedEffectsError {}

/// Runtime effects that have passed through a service-owned cap.
pub struct BoundedEffects<I>
where
    I: Isolate,
{
    max_effects: usize,
    effects: Vec<Effect<I>>,
}

impl<I> BoundedEffects<I>
where
    I: Isolate,
{
    /// Collect effects up to `max_effects`.
    ///
    /// Use this when the effect iterator is already service-bounded. If the
    /// iterator comes from request data, prefer `BoundedItems::map_effects` so
    /// over-cap request items are rejected before their effects are built.
    pub fn try_from_iter(
        max_effects: usize,
        iter: impl IntoIterator<Item = Effect<I>>,
    ) -> Result<Self, BoundedEffectsError> {
        if max_effects == 0 {
            return Err(BoundedEffectsError::ZeroMaxEffects);
        }
        let mut effects = Vec::with_capacity(max_effects);
        for effect in iter {
            if effects.len() == max_effects {
                return Err(BoundedEffectsError::TooManyEffects {
                    max: max_effects,
                    observed: max_effects + 1,
                });
            }
            effects.push(effect);
        }
        Ok(Self {
            max_effects,
            effects,
        })
    }

    /// Service-owned maximum effect count.
    pub fn max_effects(&self) -> usize {
        self.max_effects
    }

    /// Admitted effect count.
    pub fn len(&self) -> usize {
        self.effects.len()
    }

    /// True if no effects were admitted.
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Consume into the admitted effects.
    pub fn into_vec(self) -> Vec<Effect<I>> {
        self.effects
    }

    /// Consume into a normal Tina batch effect.
    pub fn into_batch(self) -> Effect<I> {
        tina::batch(self.effects)
    }
}

/// Turn a bounded effect list into a Tina batch.
pub fn bounded_batch<I>(effects: BoundedEffects<I>) -> Effect<I>
where
    I: Isolate,
{
    effects.into_batch()
}

/// Assertion failure for one service-owned bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOwnedBoundError {
    /// The service did not declare a finite cap.
    UnboundedDeclared {
        /// Bound name.
        name: String,
    },
    /// The service declared a zero cap.
    ZeroConfigured {
        /// Bound name.
        name: String,
    },
    /// No observation was supplied for this bound.
    MissingObservation {
        /// Bound name.
        name: String,
        /// Configured cap.
        configured: usize,
    },
    /// Observed work exceeded the configured cap.
    Exceeded {
        /// Bound name.
        name: String,
        /// Configured cap.
        configured: usize,
        /// Observed work.
        observed: usize,
    },
}

impl fmt::Display for ServiceOwnedBoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundedDeclared { name } => {
                write!(f, "service bound {name:?} is unbounded")
            }
            Self::ZeroConfigured { name } => {
                write!(f, "service bound {name:?} is zero")
            }
            Self::MissingObservation { name, configured } => write!(
                f,
                "service bound {name:?} configured max {configured} but no observation was supplied",
            ),
            Self::Exceeded {
                name,
                configured,
                observed,
            } => write!(
                f,
                "service bound {name:?} observed {observed} exceeds configured max {configured}",
            ),
        }
    }
}

impl Error for ServiceOwnedBoundError {}

/// Assert that observed work stayed under a service-owned finite cap.
///
/// `configured = None` means the code under test declared the path unbounded.
/// `observed = None` means the test did not collect the fact it needs.
pub fn assert_service_owned_bound(
    name: impl Into<String>,
    configured: Option<usize>,
    observed: Option<usize>,
) -> Result<(), ServiceOwnedBoundError> {
    let name = name.into();
    let Some(configured) = configured else {
        return Err(ServiceOwnedBoundError::UnboundedDeclared { name });
    };
    if configured == 0 {
        return Err(ServiceOwnedBoundError::ZeroConfigured { name });
    }
    let Some(observed) = observed else {
        return Err(ServiceOwnedBoundError::MissingObservation { name, configured });
    };
    if observed > configured {
        return Err(ServiceOwnedBoundError::Exceeded {
            name,
            configured,
            observed,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tina::{Context, Outbound, noop, send};

    #[derive(Debug, Clone, Copy)]
    enum Msg {
        Item(#[allow(dead_code)] u8),
    }

    struct Owner;

    impl Isolate for Owner {
        tina::isolate_types! {
            message: Msg,
            reply: (),
            send: Outbound<Msg>,
            spawn: Infallible,
            call: Infallible,
            shard: tina::SingleShard,
        }

        fn handle(&mut self, _msg: Msg, _ctx: &mut Context<'_, tina::SingleShard>) -> Effect<Self> {
            noop()
        }
    }

    #[test]
    fn bounded_items_reject_zero_and_over_cap_preserve_order() {
        assert_eq!(
            BoundedItems::<u8>::try_from_iter(0, [1]),
            Err(BoundedItemsError::ZeroMaxItems)
        );
        assert_eq!(
            BoundedItems::try_from_iter(2, [1, 2, 3]),
            Err(BoundedItemsError::TooManyItems {
                max: 2,
                observed: 3,
            })
        );
        let items = BoundedItems::try_from_iter(3, [1, 2, 3]).unwrap();
        assert_eq!(items.max_items(), 3);
        assert_eq!(items.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn bounded_effects_reject_zero_and_over_cap() {
        let addr = tina::Address::<Msg>::new(tina::ShardId::new(0), tina::IsolateId::new(1));
        assert!(matches!(
            BoundedEffects::<Owner>::try_from_iter(0, [send(addr, Msg::Item(1))]),
            Err(BoundedEffectsError::ZeroMaxEffects)
        ));
        assert!(matches!(
            BoundedEffects::<Owner>::try_from_iter(
                1,
                [send(addr, Msg::Item(1)), send(addr, Msg::Item(2))],
            ),
            Err(BoundedEffectsError::TooManyEffects {
                max: 1,
                observed: 2,
            })
        ));
    }

    #[test]
    fn bounded_items_map_to_effects_after_admission() {
        let addr = tina::Address::<Msg>::new(tina::ShardId::new(0), tina::IsolateId::new(1));
        let items = BoundedItems::try_from_iter(4, [1_u8, 2, 3]).unwrap();
        let effects = items.map_effects::<Owner, _>(|item| send(addr, Msg::Item(item)));
        assert_eq!(effects.max_effects(), 4);
        assert_eq!(effects.len(), 3);
    }

    #[test]
    fn service_owned_bound_assertion_names_bad_paths() {
        assert_eq!(
            assert_service_owned_bound("svc.fanout", None, Some(1)),
            Err(ServiceOwnedBoundError::UnboundedDeclared {
                name: "svc.fanout".to_string(),
            })
        );
        assert_eq!(
            assert_service_owned_bound("svc.fanout", Some(0), Some(0)),
            Err(ServiceOwnedBoundError::ZeroConfigured {
                name: "svc.fanout".to_string(),
            })
        );
        assert_eq!(
            assert_service_owned_bound("svc.fanout", Some(2), None),
            Err(ServiceOwnedBoundError::MissingObservation {
                name: "svc.fanout".to_string(),
                configured: 2,
            })
        );
        assert_eq!(
            assert_service_owned_bound("svc.fanout", Some(2), Some(3)),
            Err(ServiceOwnedBoundError::Exceeded {
                name: "svc.fanout".to_string(),
                configured: 2,
                observed: 3,
            })
        );
        assert!(assert_service_owned_bound("svc.fanout", Some(2), Some(2)).is_ok());
    }
}
