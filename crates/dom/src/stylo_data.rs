//! Per-element style state that lets the arena DOM participate in stylo's
//! cascade (design doc §10, ADR-0005).
//!
//! Stylo mutates element data through **shared** references during its style
//! traversal (it guarantees exclusive per-node access via its own threading
//! model, which the Rust borrow checker cannot see). We therefore store the
//! cascade result behind an [`UnsafeCell`] and the engine-facing flags behind
//! [`Cell`]/[`AtomicBool`], mirroring `blitz-dom`'s `node/stylo_data.rs`.
//!
//! Safety of the [`UnsafeCell`] relies on:
//!   - ordinary borrow checking for regular (`&mut`) access, and
//!   - stylo having exclusive access to each node during a style traversal, so
//!     `ensure_init`/`clear` never race a live borrow.
#![allow(unsafe_code)]

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::AtomicBool;

use selectors::matching::ElementSelectorFlags;
use servo_arc::Arc as ServoArc;
use style::data::{ElementDataMut, ElementDataRef, ElementDataWrapper};
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::{ComputedValues, PropertyDeclarationBlock};
use style::shared_lock::Locked;
use style_dom::ElementState;

/// Interior-mutable wrapper around `Option<ElementDataWrapper>`.
///
/// Encapsulates the [`UnsafeCell`] so that access sites don't need raw `unsafe`
/// blocks of their own.
#[derive(Default)]
pub struct StyloData {
    inner: UnsafeCell<Option<ElementDataWrapper>>,
}

impl fmt::Debug for StyloData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyloData").finish_non_exhaustive()
    }
}

impl Deref for StyloData {
    type Target = Option<ElementDataWrapper>;
    fn deref(&self) -> &Self::Target {
        // SAFETY: the only writers through a shared `&self` are the `unsafe`
        // methods `unsafe_stylo_only_mut`/`ensure_init`/`clear`, whose contract
        // requires no other outstanding borrow of `inner`. Stylo upholds that
        // via its exclusive per-node access on a single thread, so this safe
        // read never races a write.
        unsafe { &*self.inner.get() }
    }
}

impl DerefMut for StyloData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.get_mut()
    }
}

impl StyloData {
    /// Whether cascade data has been initialized for this element.
    #[must_use]
    pub fn has_data(&self) -> bool {
        // SAFETY: as in `Deref::deref` — the only `&self` writers are the
        // `unsafe` mutators, which require no concurrent borrow; this safe read
        // never races one.
        unsafe { &*self.inner.get() }.is_some()
    }

    /// Borrow the element data immutably, if present.
    #[must_use]
    pub fn get(&self) -> Option<ElementDataRef<'_>> {
        self.as_ref().map(ElementDataWrapper::borrow)
    }

    /// Borrow the element data mutably, if present.
    pub fn get_mut(&mut self) -> Option<ElementDataMut<'_>> {
        self.as_mut().map(|w| w.borrow_mut())
    }

    /// Inserts a restyle hint on the element's cascade data, if initialized.
    ///
    /// A no-op before the first cascade (the first pass styles everything).
    pub fn insert_hint(&mut self, hint: RestyleHint) {
        if let Some(mut data) = self.get_mut() {
            data.hint.insert(hint);
        }
    }

    /// Initialize the element data if needed and return an exclusive borrow.
    pub fn ensure_init_mut(&mut self) -> ElementDataMut<'_> {
        // SAFETY: exclusive access to `self` (via `&mut self`).
        unsafe { self.ensure_init() }
    }

    /// The primary computed style, if the cascade has produced one.
    #[must_use]
    pub fn primary_styles(&self) -> Option<StyleDataRef<'_>> {
        let has_primary = self
            .get()
            .as_ref()
            .and_then(|d| d.styles.get_primary().cloned())
            .is_some();
        has_primary.then(|| StyleDataRef(self.get().unwrap()))
    }

    /// Get an exclusive borrow of the data for stylo's traversal.
    ///
    /// # Safety
    /// There must be no other outstanding borrow of this container. Stylo's
    /// traversal upholds this via its exclusive per-node access contract.
    pub unsafe fn unsafe_stylo_only_mut(&self) -> Option<ElementDataMut<'_>> {
        // SAFETY: caller upholds exclusivity (see method docs).
        let opt = unsafe { &mut *self.inner.get() };
        opt.as_mut().map(|w| w.borrow_mut())
    }

    /// Initialize the element data if needed and return an exclusive borrow.
    ///
    /// # Safety
    /// There must be no other outstanding borrow of this container.
    pub unsafe fn ensure_init(&self) -> ElementDataMut<'_> {
        if !self.has_data() {
            // SAFETY: caller upholds exclusivity (see method docs).
            unsafe { *self.inner.get() = Some(ElementDataWrapper::default()) };
        }
        // SAFETY: same contract; freshly initialized above if it was empty.
        unsafe { self.unsafe_stylo_only_mut() }.unwrap()
    }

    /// Clear the element data, returning to the uninitialized state.
    ///
    /// # Safety
    /// There must be no other outstanding borrow of this container.
    pub unsafe fn clear(&self) {
        // SAFETY: caller upholds exclusivity (see method docs).
        unsafe { *self.inner.get() = None };
    }
}

/// A borrow that dereferences to the element's primary computed style.
pub struct StyleDataRef<'a>(ElementDataRef<'a>);

impl Deref for StyleDataRef<'_> {
    type Target = ServoArc<ComputedValues>;
    fn deref(&self) -> &Self::Target {
        self.0.styles.get_primary().unwrap()
    }
}

/// All stylo-facing state for one element, embedded in `ElementData`.
///
/// The `Cell`/`AtomicBool` fields are written by stylo through **shared**
/// element references during the style traversal; `element_state` is only
/// written through the DOM mutation path (`&mut`), so it stays a plain field.
pub struct StyloElementState {
    /// The cascade result (computed styles), lazily initialized by stylo.
    pub data: StyloData,
    /// The parsed `style="..."` attribute block, kept in sync by the mutation
    /// path so stylo's cascade and CSSOM read the same declarations.
    pub style_attribute: Option<ServoArc<Locked<PropertyDeclarationBlock>>>,
    /// Pseudo-class state (`:hover`, `:focus`, …); empty until interactivity.
    pub element_state: ElementState,
    /// Selector-matching flags written by `apply_selector_flags`.
    pub selector_flags: Cell<ElementSelectorFlags>,
    /// Stylo's "dirty descendants" bit (distinct from the engine's
    /// `NodeFlags::HAS_DIRTY_DESCENDANT` gate).
    pub dirty_descendants: Cell<bool>,
    /// Whether a snapshot of this element awaits processing by invalidation.
    pub has_snapshot: Cell<bool>,
    /// Whether the pending snapshot has already been handled this pass.
    pub snapshot_handled: AtomicBool,
}

impl Default for StyloElementState {
    fn default() -> Self {
        Self {
            data: StyloData::default(),
            style_attribute: None,
            element_state: ElementState::empty(),
            selector_flags: Cell::new(ElementSelectorFlags::empty()),
            dirty_descendants: Cell::new(false),
            has_snapshot: Cell::new(false),
            snapshot_handled: AtomicBool::new(false),
        }
    }
}

impl fmt::Debug for StyloElementState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyloElementState")
            .field("element_state", &self.element_state)
            .field("has_snapshot", &self.has_snapshot.get())
            .finish_non_exhaustive()
    }
}
