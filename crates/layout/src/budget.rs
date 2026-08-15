//! Wall-clock budget for one layout flush (ADR-0037).
//!
//! The engine's only other budget — `page`'s `ScriptBudget` — is enforced
//! through QuickJS's interrupt callback and therefore covers *only* the time a
//! task spends in JavaScript. Layout runs in Rust, where nothing polls that
//! callback, so a hostile document that makes the layout pass itself expensive
//! (deeply nested intrinsic sizing, pathological float/line interactions, an
//! overlarge `repeat(auto-fill, …)` grid) wedges the page thread with no way
//! back out.
//!
//! This module is the missing half. Its shape is copied from `ScriptBudget`
//! **including the ownership semantics**, because they carry the same load
//! here: the outermost [`arm`] owns the deadline and a nested one is a no-op,
//! so one flush has one budget however many times it re-enters the crate.
//!
//! The deadline is polled on a stride from every unbounded loop in the crate
//! ([`checkpoint`]); tripping it raises a typed panic that
//! [`LayoutEngine::reflow`](crate::LayoutEngine::reflow) catches, classifies
//! and reports as [`LayoutAborted`]. Panicking is the only way out of taffy's
//! recursion without forking taffy — the crate is entered through
//! `taffy::compute_root_layout`, which has no error channel.

use std::any::Any;
use std::cell::Cell;
use std::fmt;
use std::time::{Duration, Instant};

/// How many [`checkpoint`] calls pass between two `Instant::now()` reads.
///
/// The counter starts at zero, so the **first** checkpoint under a fresh
/// budget always measures — which is what makes [`Duration::ZERO`] a
/// deterministic trigger for tests rather than a race with the clock.
pub(crate) const STRIDE: u32 = 512;

/// Why a layout flush ended without a layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutAborted {
    /// The flush outran its wall-clock budget.
    Deadline { limit: Duration },
    /// A panic raised by the layout pass itself — today, taffy's own overflow
    /// guards on an overlarge grid (ADR-0036 D5).
    EnginePanic(String),
}

impl fmt::Display for LayoutAborted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutAborted::Deadline { limit } => {
                write!(f, "layout exceeded its {limit:?} budget")
            }
            LayoutAborted::EnginePanic(message) => {
                write!(f, "layout failed: {message}")
            }
        }
    }
}

impl std::error::Error for LayoutAborted {}

/// The armed budget of the current thread.
///
/// Thread-local rather than a field on `LayoutEngine` because [`checkpoint`]
/// is called from deep inside taffy's recursion, where no engine reference is
/// in scope — the same reason `ScriptBudget` hangs off the runtime's opaque
/// interrupt handler rather than off the task.
struct BudgetState {
    /// `Some` while a flush is on the stack and the budget is enabled.
    deadline: Cell<Option<Instant>>,
    /// The limit the live deadline was armed with, reported by the abort.
    limit: Cell<Duration>,
    /// Checkpoints remaining before the next clock read.
    countdown: Cell<u32>,
}

thread_local! {
    static BUDGET: BudgetState = const {
        BudgetState {
            deadline: Cell::new(None),
            limit: Cell::new(Duration::MAX),
            countdown: Cell::new(0),
        }
    };
}

/// Disarms the budget it armed, on the way out — including on an unwind, which
/// is how the deadline never outlives the flush that set it.
pub struct LayoutBudgetGuard {
    /// Whether this guard armed the live deadline. A nested [`arm`] leaves the
    /// outer one's deadline alone and clears nothing on drop.
    owns: bool,
}

impl Drop for LayoutBudgetGuard {
    fn drop(&mut self) {
        if self.owns {
            BUDGET.with(|budget| budget.deadline.set(None));
        }
    }
}

/// Arms a wall-clock budget for the layout work done while the guard lives.
///
/// A no-op — and free at every checkpoint — when `limit` is [`Duration::MAX`]
/// (the budget is disabled) or when a budget is already on the stack: the
/// outermost call owns the deadline, exactly as `ScriptBudget::arm` does.
/// [`Duration::ZERO`] trips at the first checkpoint, for deterministic tests.
#[must_use]
pub fn arm(limit: Duration) -> LayoutBudgetGuard {
    BUDGET.with(|budget| {
        if limit == Duration::MAX || budget.deadline.get().is_some() {
            return LayoutBudgetGuard { owns: false };
        }
        budget.limit.set(limit);
        budget.deadline.set(Some(Instant::now() + limit));
        budget.countdown.set(0);
        LayoutBudgetGuard { owns: true }
    })
}

