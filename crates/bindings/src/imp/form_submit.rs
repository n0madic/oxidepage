//! HTML's **form submission algorithm**, and the reset activation behavior
//! next to it.
//!
//! Submission is a navigation, so — like every other navigation script can
//! trigger — it ends by queueing a [`PendingNavigation`] the page's event loop
//! performs (ADR-0022). This module's job is everything up to that point: the
//! `submit` event, the entry list, and the three encodings.
//!
//! The three `Content-Type` values are the spec's verbatim mime types — no
//! `charset` parameter, because HTML's "submit as entity body" does not add one
//! and the payload is always UTF-8. `accept-charset` is likewise ignored: this
//! engine encodes in UTF-8 and nothing else, so honouring the attribute would
//! mean claiming an encoding the bytes are not in.
//!
//! Deliberately not implemented (each would be a fake, not a simplification):
//! `<input type=image>` (not a listed control, so it has no coordinates to
//! submit), the `formdata` event, constraint validation (`novalidate` is
//! reflected but nothing validates), `method=dialog`, and `javascript:` action
//! URLs. Each of those warns rather than silently doing the wrong thing.

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_dom::DomTree;
use oxidepage_js::JsThrow;
use url::Url;

use crate::cx::BindCx;
use crate::events::{EventData, EventTargetKey, dispatch_event};
use crate::state::{NavigationBody, PendingNavigation};

const URLENCODED: &str = "application/x-www-form-urlencoded";
const MULTIPART: &str = "multipart/form-data";
const TEXT_PLAIN: &str = "text/plain";

/// Submits `form`.
///
/// `fire_event` distinguishes the two entry points HTML itself distinguishes:
/// `form.submit()` submits *without* firing `submit` (and without validating),
/// while a submit button's activation and `requestSubmit()` fire it and honour
/// `preventDefault()`.
pub(crate) fn submit(
    cx: &BindCx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
    fire_event: bool,
) -> Result<(), JsThrow> {
    // HTML's "form cannot navigate" check, step 1 of form submission: a form
    // that is not in a document has no browsing context to navigate, so nothing
    // happens — not even the `submit` event. `Event-dispatch-click.html`'s
    // "disconnected form should not submit" asserts exactly this.
    if !cx
        .state
        .dom
        .borrow()
        .get(form)
        .is_some_and(oxidepage_dom::Node::is_connected)
    {
        return Ok(());
    }
    if fire_event {
        // HTML's **firing submission events** flag, and it guards only this
        // half of the algorithm. The spec checks it under "if submitted from
        // submit() method is false", so it stops a `requestSubmit()` or a
        // button activation raised from inside `onsubmit` from recursing until
        // the script budget kills the page — and deliberately does *not* stop
        // `form.submit()`, which skips the event entirely.
        //
        // Guarding the whole of `submit()` with it instead silently dropped
        // `onsubmit = e => { e.preventDefault(); validate(); form.submit(); }`,
        // the canonical validate-then-submit idiom, leaving the page looking
        // hung with no warning.
        if cx.state.frame.firing_submission_events.get() {
            return Ok(());
        }
        cx.state.frame.firing_submission_events.set(true);
        let proceed = fire_submit_event(cx, form, submitter);
        cx.state.frame.firing_submission_events.set(false);
        if !proceed? {
            return Ok(());
        }
    }
    submit_inner(cx, form, submitter)
}

