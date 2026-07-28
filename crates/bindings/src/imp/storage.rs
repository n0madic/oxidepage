//! The `Storage` interface over a (possibly shared) [`StorageArea`].
//!
//! Every method takes the area's `Mutex`, does its work, releases it, and only
//! *then* notifies the other documents — the notification callback belongs to
//! another page's thread and may want the same lock, so holding it across the
//! call would be a deadlock waiting for two pages to write at once.
//!
//! A poisoned area is **recovered from**, not propagated. The area is a plain
//! key/value map with no cross-field invariant a panic can break (the byte
//! accounting is `saturating_*`, so it cannot panic mid-update), and it is
//! shared by every page of a browsing context: turning one page's panic into a
//! permanent `TypeError` from every later `localStorage` call in that context
//! would convert a contained failure into a total one. `LoopHooks::storage`
//! takes the same view, and for the same reason.
//!
//! [`StorageArea`]: crate::storage::StorageArea

use std::rc::Rc;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::JsThrow;

use crate::cx::BindCx;
use crate::storage::{QuotaExceeded, StorageHandle, StorageNotification};

/// Runs `f` under the area's lock. Delivering whatever notification `f`
/// produced is [`notify`]'s job, and every mutating caller pairs the two.
fn with_area<T>(
    this: &Rc<StorageHandle>,
    f: impl FnOnce(&mut crate::storage::StorageArea) -> T,
) -> T {
    let area = this.area();
    let mut area = area.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut area)
}

/// Delivers `notification` to every *other* document of the area, and prunes
/// any subscriber that reports itself gone.
///
/// The lock is taken to collect, released for the calls, and taken again only
/// if something needs pruning — a listener runs on another page's thread and
/// must never be called with this area's `Mutex` held.
fn notify(this: &Rc<StorageHandle>, notification: Option<StorageNotification>) {
    let Some(notification) = notification else {
        return;
    };
    let listeners = with_area(this, |area| area.others(this.subscriber));
    let mut dead = Vec::new();
    for (subscriber, listener) in listeners {
        if !listener(notification.clone()) {
            dead.push(subscriber);
        }
    }
    if !dead.is_empty() {
        with_area(this, |area| {
            for subscriber in dead {
                area.unsubscribe(subscriber);
            }
        });
    }
}

/// The URL stamped into a notification: this document's, because *this* is the
/// document doing the writing.
fn writer_url(cx: &BindCx<'_>) -> String {
    cx.state.dom.borrow().document_url().to_owned()
}

pub(crate) fn length(_cx: &BindCx<'_>, this: Rc<StorageHandle>) -> Result<f64, JsThrow> {
    Ok(with_area(&this, |area| area.len() as f64))
}

pub(crate) fn key(
    _cx: &BindCx<'_>,
    this: Rc<StorageHandle>,
    index: u32,
) -> Result<Option<String>, JsThrow> {
    // Through the handle's cached key list: `key_at` is an O(n) B-tree walk and
    // one lock per call, so the enumeration idiom this member exists for —
    // `for (i = 0; i < length; i++) key(i)` — was quadratic.
    Ok(this.keys().get(index as usize).cloned())
}

pub(crate) fn get_item(
    _cx: &BindCx<'_>,
    this: Rc<StorageHandle>,
    key: String,
) -> Result<Option<String>, JsThrow> {
    Ok(with_area(&this, |area| area.get(&key)))
}

pub(crate) fn set_item(
    cx: &BindCx<'_>,
    this: Rc<StorageHandle>,
    key: String,
    value: String,
) -> Result<(), JsThrow> {
    let url = writer_url(cx);
    let origin = this.origin();
    let outcome = with_area(&this, |area| {
        area.set(&key, &value, this.kind, &url, &origin, this.subscriber)
    });
    match outcome {
        Ok(notification) => {
            notify(&this, notification);
            Ok(())
        }
        Err(QuotaExceeded) => Err(cx.dom_throw(
            DomExceptionKind::QuotaExceededError,
            "Failed to execute 'setItem' on 'Storage': \
             the quota has been exceeded",
        )),
    }
}

pub(crate) fn remove_item(
    cx: &BindCx<'_>,
    this: Rc<StorageHandle>,
    key: String,
) -> Result<(), JsThrow> {
    let url = writer_url(cx);
    let origin = this.origin();
    let notification = with_area(&this, |area| {
        area.remove(&key, this.kind, &url, &origin, this.subscriber)
    });
    notify(&this, notification);
    Ok(())
}

pub(crate) fn clear(cx: &BindCx<'_>, this: Rc<StorageHandle>) -> Result<(), JsThrow> {
    let url = writer_url(cx);
    let origin = this.origin();
    let notification = with_area(&this, |area| {
        area.clear(this.kind, &url, &origin, this.subscriber)
    });
    notify(&this, notification);
    Ok(())
}