/// Polls the deadline, on a stride. Panics with a [`LayoutAborted::Deadline`]
/// payload once it has passed.
///
/// Free (one thread-local read and a branch) when no budget is armed, which is
/// what every test, benchmark and unbudgeted embedder sees.
///
/// The deadline is deliberately **not** cleared when it trips: a flush walks
/// several frames under one budget, and clearing it would leave every frame
/// after the first unbudgeted. The cost is that a checkpoint reached while
/// already unwinding would panic again — an abort — so no `Drop` impl in this
/// crate may call this, and none does: every call site is the top of a loop or
/// of a compute function.
#[inline]
pub(crate) fn checkpoint() {
    BUDGET.with(|budget| {
        let Some(deadline) = budget.deadline.get() else {
            return;
        };
        let countdown = budget.countdown.get();
        if countdown > 0 {
            budget.countdown.set(countdown - 1);
            return;
        }
        budget.countdown.set(STRIDE - 1);
        if Instant::now() >= deadline {
            trip(budget.limit.get());
        }
    });
}

#[cold]
#[inline(never)]
fn trip(limit: Duration) -> ! {
    std::panic::panic_any(LayoutAborted::Deadline { limit })
}

/// Runs `f` inside the crate's landing pad, turning a tripped deadline — and
/// any other panic raised by the layout pass — into a typed error.
///
/// **The caller owns the unwind-safety argument.** `AssertUnwindSafe` is
/// applied here because a budgeted pass mutates by nature and no signature
/// could express otherwise; what makes each boundary sound is what its caller
/// does with the half-written state afterwards. `LayoutEngine::reflow`
/// discards the box tree wholesale; a caller that cannot say something equally
/// concrete has no business calling this.
pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, LayoutAborted> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(classify)
}

/// Sorts a caught panic payload into "our deadline" and "someone else's
/// panic".
///
/// A foreign panic is **not** resumed: unwinding it further kills the page
/// thread, and taffy's `u16` overflow guard on an overlarge grid
/// (ADR-0036 D5) is precisely the panic this boundary exists to contain.
fn classify(payload: Box<dyn Any + Send>) -> LayoutAborted {
    match payload.downcast::<LayoutAborted>() {
        Ok(aborted) => *aborted,
        Err(payload) => {
            LayoutAborted::EnginePanic(oxidepage_base::panic_message(&payload, "layout panicked"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a budget is armed on this thread, for the tests below.
    fn armed() -> bool {
        BUDGET.with(|budget| budget.deadline.get().is_some())
    }

    #[test]
    fn an_unarmed_checkpoint_is_a_no_op() {
        assert!(!armed());
        for _ in 0..STRIDE * 4 {
            checkpoint();
        }
    }

    #[test]
    fn a_max_budget_arms_nothing() {
        let _guard = arm(Duration::MAX);
        assert!(!armed());
        for _ in 0..STRIDE * 4 {
            checkpoint();
        }
    }

    #[test]
    fn a_zero_budget_trips_at_the_first_checkpoint() {
        let outcome = std::panic::catch_unwind(|| {
            let _guard = arm(Duration::ZERO);
            checkpoint();
        });
        let aborted = classify(outcome.expect_err("the zero budget must trip"));
        assert_eq!(
            aborted,
            LayoutAborted::Deadline {
                limit: Duration::ZERO
            }
        );
    }

    #[test]
    fn the_stride_holds_the_clock_off() {
        let _guard = arm(Duration::from_secs(3600));
        // The first checkpoint reads the clock and reloads the counter, so the
        // next `STRIDE - 1` must not.
        checkpoint();
        BUDGET.with(|budget| assert_eq!(budget.countdown.get(), STRIDE - 1));
        for expected in (0..STRIDE - 1).rev() {
            checkpoint();
            BUDGET.with(|budget| assert_eq!(budget.countdown.get(), expected));
        }
        checkpoint();
        BUDGET.with(|budget| assert_eq!(budget.countdown.get(), STRIDE - 1));
    }

    #[test]
    fn a_guard_disarms_on_an_unwind() {
        let outcome = std::panic::catch_unwind(|| {
            let _guard = arm(Duration::from_secs(3600));
            assert!(armed());
            panic!("something else");
        });
        assert!(outcome.is_err());
        assert!(!armed(), "the guard must disarm while unwinding");
    }

    #[test]
    fn a_nested_arm_neither_owns_nor_clears_the_deadline() {
        let outer = arm(Duration::from_secs(3600));
        let deadline = BUDGET.with(|budget| budget.deadline.get());
        {
            let _inner = arm(Duration::from_secs(1));
            assert_eq!(
                BUDGET.with(|budget| budget.deadline.get()),
                deadline,
                "a nested arm must not move the deadline"
            );
        }
        assert!(armed(), "a nested guard must not disarm on drop");
        drop(outer);
        assert!(!armed());
    }

    #[test]
    fn a_foreign_payload_is_classified_as_an_engine_panic() {
        let outcome = std::panic::catch_unwind(|| panic!("grid overflowed"));
        let aborted = classify(outcome.expect_err("the closure panics"));
        assert_eq!(
            aborted,
            LayoutAborted::EnginePanic("grid overflowed".to_owned())
        );
    }
}