/// Fires the `submit` event; `false` means the submission must not continue,
/// either because a listener canceled it or because one detached the form.
fn fire_submit_event(
    cx: &BindCx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Result<bool, JsThrow> {
    let data = crate::imp::submit_event::new_trusted_data(cx, submitter)?;
    if !dispatch_event(cx, EventTargetKey::Node(form), &data)? {
        return Ok(false);
    }
    // A listener may have detached the form or removed the submitter.
    let dom = cx.state.dom.borrow();
    Ok(dom.get(form).is_some_and(oxidepage_dom::Node::is_connected))
}

fn submit_inner(cx: &BindCx<'_>, form: NodeId, submitter: Option<NodeId>) -> Result<(), JsThrow> {
    let plan = match plan_submission(cx, form, submitter) {
        Some(plan) => plan,
        None => return Ok(()),
    };
    let entries = crate::imp::form_data::construct_the_entry_list(cx, form, submitter);
    let navigation = encode(cx, &plan, entries);
    // `target` names the context that receives the response — a named frame, an
    // ancestor, or this one (ADR-0035 D10). A name nothing answers to would
    // open a context in a browser; here it falls back to this one, with a
    // warning, rather than silently submitting somewhere else.
    match crate::window_open::resolve_target(&cx.state.frame, &plan.target) {
        Some(context) => context.request_navigation(navigation),
        None => {
            cx.warn(&format!(
                "form submission: target=`{}` names no browsing context; submitting in place",
                plan.target
            ));
            cx.state.request_navigation(navigation);
        }
    }
    Ok(())
}

/// The action URL, method and encoding a submission resolves to. The submitter's
/// `formaction`/`formmethod`/`formenctype` override the form's — that is the
/// entire reason the submitter has to travel this far.
struct Plan {
    action: Url,
    post: bool,
    enctype: &'static str,
    /// The `target`/`formtarget` attribute, uninterpreted. Resolved to a
    /// browsing context at the moment the navigation is queued (ADR-0035 D10).
    target: String,
}

fn plan_submission(cx: &BindCx<'_>, form: NodeId, submitter: Option<NodeId>) -> Option<Plan> {
    let (raw_action, raw_method, raw_enctype, document_url) = {
        let dom = cx.state.dom.borrow();
        let overridden = |form_attr: &str, own_attr: &str| {
            submitter
                .and_then(|s| attr(&dom, s, form_attr))
                .or_else(|| attr(&dom, form, own_attr))
        };
        (
            overridden("formaction", "action"),
            overridden("formmethod", "method"),
            overridden("formenctype", "enctype"),
            // The **submitting** document's URL: an empty `action` is its own
            // URL, and a relative one resolves against it. `dom.document_url()`
            // is the top-level document's, so a form inside a frame submitted
            // to the embedder's URL (ADR-0035 D1).
            dom.document_url_of(cx.state.frame.document()).to_owned(),
        )
    };
    // `target`/`formtarget`: which browsing context receives the response.
    let target = {
        let dom = cx.state.dom.borrow();
        submitter
            .and_then(|s| attr(&dom, s, "formtarget"))
            .or_else(|| attr(&dom, form, "target"))
            .unwrap_or_default()
    };

    // An empty (or absent) action is the document's own URL.
    let action = match raw_action.as_deref().map(str::trim).unwrap_or("") {
        "" => document_url.clone(),
        // Resolve against the base URL, like every other URL-valued attribute.
        _ => match submitter {
            Some(s) if attr_present(cx, s, "formaction") => {
                crate::imp::reflect::reflect_url(cx, s, "formaction")
            }
            _ => crate::imp::reflect::reflect_url(cx, form, "action"),
        },
    };
    let Ok(action) = Url::parse(&action) else {
        cx.warn(&format!(
            "form submission: action `{action}` is not a valid URL; the submission was skipped"
        ));
        return None;
    };
    if action.scheme() == "javascript" {
        cx.warn("form submission: `javascript:` action URLs are not implemented");
        return None;
    }

    // The method attribute is an enumerated attribute whose invalid-value
    // default is `get`.
    let method = raw_method.unwrap_or_default().to_ascii_lowercase();
    if method == "dialog" {
        cx.warn("form submission: `method=dialog` is not implemented (no `<dialog>` support)");
        return None;
    }
    let post = method == "post";

    let enctype = match raw_enctype.unwrap_or_default().to_ascii_lowercase().trim() {
        MULTIPART => MULTIPART,
        TEXT_PLAIN => TEXT_PLAIN,
        // Invalid-value default.
        _ => URLENCODED,
    };
    Some(Plan {
        action,
        post,
        enctype,
        target,
    })
}

/// Turns the entry list into the navigation it produces: a GET rewrites the
/// action URL's query, a POST carries a body.
fn encode(
    cx: &BindCx<'_>,
    plan: &Plan,
    entries: Vec<(String, crate::netdata::FormDataValue)>,
) -> PendingNavigation {
    // A form with a non-empty file input is sent as `multipart/form-data`
    // whatever its `enctype` says: urlencoded and `text/plain` cannot carry
    // bytes, so honouring the author's enctype would send the *filenames* and
    // nothing else — which is what this engine did before file entries existed.
    //
    // A deliberate deviation from HTML, which says to send just the names; it
    // is announced with a warning rather than done quietly, because a silent
    // upload that drops the file is the worse failure by far (ADR-0032's
    // deliberate limits record it).
    let data = crate::netdata::FormDataData::new(entries);
    let enctype = if data.has_selected_file() && plan.post {
        if plan.enctype != MULTIPART {
            cx.warn(&format!(
                "form submission: enctype `{}` cannot carry a file, so \
                 multipart/form-data was used instead",
                plan.enctype
            ));
        }
        MULTIPART
    } else {
        plan.enctype
    };
    if !plan.post {
        // Only the two byte-less encodings need the flattened pairs; multipart
        // reads the entry list itself, files and all.
        let entries = data.pairs();
        // "Mutate action URL": the query is *replaced*, and the fragment of the
        // action URL is dropped.
        let mut url = plan.action.clone();
        url.set_fragment(None);
        url.set_query(Some(&urlencoded(&entries)));
        if plan.enctype != URLENCODED {
            cx.warn(&format!(
                "form submission: enctype `{}` is ignored for method=get",
                plan.enctype
            ));
        }
        return PendingNavigation::Load {
            url: url.to_string(),
            replace: false,
            body: None,
            reload: false,
            download: None,
            // The document the form is in; a `target="side"` submission is
            // queued on another context and must not borrow its referrer.
            initiator: Some(cx.state.frame.document_url()),
        };
    }

    let (bytes, content_type) = match enctype {
        MULTIPART => {
            let boundary = multipart_boundary(cx);
            (
                data.to_multipart(&boundary),
                format!("{MULTIPART}; boundary={boundary}"),
            )
        }
        // Five lines rather than quietly reusing the urlencoded serializer:
        // `text/plain` is a distinct wire format, and encoding it as something
        // else is exactly the "fake" P6 forbids.
        TEXT_PLAIN => {
            let mut out = String::new();
            for (name, value) in &data.pairs() {
                out.push_str(&name.replace(['\r', '\n'], " "));
                out.push('=');
                out.push_str(&value.replace(['\r', '\n'], " "));
                out.push_str("\r\n");
            }
            (out.into_bytes(), TEXT_PLAIN.to_owned())
        }
        _ => (
            urlencoded(&data.pairs()).into_bytes(),
            URLENCODED.to_owned(),
        ),
    };
    PendingNavigation::Load {
        url: plan.action.to_string(),
        replace: false,
        body: Some(NavigationBody {
            method: "POST".to_owned(),
            bytes,
            content_type,
        }),
        reload: false,
        download: None,
        initiator: Some(cx.state.frame.document_url()),
    }
}

/// The `application/x-www-form-urlencoded` serializer `URLSearchParams` already
/// uses, so the two cannot disagree about `+` versus `%20`.
fn urlencoded(entries: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in entries {
        ser.append_pair(name, value);
    }
    ser.finish()
}

/// A multipart boundary that cannot appear in the payload: `getrandom` bytes in
/// hex. A predictable boundary would let a crafted field value terminate the
/// part it sits in and forge the rest of the body.
fn multipart_boundary(cx: &BindCx<'_>) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // The OS CSPRNG is unavailable — fall back to the monotonic clock
        // rather than a constant, and say so.
        cx.warn("form submission: random source unavailable for the multipart boundary");
        let nanos = cx.now_ms().to_bits();
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
    }
    let mut out = String::from("----OxidePageFormBoundary");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `form.requestSubmit(submitter?)`: validate the submitter, then submit with
/// the `submit` event.
pub(crate) fn request_submit(
    cx: &BindCx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Result<(), JsThrow> {
    if let Some(node) = submitter {
        let (is_submit_button, owns) = {
            let dom = cx.state.dom.borrow();
            (is_submit_button(&dom, node), dom.form_owner(node))
        };
        if !is_submit_button {
            return Err(JsThrow::Type(
                "Failed to execute 'requestSubmit' on 'HTMLFormElement': \
                 The specified element is not a submit button."
                    .into(),
            ));
        }
        if owns != Some(form) {
            return Err(cx.dom_throw(
                DomExceptionKind::NotFoundError,
                "Failed to execute 'requestSubmit' on 'HTMLFormElement': \
                 The specified element is not owned by this form element.",
            ));
        }
    }
    submit(cx, form, submitter, /* fire_event */ true)
}

/// Whether `node` is a submit button — `<button>` with a missing/`submit`
/// `type`, or `<input type=submit>`.
fn is_submit_button(dom: &DomTree, node: NodeId) -> bool {
    let Some(el) = dom.get(node).and_then(|n| n.as_element()) else {
        return false;
    };
    if !el.is_html_element() {
        return false;
    }
    match &**el.local_name() {
        "button" => {
            let raw = el
                .attr(&oxidepage_dom::node::attr_name("type".into()))
                .map(|v| v.to_ascii_lowercase())
                .unwrap_or_default();
            !matches!(raw.as_str(), "reset" | "button")
        }
        "input" => oxidepage_dom::input_type(el) == "submit",
        _ => false,
    }
}

/// The reset button's activation behavior: fire a cancelable `reset`, then —
/// unless a listener cancelled — reset the form.
pub(crate) fn reset(cx: &BindCx<'_>, form: NodeId) -> Result<(), JsThrow> {
    let mut data = EventData::new(
        "reset".to_owned(),
        /* bubbles */ true,
        /* cancelable */ true,
        /* composed */ false,
    );
    data.is_trusted = true;
    data.time_stamp = cx.now_ms();
    let data = cx.new_event_data("Event", data);
    if dispatch_event(cx, EventTargetKey::Node(form), &data)? {
        cx.state.dom.borrow_mut().reset_form(form);
    }
    Ok(())
}

fn attr(dom: &DomTree, node: NodeId, name: &str) -> Option<String> {
    dom.get(node)
        .and_then(|n| n.as_element())
        .and_then(|el| el.attr(&oxidepage_dom::node::attr_name(name.into())))
        .map(ToString::to_string)
}

fn attr_present(cx: &BindCx<'_>, node: NodeId, name: &str) -> bool {
    attr(&cx.state.dom.borrow(), node, name).is_some_and(|v| !v.trim().is_empty())
}
