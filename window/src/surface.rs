//! State of a presentation surface that a platform attaches to and detaches
//! from a logical window after the window exists (Android `SurfaceView`).
//!
//! The state machine is pure: it takes the current state and one surface
//! event and returns the next state plus the effects the backend must carry
//! out, in order.  The native lease type is a parameter so the transitions
//! are host-testable without an Android runtime.

#![forbid(unsafe_code)]

use std::convert::TryFrom;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::mpsc::Sender;

/// Identity of one native surface instance.  The platform side assigns
/// strictly increasing values per process; every callback names the
/// generation it belongs to and callbacks for any other generation are
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceGeneration(NonZeroU64);

impl SurfaceGeneration {
    /// Parse a platform-supplied generation; zero is not a generation.
    pub fn new(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Drawable size of a surface in pixels.  Both sides are nonzero, so a
/// GPU surface configured from it is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceGeometry {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl SurfaceGeometry {
    /// Validate platform dimensions; zero or negative sides mean "not
    /// drawable yet".
    pub fn new(width: i32, height: i32) -> Option<Self> {
        Some(Self {
            width: NonZeroU32::new(u32::try_from(width).ok()?)?,
            height: NonZeroU32::new(u32::try_from(height).ok()?)?,
        })
    }

    pub fn width(self) -> u32 {
        self.width.get()
    }

    pub fn height(self) -> u32 {
        self.height.get()
    }
}

/// Completion signal for a retirement.  The platform thread that destroyed
/// the surface blocks on the receiving side until the native surface
/// reference has actually been released.
#[derive(Debug)]
pub struct RetireAck(Sender<()>);

impl RetireAck {
    pub fn new(sender: Sender<()>) -> Self {
        Self(sender)
    }

    /// Signal completion.  A dropped receiver means the waiter gave up;
    /// there is nothing further to do for it.
    pub fn send(self) {
        self.0.send(()).ok();
    }
}

/// A platform callback about the surface.
#[derive(Debug)]
pub enum SurfaceEvent<L> {
    /// A new surface instance exists.  `geometry` is `None` until the
    /// platform reports a nonzero size.
    Created {
        generation: SurfaceGeneration,
        lease: L,
        geometry: Option<SurfaceGeometry>,
    },
    /// The surface's size changed; `None` means it became zero-sized.
    Changed {
        generation: SurfaceGeneration,
        geometry: Option<SurfaceGeometry>,
    },
    /// The surface is going away.  The platform waits for `ack`.
    Destroyed {
        generation: SurfaceGeneration,
        ack: RetireAck,
    },
}

/// Whether a logical window can currently present.
#[derive(Debug)]
pub enum SurfaceState<L> {
    /// No native surface.  `retired` is the newest generation this slot
    /// has held or seen destroyed; a creation for it or an older one is
    /// stale, so a destroyed surface can never be resurrected.
    Absent { retired: Option<SurfaceGeneration> },
    /// A native surface exists but has no drawable size.
    Unsized {
        generation: SurfaceGeneration,
        lease: L,
    },
    /// A native surface with a valid size; GPU state may target it.
    Present {
        generation: SurfaceGeneration,
        lease: L,
        geometry: SurfaceGeometry,
    },
}

/// What the backend must do after a transition, in order.
#[derive(Debug)]
pub enum SurfaceEffect<L> {
    /// The window entered `Present`: create GPU state for `geometry`.
    Available(SurfaceGeometry),
    /// The window stayed `Present` with a new size.
    Resized(SurfaceGeometry),
    /// The window left `Present`: every GPU reference to the surface must
    /// be dropped before any lease is released.
    Lost,
    /// A lease is no longer current.  Release it; when `ack` is present the
    /// platform is waiting for the release.
    Retire { lease: L, ack: Option<RetireAck> },
    /// A destroy callback for a generation that holds nothing.
    Ack(RetireAck),
    /// The event belonged to a generation that is not current.
    Stale(SurfaceGeneration),
}

impl<L> Default for SurfaceState<L> {
    fn default() -> Self {
        Self::Absent { retired: None }
    }
}

impl<L> SurfaceState<L> {
    /// Generation of the surface currently held.
    pub fn generation(&self) -> Option<SurfaceGeneration> {
        match self {
            Self::Absent { .. } => None,
            Self::Unsized { generation, .. } | Self::Present { generation, .. } => {
                Some(*generation)
            }
        }
    }

