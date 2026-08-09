//! `WindowProxy`: the handle `window.open` returns (ADR-0027 D12), and the one
//! `iframe.contentWindow` / `window.parent` / `window.top` return
//! (ADR-0035 D4).
//!
//! For a **sibling** everything is an atomic read or a fire-and-forget message:
//! it lives on another thread with its own realm, and a getter that blocked on
//! a round trip would deadlock the first time two pages opened each other.
//!
//! For a **frame** of this page the members are real and synchronous — same
//! thread, same event loop. Even so no `JsValue` crosses: the proxy is an
//! object of the *accessing* realm, and reaching the child's global would mean
//! sharing a `Runtime`, which is what would make nested delivery a
//! `BorrowMutError` (ADR-0033 D1).

use std::rc::Rc;
use std::sync::atomic::Ordering;

use oxidepage_base::DomExceptionKind;
use oxidepage_js::{JsThrow, JsValue};

use crate::cx::BindCx;
use crate::window_open::{WindowOp, WindowProxyData};

pub(crate) fn closed(cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<bool, JsThrow> {
    match &*this {
        WindowProxyData::Sibling(window) => Ok(window.closed.load(Ordering::Acquire)),
        // A frame is "closed" once its context is gone — which is what a
        // detached `<iframe>` leaves a script holding.
        WindowProxyData::Frame(frame) => Ok(cx.frame_state(*frame).is_none()),
    }
}

pub(crate) fn close(_cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<(), JsThrow> {
    match &*this {
        WindowProxyData::Sibling(window) => {
            // Set the flag here as well as asking the sibling to go: a browser
            // reports `w.closed === true` on the very next line, and waiting
            // for the other thread to acknowledge would make that a race.
            window.closed.store(true, Ordering::Release);
            (window.ops)(WindowOp::Close);
        }
        // Per HTML, `close()` only applies to a context script opened. A frame
        // is closed by removing its element, not from inside.
        WindowProxyData::Frame(_) => {}
    }
    Ok(())
}

/// There is no window manager here, so focusing a browsing context has no
/// intrinsic effect. The embedder is *told* rather than obeyed — which is what
/// keeps this from being the silent no-op P6 forbids.
pub(crate) fn focus(_cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<(), JsThrow> {
    match &*this {
        WindowProxyData::Sibling(window) => (window.ops)(WindowOp::Focus),
        // Focus is page-global here and there is no window manager, so focusing
        // a frame changes nothing observable. Told rather than obeyed for a
        // sibling; for a frame there is nobody to tell.
        WindowProxyData::Frame(_) => {}
    }
    Ok(())
}

/// Reading a sibling's `location` throws, exactly as it does for a cross-origin
/// `WindowProxy` in a browser — which is what this *is*: a separate browsing
/// context this realm cannot synchronously introspect.
pub(crate) fn location(cx: &BindCx<'_>, this: Rc<WindowProxyData>) -> Result<JsValue, JsThrow> {
    // A frame of this page *can* be introspected — same thread, one arena — so
    // a same-origin read answers with a real `location`, as a browser does.
    // Only the cross-origin and cross-thread cases throw.
    if let WindowProxyData::Frame(frame) = &*this
        && let Some(state) = cx.frame_state(*frame)
        && cx.same_origin_frame(&state)
    {
        return frame_location_object(cx, *frame);
    }
    Err(cx.dom_throw(
        DomExceptionKind::SecurityError,
        "Failed to read the 'location' property from 'WindowProxy': \
         cannot read the location of another browsing context",
    ))
}

/// `w.location = url` navigates the sibling. Resolved against the *opener's*
/// document, which is what HTML says and what needs no round trip.
pub(crate) fn set_location(
    cx: &BindCx<'_>,
    this: Rc<WindowProxyData>,
    value: JsValue,
) -> Result<(), JsThrow> {
    let url = cx.scope.coerce_string(&value)?;
    let WindowProxyData::Sibling(window) = &*this else {
        // Navigating a frame from its embedder is a frame navigation, not a
        // sibling message: it goes through the element's `src`, which is the
        // task source the event loop already drains (ADR-0035 D5).
        return set_frame_location(cx, &this, &url);
    };
    if window.closed.load(Ordering::Acquire) {
        return Ok(());
    }
    // Resolved against the accessing realm's **current** document, read now —
    // not against a snapshot taken when `window.open` returned. The realm
    // outlives a navigation, so a proxy captured before one would otherwise
    // keep resolving against a URL this context has left, and send its sibling
    // to a different origin than the script asked for. The realm's *own*
    // document, not the page's: a frame that opened a window resolves against
    // itself (ADR-0035 D1).
    let base = cx
        .state
        .dom
        .borrow()
        .document_url_of(cx.state.frame.document())
        .to_owned();
    let resolved = crate::window_open::resolve_against(&base, &url);
    (window.ops)(WindowOp::Navigate(resolved));
    Ok(())
}

/// `frame.location = url`: writes the owning `<iframe>`'s `src`, which the DOM
/// queues and the event loop performs.
///
/// Same-origin only, as HTML says — and it is a *write*, which a cross-origin
/// context is allowed to refuse loudly rather than silently.
fn set_frame_location(
    cx: &BindCx<'_>,
    this: &Rc<WindowProxyData>,
    url: &str,
) -> Result<(), JsThrow> {
    let WindowProxyData::Frame(frame) = &**this else {
        return Ok(());
    };
    let Some(state) = cx.frame_state(*frame) else {
        return Ok(()); // the context is gone
    };
    if !cx.same_origin_frame(&state) {
        return Err(cx.dom_throw(
            DomExceptionKind::SecurityError,
            "cannot navigate a cross-origin browsing context",
        ));
    }
    let Some(owner) = cx.frame_owner(*frame) else {
        return Ok(()); // the top-level context has no `<iframe>` to write
    };
    // The *accessing* document's URL: a relative target is relative to the
    // realm doing the navigating, not to the page (ADR-0035 D1).
    let base = cx
        .state
        .dom
        .borrow()
        .document_url_of(cx.state.frame.document())
        .to_owned();
    let resolved = crate::window_open::resolve_against(&base, url);
    cx.state.dom.borrow_mut().set_attribute(
        owner,
        oxidepage_dom::node::attr_name("src".into()),
        resolved.into(),
    );
    Ok(())
}

/// `postMessage`: hands a value to another browsing context.
///
/// **Delivered as a task, never synchronously.** A listener in the receiver
/// commonly answers with a `postMessage` back, and a synchronous entry would
/// ride the native stack until `MAX_WORLD_DEPTH` caught it (ADR-0035 D4).
///
/// The value is serialized here, in the *sender's* realm, and deserialized in
/// the receiver's — which is what keeps every `JsValue` inside its own runtime.
/// That serializer is a JSON subset, so `Map`, `Set`, `Date`, `ArrayBuffer`,
/// typed arrays and cycles are refused rather than silently flattened.
pub(crate) fn post_message(
    cx: &BindCx<'_>,
    this: Rc<WindowProxyData>,
    message: JsValue,
    target_origin: JsValue,
) -> Result<(), JsThrow> {
    let target = match &*this {
        WindowProxyData::Frame(frame) => *frame,
        // A sibling page is another OS thread with its own event loop; there is
        // no channel for a message to travel and inventing one that dropped it
        // would be the silent no-op P6 forbids.
        WindowProxyData::Sibling(_) => {
            return Err(cx.dom_throw(
                DomExceptionKind::NotSupportedError,
                "postMessage to a window opened with window.open is not supported",
            ));
        }
    };
    let Some(state) = cx.frame_state(target) else {
        return Ok(()); // the context is gone; HTML drops the message
    };
    // `targetOrigin` is the sender saying *who it believes it is talking to*.
    // Accepting it and ignoring it — which this did — turns
    // `child.postMessage(token, "https://trusted.example")` into a delivery to
    // whatever the frame actually is, which is the exact confusion the argument
    // exists to prevent.
    //
    // HTML: `*` matches anything, `/` matches the sender's own origin, anything
    // else is parsed (a `SyntaxError` if it will not) and compared at delivery.
    // A mismatch **drops the message silently** — telling the sender would leak
    // the target's origin to a page not entitled to it.
    let target_origin = cx.scope.coerce_string(&target_origin)?;
    let sender_origin = crate::imp::htmli_frame_element::origin_of(
        cx.state
            .dom
            .borrow()
            .document_url_of(cx.state.frame.document()),
    );
    if target_origin != "*" {
        let wanted = if target_origin == "/" {
            sender_origin.clone()
        } else {
            match url::Url::parse(&target_origin) {
                Ok(url) => crate::imp::htmli_frame_element::origin_of(url.as_str()),
                Err(_) => {
                    return Err(cx.dom_throw(
                        DomExceptionKind::SyntaxError,
                        "postMessage: targetOrigin is not a valid origin",
                    ));
                }
            }
        };
        let actual = crate::imp::htmli_frame_element::origin_of(
            cx.state.dom.borrow().document_url_of(state.document()),
        );
        // An opaque origin (`origin_of` spells it `"null"`) matches nothing but
        // `*` — it is not equal to itself, which is what makes a sandboxed
        // frame unaddressable by name.
        if wanted == "null" || actual == "null" || wanted != actual {
            return Ok(());
        }
    }
    let serialized = clone_message(cx, &message)?;
    let origin = sender_origin;
    cx.state
        .frame
        .global
        .queue_message(crate::state::PendingMessage {
            target,
            source: cx.state.frame.frame(),
            origin,
            data: serialized,
        });
    Ok(())
}

/// Serializes a message body, refusing what the subset cannot carry.
///
/// Not `serialize_for_event`: that is `JSON.stringify`, which turns a `Map`
/// into `{}` and a `Date` into a string — a page would be told its value
/// travelled when it did not. The bootstrap's `cloneForMessage` walks first and
/// throws, which is what turns the limit into a `DataCloneError` the page can
/// see (ADR-0035 D4).
fn clone_message(cx: &BindCx<'_>, message: &JsValue) -> Result<String, JsThrow> {
    let clone = cx.with_js(|js| js.clone_for_message.clone())?;
    let refused = |cx: &BindCx<'_>| {
        cx.dom_throw(
            DomExceptionKind::DataCloneError,
            "the message could not be cloned: postMessage carries a JSON subset \
             (no Map, Set, Date, ArrayBuffer, typed arrays, cycles or transferables)",
        )
    };
    let text = cx
        .scope
        .call(&clone, &JsValue::Undefined, std::slice::from_ref(message))
        .map_err(|_| refused(cx))?;
    match text {
        JsValue::String(text) => Ok(text),
        // `undefined` in, `undefined` out: JSON has no representation for it,
        // and HTML's clone of `undefined` is `undefined`.
        _ => Ok("null".to_owned()),
    }
}

/// A `location` for a same-origin frame, minted in the **accessing** realm.
///
/// An object rather than the URL string, because `w.location.href = url` is the
/// idiom pages actually write — a string would let that assignment succeed and
/// do nothing, which is the silent no-op P6 forbids and which a page waiting on
/// the frame's `load` never recovers from. Not the child's own `Location`: that
/// is a value of another runtime and could not cross (ADR-0035 D4).
///
/// `href`, `assign` and `replace` all navigate through the same path a `src`
/// write takes, so the load stays a task.
fn frame_location_object(
    cx: &BindCx<'_>,
    frame: oxidepage_base::FrameId,
) -> Result<JsValue, JsThrow> {
    let object = cx.scope.new_object().map_err(JsThrow::from)?;
    let read_href = {
        let cx_state = Rc::clone(&cx.state);
        move |scope: &dyn oxidepage_js::JsScope| -> Result<JsValue, JsThrow> {
            let _ = scope;
            let Some(state) = cx_state.frame.global.frame_state(frame) else {
                return Ok(JsValue::String(String::from("about:blank")));
            };
            let url = cx_state
                .dom
                .borrow()
                .document_url_of(state.document())
                .to_owned();
            Ok(JsValue::String(url))
        }
    };
    let getter = JsValue::Object(
        cx.scope
            .new_function("get href", 0, {
                let read_href = read_href.clone();
                Rc::new(move |scope: &dyn oxidepage_js::JsScope, _call| read_href(scope))
            })
            .map_err(JsThrow::from)?,
    );
    let navigate = move |scope: &dyn oxidepage_js::JsScope,
                         call: oxidepage_js::HostCall|
          -> Result<JsValue, JsThrow> {
        let cx = BindCx {
            scope,
            state: crate::cx::world_state(scope)?,
        };
        let value = call.args.first().cloned().unwrap_or(JsValue::Undefined);
        let url = cx.scope.coerce_string(&value)?;
        let data = Rc::new(WindowProxyData::Frame(frame));
        set_frame_location(&cx, &data, &url)?;
        Ok(JsValue::Undefined)
    };
    let setter = JsValue::Object(
        cx.scope
            .new_function("set href", 1, Rc::new(navigate))
            .map_err(JsThrow::from)?,
    );
    cx.scope
        .define_property(
            &object,
            "href",
            oxidepage_js::PropertyDef::Accessor {
                getter: Some(&getter),
                setter: Some(&setter),
                enumerable: true,
                configurable: false,
            },
        )
        .map_err(JsThrow::from)?;
    for name in ["assign", "replace"] {
        let f = JsValue::Object(
            cx.scope
                .new_function(name, 1, Rc::new(navigate))
                .map_err(JsThrow::from)?,
        );
        cx.scope.set(&object, name, &f).map_err(JsThrow::from)?;
    }
    let to_string = JsValue::Object(
        cx.scope
            .new_function("toString", 0, {
                let read_href = read_href.clone();
                Rc::new(move |scope: &dyn oxidepage_js::JsScope, _call| read_href(scope))
            })
            .map_err(JsThrow::from)?,
    );
    cx.scope
        .set(&object, "toString", &to_string)
        .map_err(JsThrow::from)?;
    Ok(JsValue::Object(object))
}
