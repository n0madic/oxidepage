//! The embedder command port: how a *different* thread asks a page to do
//! something (ADR-0027 D2).
//!
//! A [`Page`](crate::Page) is permanently `!Send` — rquickjs is pinned without
//! `parallel`, and stylo keeps thread-local caches (ADR-0005 D3) — so a driver
//! that wants many pages runs each on its own OS thread and talks to it over a
//! channel. This module is the page-side half of that: the unit of work, and
//! the counters that let a test prove the loop still parks exactly once per
//! iteration.
//!
//! ## Why a closure and not a command enum
//!
//! A `PageCommand` enum mirroring the `Page` API would need ~35 request
//! variants and ~35 response variants, and would have to be extended for every
//! method added afterwards. Worse, it cannot express the API's most useful
//! half: `Page::dom` returns a `Ref<'_, DomTree>`, which physically cannot
//! cross a channel. A boxed closure runs *on the page thread*, with the borrow
//! live in its own scope, and sends back an owned `Send` projection — so
//! `handle.with(|p| p.dom().document_url().to_owned())` just works, and the
//! `engine` crate wraps the typed methods it wants over one generic helper.

use crate::Page;

/// A unit of embedder work, executed on the page thread with `&Page` in hand.
///
/// The closure returns nothing: it captures its own typed reply `Sender` and
/// answers itself, so no type erasure (and no downcast) is involved.
pub struct PageJob {
    control: bool,
    run: Box<dyn FnOnce(&Page) + Send>,
}

impl PageJob {
    /// An ordinary job. It runs at the top of the event loop, under the same
    /// `!navigating && !parsing` guard as a queued navigation: a job sent while
    /// the page is inside `load_document` would otherwise re-enter the engine
    /// under the `RefCell` borrows that load is holding.
    #[must_use]
    pub fn new(f: impl FnOnce(&Page) + Send + 'static) -> Self {
        Self {
            control: false,
            run: Box::new(f),
        }
    }

    /// A control job, executed **immediately** at whatever wait point receives
    /// it — including in the middle of a navigation.
    ///
    /// That is only sound for work that touches `Cell`s and channels and
    /// nothing else: closing the page, answering a dialog, stopping a load.
    /// A control job that reaches the DOM, style, layout or JS is a
    /// `BorrowMutError` waiting for the right timing, so keep the set of
    /// constructors small enough to audit by reading them.
    #[must_use]
    pub fn control(f: impl FnOnce(&Page) + Send + 'static) -> Self {
        Self {
            control: true,
            run: Box::new(f),
        }
    }

    /// Whether this job may run at any wait point.
    pub(crate) fn is_control(&self) -> bool {
        self.control
    }

    pub(crate) fn run(self, page: &Page) {
        (self.run)(page);
    }
}

impl std::fmt::Debug for PageJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageJob")
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

/// Event-loop counters.
///
/// A diagnostic, and the regression test for ADR-0004's "one blocking wait, no
/// busy-wait": a loop that spins shows thousands of `blocking_waits` where a
/// correct one shows a handful. Plain `Cell<u64>`s, not atomics — they are read
/// and written only from the page thread, and the loop increments them on every
/// iteration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoopStats {
    /// Iterations of `run_until_stalled_until`'s inner loop.
    pub turns: u64,
    /// Times the loop entered its blocking wait.
    ///
    /// A *count*, not a measure of parking: a wait whose deadline had already
    /// passed returns instantly and still counts. Read it against elapsed time
    /// — a park shows a handful, a spin shows tens of thousands per second —
    /// or use [`LoopStats::parked_micros`], which cannot be confused.
    pub blocking_waits: u64,
    /// Microseconds actually spent parked in that wait.
    ///
    /// The direct form of ADR-0004's property: a loop that parks accounts for
    /// nearly all of its wall-clock time here, and one that spins accounts for
    /// almost none of it, whatever the call count says.
    pub parked_micros: u64,
    /// Jobs executed (control and ordinary alike).
    pub jobs_run: u64,
    /// Jobs parked because the page was mid-navigation or mid-parse.
    pub jobs_deferred: u64,
}