    /// Newest generation the slot has ever held or seen destroyed.
    pub fn high_water(&self) -> Option<SurfaceGeneration> {
        match self {
            Self::Absent { retired } => *retired,
            Self::Unsized { generation, .. } | Self::Present { generation, .. } => {
                Some(*generation)
            }
        }
    }

    pub fn geometry(&self) -> Option<SurfaceGeometry> {
        match self {
            Self::Present { geometry, .. } => Some(*geometry),
            Self::Absent { .. } | Self::Unsized { .. } => None,
        }
    }

    pub fn lease(&self) -> Option<&L> {
        match self {
            Self::Absent { .. } => None,
            Self::Unsized { lease, .. } | Self::Present { lease, .. } => Some(lease),
        }
    }

    pub fn into_lease(self) -> Option<L> {
        match self {
            Self::Absent { .. } => None,
            Self::Unsized { lease, .. } | Self::Present { lease, .. } => Some(lease),
        }
    }

    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// Apply one platform event.
    pub fn apply(self, event: SurfaceEvent<L>) -> (Self, Vec<SurfaceEffect<L>>) {
        match event {
            SurfaceEvent::Created {
                generation,
                lease,
                geometry,
            } => {
                let mut effects = Vec::new();
                if self
                    .high_water()
                    .map_or(false, |newest| generation <= newest)
                {
                    effects.push(SurfaceEffect::Stale(generation));
                    drop(lease);
                    return (self, effects);
                }
                self.retire_into(None, &mut effects);
                (
                    Self::with_geometry(generation, lease, geometry, &mut effects),
                    effects,
                )
            }
            SurfaceEvent::Changed {
                generation,
                geometry,
            } => {
                let mut effects = Vec::new();
                if self.generation() != Some(generation) {
                    effects.push(SurfaceEffect::Stale(generation));
                    return (self, effects);
                }
                let next = match (self, geometry) {
                    (
                        Self::Present {
                            lease,
                            geometry: old,
                            ..
                        },
                        Some(new),
                    ) => {
                        if old != new {
                            effects.push(SurfaceEffect::Resized(new));
                        }
                        Self::Present {
                            generation,
                            lease,
                            geometry: new,
                        }
                    }
                    (Self::Present { lease, .. }, None) => {
                        effects.push(SurfaceEffect::Lost);
                        Self::Unsized { generation, lease }
                    }
                    (Self::Unsized { lease, .. }, geometry) => {
                        Self::with_geometry(generation, lease, geometry, &mut effects)
                    }
                    (Self::Absent { .. }, _) => unreachable!("generation matched Absent"),
                };
                (next, effects)
            }
            SurfaceEvent::Destroyed { generation, ack } => {
                let mut effects = Vec::new();
                match self {
                    // A generation the slot never held is retired on the
                    // spot, so its creation can no longer arrive late.
                    Self::Absent { retired } if retired.map_or(true, |r| generation > r) => {
                        effects.push(SurfaceEffect::Ack(ack));
                        (
                            Self::Absent {
                                retired: Some(generation),
                            },
                            effects,
                        )
                    }
                    held if held.generation() == Some(generation) => {
                        held.retire_into(Some(ack), &mut effects);
                        (
                            Self::Absent {
                                retired: Some(generation),
                            },
                            effects,
                        )
                    }
                    other => {
                        effects.push(SurfaceEffect::Stale(generation));
                        effects.push(SurfaceEffect::Ack(ack));
                        (other, effects)
                    }
                }
            }
        }
    }

    fn with_geometry(
        generation: SurfaceGeneration,
        lease: L,
        geometry: Option<SurfaceGeometry>,
        effects: &mut Vec<SurfaceEffect<L>>,
    ) -> Self {
        match geometry {
            Some(geometry) => {
                effects.push(SurfaceEffect::Available(geometry));
                Self::Present {
                    generation,
                    lease,
                    geometry,
                }
            }
            None => Self::Unsized { generation, lease },
        }
    }

    fn retire_into(self, ack: Option<RetireAck>, effects: &mut Vec<SurfaceEffect<L>>) {
        match self {
            Self::Absent { .. } => {
                if let Some(ack) = ack {
                    effects.push(SurfaceEffect::Ack(ack));
                }
            }
            Self::Unsized { lease, .. } => effects.push(SurfaceEffect::Retire { lease, ack }),
            Self::Present { lease, .. } => {
                effects.push(SurfaceEffect::Lost);
                effects.push(SurfaceEffect::Retire { lease, ack });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn gen(n: u64) -> SurfaceGeneration {
        SurfaceGeneration::new(n).unwrap()
    }

    fn geom(w: i32, h: i32) -> SurfaceGeometry {
        SurfaceGeometry::new(w, h).unwrap()
    }

    fn ack() -> (RetireAck, Receiver<()>) {
        let (tx, rx) = channel();
        (RetireAck::new(tx), rx)
    }

    fn kinds(effects: &[SurfaceEffect<&'static str>]) -> Vec<String> {
        effects
            .iter()
            .map(|e| match e {
                SurfaceEffect::Available(g) => format!("available:{}x{}", g.width(), g.height()),
                SurfaceEffect::Resized(g) => format!("resized:{}x{}", g.width(), g.height()),
                SurfaceEffect::Lost => "lost".into(),
                SurfaceEffect::Retire { lease, ack } => {
                    format!(
                        "retire:{lease}:{}",
                        if ack.is_some() { "ack" } else { "silent" }
                    )
                }
                SurfaceEffect::Ack(_) => "ack".into(),
                SurfaceEffect::Stale(g) => format!("stale:{}", g.get()),
            })
            .collect()
    }

    #[test]
    fn geometry_rejects_zero_and_negative_sides() {
        assert!(SurfaceGeometry::new(0, 10).is_none());
        assert!(SurfaceGeometry::new(10, 0).is_none());
        assert!(SurfaceGeometry::new(-1, 10).is_none());
        assert_eq!(geom(1080, 2424).width(), 1080);
        assert!(SurfaceGeneration::new(0).is_none());
    }

    #[test]
    fn creation_without_size_defers_availability() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "lease1",
            geometry: None,
        });
        assert!(matches!(state, SurfaceState::Unsized { .. }));
        assert!(effects.is_empty(), "{:?}", kinds(&effects));

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(100, 200)),
        });
        assert_eq!(kinds(&effects), ["available:100x200"]);
        assert_eq!(state.geometry(), Some(geom(100, 200)));
    }

    #[test]
    fn size_changes_resize_and_zero_size_loses_the_surface() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "lease1",
            geometry: Some(geom(100, 200)),
        });
        assert_eq!(kinds(&effects), ["available:100x200"]);

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(100, 200)),
        });
        assert!(effects.is_empty(), "same size is not a resize");

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(200, 100)),
        });
        assert_eq!(kinds(&effects), ["resized:200x100"]);

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: None,
        });
        assert_eq!(kinds(&effects), ["lost"]);
        assert!(matches!(state, SurfaceState::Unsized { .. }));
        assert!(!state.is_present());

        let (_, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(50, 50)),
        });
        assert_eq!(kinds(&effects), ["available:50x50"]);
    }

    #[test]
    fn stale_generations_never_touch_the_live_binding() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (state, _) = state.apply(SurfaceEvent::Created {
            generation: gen(2),
            lease: "lease2",
            geometry: Some(geom(100, 200)),
        });

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(1, 1)),
        });
        assert_eq!(kinds(&effects), ["stale:1"]);
        assert_eq!(state.geometry(), Some(geom(100, 200)));

        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "old-lease",
            geometry: Some(geom(1, 1)),
        });
        assert_eq!(kinds(&effects), ["stale:1"]);
        assert_eq!(state.lease(), Some(&"lease2"));

        let (retire_ack, rx) = ack();
        let (state, effects) = state.apply(SurfaceEvent::Destroyed {
            generation: gen(1),
            ack: retire_ack,
        });
        assert_eq!(kinds(&effects), ["stale:1", "ack"]);
        assert_eq!(state.generation(), Some(gen(2)));
        for effect in effects {
            if let SurfaceEffect::Ack(ack) = effect {
                ack.send();
            }
        }
        assert!(
            rx.try_recv().is_ok(),
            "stale destroy is acknowledged at once"
        );
    }

    #[test]
    fn destroy_loses_before_retiring_and_newer_creation_retires_silently() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (state, _) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "lease1",
            geometry: Some(geom(100, 200)),
        });
        let (retire_ack, _rx) = ack();
        let (state, effects) = state.apply(SurfaceEvent::Destroyed {
            generation: gen(1),
            ack: retire_ack,
        });
        assert_eq!(kinds(&effects), ["lost", "retire:lease1:ack"]);
        assert!(matches!(state, SurfaceState::Absent { .. }));

        let (state, _) = state.apply(SurfaceEvent::Created {
            generation: gen(2),
            lease: "lease2",
            geometry: Some(geom(100, 200)),
        });
        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(3),
            lease: "lease3",
            geometry: None,
        });
        assert_eq!(kinds(&effects), ["lost", "retire:lease2:silent"]);
        assert_eq!(state.generation(), Some(gen(3)));
        assert!(!state.is_present());
    }

    #[test]
    fn destroyed_generation_is_never_created_afterwards() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (state, _) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "lease1",
            geometry: Some(geom(100, 200)),
        });
        let (retire_ack, _rx) = ack();
        let (state, effects) = state.apply(SurfaceEvent::Destroyed {
            generation: gen(1),
            ack: retire_ack,
        });
        assert_eq!(kinds(&effects), ["lost", "retire:lease1:ack"]);
        assert_eq!(state.high_water(), Some(gen(1)));

        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "lease1-again",
            geometry: Some(geom(100, 200)),
        });
        assert_eq!(kinds(&effects), ["stale:1"]);
        assert!(matches!(state, SurfaceState::Absent { .. }));
        assert_eq!(state.lease(), None);

        let (state, effects) = state.apply(SurfaceEvent::Changed {
            generation: gen(1),
            geometry: Some(geom(1, 1)),
        });
        assert_eq!(kinds(&effects), ["stale:1"]);

        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(2),
            lease: "lease2",
            geometry: Some(geom(100, 200)),
        });
        assert_eq!(kinds(&effects), ["available:100x200"]);
        assert_eq!(state.lease(), Some(&"lease2"));
    }

    #[test]
    fn destroy_of_a_never_held_generation_retires_it_before_its_creation() {
        let state: SurfaceState<&str> = SurfaceState::default();
        let (retire_ack, rx) = ack();
        let (state, effects) = state.apply(SurfaceEvent::Destroyed {
            generation: gen(1),
            ack: retire_ack,
        });
        assert_eq!(kinds(&effects), ["ack"]);
        assert_eq!(state.high_water(), Some(gen(1)));
        for effect in effects {
            if let SurfaceEffect::Ack(ack) = effect {
                ack.send();
            }
        }
        assert!(
            rx.try_recv().is_ok(),
            "nothing is held, so the destroy is acknowledged at once"
        );

        let (state, effects) = state.apply(SurfaceEvent::Created {
            generation: gen(1),
            lease: "late-lease1",
            geometry: Some(geom(100, 200)),
        });
        assert_eq!(kinds(&effects), ["stale:1"]);
        assert_eq!(state.lease(), None);

        let (retire_ack, _rx) = ack();
        let (state, effects) = state.apply(SurfaceEvent::Destroyed {
            generation: gen(1),
            ack: retire_ack,
        });
        assert_eq!(kinds(&effects), ["stale:1", "ack"]);
        assert_eq!(state.high_water(), Some(gen(1)));
    }
}
