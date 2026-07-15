//! `BindCx`: the per-call bindings context. Bundles the active JS scope with
//! the page state and provides WebIDL argument conversion, wrapper cache
//! management (§5.3 identity + pin contract), interface registration for the
//! generated glue, and exception construction.

use std::cell::RefCell;
use std::rc::Rc;

use oxidepage_base::{DomException, DomExceptionKind, NodeId};
use oxidepage_dom::{NodeData, NodeKind};
use oxidepage_js::{HostCall, HostFn, JsObject, JsScope, JsThrow, JsValue, PropertyDef};

use crate::collections::CollectionData;
use crate::cssdata::{RuleData, RuleListData, SheetData, StyleDeclData};
use crate::events::{EventData, EventTargetKey};
use crate::netdata::{
    FormDataData, HeadersData, RequestData, ResponseData, UrlData, UrlSearchParamsData, XhrData,
};
use crate::state::{
    AbortSignalData, AttrData, HostData, InterfaceEntry, IntersectionObserverData, IoEntryView,
    JsRefs, MediaQueryListData, NavigatorData, PageState, RecordView, RectData, ResizeObserverData,
    RoEntryView, ScreenData, TAG_NODE, TAG_SLAB,
};

/// Glue-function signature emitted by the codegen.
pub(crate) type NativeFn = fn(&BindCx<'_>, &HostCall) -> Result<JsValue, JsThrow>;

/// Constructor behavior for [`BindCx::finish_interface`].
pub(crate) enum CtorSpec {
    /// `new X()` throws `TypeError: Illegal constructor`.
    Illegal,
    /// IDL-declared constructor.
    Native { length: u32, construct: NativeFn },
}

/// The bindings context: an entered scope plus the page state.
pub struct BindCx<'a> {
    pub scope: &'a dyn JsScope,
    pub state: Rc<PageState>,
}

/// The page state installed in `scope`'s realm. Host callbacks recover it here
/// rather than capturing it, which keeps JS→Rust reference cycles impossible.
pub(crate) fn page_state(scope: &dyn JsScope) -> Result<Rc<PageState>, JsThrow> {
    scope
        .state()
        .and_then(|s| s.downcast::<PageState>().ok())
        .ok_or_else(|| JsThrow::Type("no page state installed in this realm".into()))
}

/// Wraps a plain glue function into an engine `HostFn`. The closure captures
/// nothing stateful: the page state is recovered from the scope at call time.
pub(crate) fn native(f: NativeFn) -> HostFn {
    native_inner(f, false)
}

/// [`native`] for a `[CEReactions]` member: the call is scoped in a custom
/// element reactions stack entry, so every reaction it enqueues (upgrade,
/// connected, disconnected, attributeChanged) is invoked *before it returns to
/// script* — the spec's timing, and what `el.innerHTML = '<my-el>'` followed by
/// a method call on the new element depends on (ADR-0021).
pub(crate) fn native_ce(f: NativeFn) -> HostFn {
    native_inner(f, true)
}

fn native_inner(f: NativeFn, ce: bool) -> HostFn {
    Rc::new(move |scope, call| {
        let cx = BindCx {
            scope,
            state: page_state(scope)?,
        };
        // "Push a new element queue": a mark into the DOM's FIFO. A nested
        // `[CEReactions]` call marks above ours, so the slices cannot interleave.
        let mark = ce.then(|| cx.state.dom.borrow().custom_reaction_mark());
        let result = f(&cx, &call);
        if result.is_ok() {
            // A DOM mutation may have added or removed element ids; the names
            // must be visible to the script that resumes after this call, and
            // to any inline script the call connected.
            crate::sync_named_properties(&cx)?;
            // "Pop the element queue and invoke its reactions." Gated on
            // `is_ok` like the sibling hooks: a DOM operation validates before
            // it mutates, so a throwing one enqueued nothing — and were that
            // ever untrue, the reactions are not lost, they fall through to the
            // enclosing operation's drain or to the microtask checkpoint.
            if let Some(mark) = mark {
                crate::invoke_custom_element_reactions(&cx, mark);
            }
            // …and if it queued a MutationObserver record, the spec queues the
            // compound microtask *now*, so it is ordered against promise
            // reactions queued later in this same task. This is the one point
            // where the DOM borrow is released but no further JS has run. It
            // runs after the reactions so that records *they* queued (a
            // connectedCallback that mutates the DOM) join this same microtask.
            cx.queue_mutation_microtask()?;
            // Apply wrapper retention for nodes this call connected/disconnected
            // *synchronously*: a later allocation in the same task could GC an
            // unretained wrapper — losing its expando state — before the event
            // loop's deferred drain runs. Cheap no-op unless the call moved a
            // pinned (JS-wrapped) node across the connectedness boundary.
            crate::drain_pinned_connectivity(&cx);
        }
        // The host-call boundary is where a DOM insertion has finished but the
        // calling script has not resumed: the point at which a script-inserted
        // inline classic script must run. A throwing operation connected
        // nothing, so nothing can be pending.
        if result.is_ok() && !cx.state.dom.borrow().script_updates().is_empty() {
            crate::script::run_pending_inline_scripts(&cx);
        }
        result
    })
}

/// `[SameObject]` member key for `CSSStyleSheet.cssRules`. Shared so the list's
/// cache and its invalidation cannot drift apart.
pub(crate) const CSS_RULES_MEMBER: &str = "cssom-cssRules";

/// The interface an HTML element with local name `local` is wrapped as.
///
/// One map, two consumers — [`BindCx::node_to_js`] picks the prototype and
/// [`BindCx::this_html_iface`] brand-checks against it — so a new per-tag
/// interface cannot be half-wired.
pub(crate) fn html_interface_for(local: &str) -> &'static str {
    match local {
        "a" => "HTMLAnchorElement",
        "area" => "HTMLAreaElement",
        "img" => "HTMLImageElement",
        "link" => "HTMLLinkElement",
        "form" => "HTMLFormElement",
        "script" => "HTMLScriptElement",
        "html" => "HTMLHtmlElement",
        "head" => "HTMLHeadElement",
        "body" => "HTMLBodyElement",
        "title" => "HTMLTitleElement",
        "meta" => "HTMLMetaElement",
        "base" => "HTMLBaseElement",
        "style" => "HTMLStyleElement",
        "template" => "HTMLTemplateElement",
        "div" => "HTMLDivElement",
        "span" => "HTMLSpanElement",
        "p" => "HTMLParagraphElement",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "HTMLHeadingElement",
        "pre" => "HTMLPreElement",
        "blockquote" | "q" => "HTMLQuoteElement",
        "br" => "HTMLBRElement",
        "hr" => "HTMLHRElement",
        "ul" => "HTMLUListElement",
        "ol" => "HTMLOListElement",
        "li" => "HTMLLIElement",
        "dl" => "HTMLDListElement",
        "input" => "HTMLInputElement",
        "button" => "HTMLButtonElement",
        "select" => "HTMLSelectElement",
        "optgroup" => "HTMLOptGroupElement",
        "option" => "HTMLOptionElement",
        "textarea" => "HTMLTextAreaElement",
        "label" => "HTMLLabelElement",
        "fieldset" => "HTMLFieldSetElement",
        "legend" => "HTMLLegendElement",
        "table" => "HTMLTableElement",
        "thead" | "tbody" | "tfoot" => "HTMLTableSectionElement",
        "tr" => "HTMLTableRowElement",
        "td" | "th" => "HTMLTableCellElement",
        "col" | "colgroup" => "HTMLTableColElement",
        "caption" => "HTMLTableCaptionElement",
        "iframe" => "HTMLIFrameElement",
        "canvas" => "HTMLCanvasElement",
        "picture" => "HTMLPictureElement",
        "source" => "HTMLSourceElement",
        "video" => "HTMLVideoElement",
        "audio" => "HTMLAudioElement",
        "track" => "HTMLTrackElement",
        "object" => "HTMLObjectElement",
        "embed" => "HTMLEmbedElement",
        "map" => "HTMLMapElement",
        "datalist" => "HTMLDataListElement",
        "output" => "HTMLOutputElement",
        "progress" => "HTMLProgressElement",
        "meter" => "HTMLMeterElement",
        "details" => "HTMLDetailsElement",
        "dialog" => "HTMLDialogElement",
        "menu" => "HTMLMenuElement",
        "time" => "HTMLTimeElement",
        "data" => "HTMLDataElement",
        "ins" | "del" => "HTMLModElement",
        "slot" => "HTMLSlotElement",
        _ => "HTMLElement",
    }
}

fn pack_node(id: NodeId) -> u64 {
    (u64::from(id.index()) << 32) | u64::from(id.generation().get())
}

pub(crate) fn unpack_node(data: u64) -> Option<NodeId> {
    let index = (data >> 32) as u32;
    let generation = std::num::NonZeroU32::new((data & 0xFFFF_FFFF) as u32)?;
    Some(NodeId::from_parts(index, generation))
}

impl BindCx<'_> {
    // === JS-side helper access ===

    fn with_js<T>(&self, f: impl FnOnce(&JsRefs) -> T) -> Result<T, JsThrow> {
        let js = self.state.js.borrow();
        let refs = js
            .as_ref()
            .ok_or_else(|| JsThrow::Type("bindings bootstrap not installed".into()))?;
        Ok(f(refs))
    }

    /// The DOM spec's "queue a mutation observer microtask", called from the
    /// host-call trampoline — i.e. at the moment a record was queued.
    ///
    /// This is what orders observer delivery *against* promise reactions
    /// instead of after all of them: `await Promise.resolve()` later in the
    /// same task must see records queued before it. Delivering observers only
    /// once `pump_jobs()` had drained the whole job queue inverted that.
    ///
    /// The trampoline is the one place where the mutating call has returned
    /// (so no `dom` borrow is held) and no further JS has run yet.
    pub(crate) fn queue_mutation_microtask(&self) -> Result<(), JsThrow> {
        if self.state.mutation_microtask_queued.get() {
            return Ok(());
        }
        if !self.state.dom.borrow().observers().has_pending_records() {
            return Ok(());
        }
        let (enqueue, notify) =
            self.with_js(|js| (js.enqueue_microtask.clone(), js.mutation_notify.clone()))?;
        self.state.mutation_microtask_queued.set(true);
        self.call_helper(&enqueue, &[notify])?;
        Ok(())
    }

    fn call_helper(&self, helper: &JsValue, args: &[JsValue]) -> Result<JsValue, JsThrow> {
        self.scope
            .call(helper, &JsValue::Undefined, args)
            .map_err(JsThrow::from)
    }

    // === Exceptions ===

    /// Builds a real `DOMException` throw from an engine-side error.
    pub fn dom_exception(&self, error: DomException) -> JsThrow {
        self.dom_throw(error.kind, error.message)
    }

    /// Builds a `DOMException` throw by spec name.
    pub fn dom_throw(&self, kind: DomExceptionKind, message: &str) -> JsThrow {
        let make = match self.with_js(|js| js.make_dom_exception.clone()) {
            Ok(make) => make,
            Err(_) => return JsThrow::Type(format!("{}: {message}", kind.name())),
        };
        match self.call_helper(
            &make,
            &[
                JsValue::String(kind.name().to_owned()),
                JsValue::String(message.to_owned()),
            ],
        ) {
            Ok(value) => JsThrow::Value(value),
            Err(_) => JsThrow::Type(format!("{}: {message}", kind.name())),
        }
    }

    /// Builds a `DOMException` value (not a throw) by spec name — for
    /// `AbortSignal.reason` and similar carried-as-value exceptions.
    pub(crate) fn make_dom_exception_value(
        &self,
        name: &str,
        message: &str,
    ) -> Result<JsValue, JsThrow> {
        let make = self.with_js(|js| js.make_dom_exception.clone())?;
        self.call_helper(
            &make,
            &[
                JsValue::String(name.to_owned()),
                JsValue::String(message.to_owned()),
            ],
        )
    }

    // === Wrapper cache (one wrapper per node per realm; wrapper pins node) ===

    /// Returns the unique JS wrapper for `id`, creating (and pinning) it on
    /// first access.
    ///
    /// The cache is keyed by arena *index*, so both the requested id and any hit
    /// must be checked against the generation. A freed node's index is reused by
    /// the next allocation, and a stale [`NodeId`] — one stored without a pin,
    /// such as a delivered `MutationRecord.target` or an `Event.target` — would
    /// otherwise resolve to the wrapper of whatever unrelated node now occupies
    /// that slot.
    pub fn node_to_js(&self, id: NodeId) -> Result<JsValue, JsThrow> {
        // A generation-checked liveness test, before the index-keyed lookup.
        if self.state.dom.borrow().get(id).is_none() {
            return Err(self.dom_throw(DomExceptionKind::InvalidStateError, "stale node"));
        }

        let (map, cache_get) = self.with_js(|js| (js.wrapper_map.clone(), js.cache_get.clone()))?;
        let key = JsValue::Number(f64::from(id.index()));
        let cached = self.call_helper(&cache_get, &[map.clone(), key.clone()])?;
        // A hit whose payload names a different generation belongs to a node that
        // used to live in this slot; drop it and mint a fresh wrapper.
        if !cached.is_undefined() && self.payload(&cached) == Some((TAG_NODE, pack_node(id))) {
            return Ok(cached);
        }

        // Create a fresh wrapper with the interface prototype for the node's
        // kind, cache it weakly, and pin the node.
        let iface = {
            let dom = self.state.dom.borrow();
            let node = dom
                .get(id)
                .ok_or_else(|| self.dom_throw(DomExceptionKind::InvalidStateError, "stale node"))?;
            match node.data() {
                // `createDocument()` exposes XMLDocument; `new Document()` does
                // not, though both are XML documents. The bit rides on the node.
                NodeData::Document(doc) => {
                    if doc.xml_document_interface {
                        "XMLDocument"
                    } else {
                        "Document"
                    }
                }
                NodeData::DocumentFragment {
                    shadow: Some(_), ..
                } => "ShadowRoot",
                NodeData::DocumentFragment { .. } => "DocumentFragment",
                NodeData::Doctype { .. } => "DocumentType",
                NodeData::Text(_) => "Text",
                NodeData::CdataSection(_) => "CDATASection",
                NodeData::Comment(_) => "Comment",
                NodeData::ProcessingInstruction { .. } => "ProcessingInstruction",
                NodeData::Element(el) => {
                    if el.is_html_element() {
                        html_interface_for(&el.name.local)
                    } else if el.is_svg_element() {
                        // `<a>` is the one SVG element with a member (`href`);
                        // every other SVG-namespace element, `<svg>` included,
                        // lands on the base interface, because the rest of the
                        // SVG DOM is not implemented.
                        match &*el.name.local {
                            "a" => "SVGAElement",
                            _ => "SVGElement",
                        }
                    } else {
                        "Element"
                    }
                }
            }
        };
        let proto = self.interface_proto(iface)?;
        let wrapper = self
            .scope
            .new_host_object(Some(&proto), TAG_NODE, pack_node(id))
            .map_err(JsThrow::from)?;
        let wrapper = JsValue::Object(wrapper);
        let cache_set = self.with_js(|js| js.cache_set.clone())?;
        self.call_helper(&cache_set, &[map, key, wrapper.clone()])?;
        self.state.dom.borrow_mut().pin(id);
        // A wrapper minted for an already-connected node must be retained
        // strongly at once: the weak cache would otherwise let it — and any
        // author-set expando properties on it — be collected while the node
        // lives on in the tree. Later connect/disconnect transitions are handled
        // by the pinned-connectivity queue (see `drain_pinned_connectivity`).
        if self
            .state
            .dom
            .borrow()
            .get(id)
            .is_some_and(|node| node.is_connected())
        {
            self.state
                .connected_wrappers
                .borrow_mut()
                .insert(id, wrapper.clone());
        }
        Ok(wrapper)
    }

    /// Returns the cached wrapper for `id` if one currently exists, without
    /// minting a new one. The generation check mirrors [`Self::node_to_js`]: a
    /// cache hit whose payload names a different generation belongs to a node
    /// that used to live in this slot and is ignored.
    pub(crate) fn peek_node_wrapper(&self, id: NodeId) -> Option<JsValue> {
        let (map, cache_get) = self
            .with_js(|js| (js.wrapper_map.clone(), js.cache_get.clone()))
            .ok()?;
        let key = JsValue::Number(f64::from(id.index()));
        let cached = self.call_helper(&cache_get, &[map, key]).ok()?;
        if !cached.is_undefined() && self.payload(&cached) == Some((TAG_NODE, pack_node(id))) {
            Some(cached)
        } else {
            None
        }
    }

    pub fn opt_node_to_js(&self, id: Option<NodeId>) -> Result<JsValue, JsThrow> {
        match id {
            Some(id) => self.node_to_js(id),
            None => Ok(JsValue::Null),
        }
    }

    /// Removes `key` from `obj` (`JsScope` exposes no property deletion).
    pub(crate) fn delete_property(&self, obj: &JsObject, key: &str) -> Result<(), JsThrow> {
        let delete = self.with_js(|js| js.delete_property.clone())?;
        self.call_helper(
            &delete,
            &[
                JsValue::Object(obj.clone()),
                JsValue::String(key.to_owned()),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn interface_proto(&self, name: &str) -> Result<JsObject, JsThrow> {
        let interfaces = self.state.interfaces.borrow();
        interfaces
            .get(name)
            .map(|entry| entry.proto.clone())
            .ok_or_else(|| JsThrow::Type(format!("interface `{name}` is not registered")))
    }

    // === Host payload unwrapping (brand checks) ===

    fn payload(&self, value: &JsValue) -> Option<(u32, u64)> {
        self.scope.host_payload(value)
    }

    /// `this` as a live node (any kind).
    pub fn this_node(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        let Some((TAG_NODE, data)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a Node".into()));
        };
        let id = unpack_node(data).ok_or_else(|| JsThrow::Type("corrupt node payload".into()))?;
        if self.state.dom.borrow().get(id).is_none() {
            return Err(
                self.dom_throw(DomExceptionKind::InvalidStateError, "node no longer exists")
            );
        }
        Ok(id)
    }

    fn this_node_of_kinds(
        &self,
        value: &JsValue,
        kinds: &[NodeKind],
        expected: &str,
    ) -> Result<NodeId, JsThrow> {
        let id = self.this_node(value)?;
        let kind = self.state.dom.borrow().node(id).data().kind();
        if kinds.contains(&kind) {
            Ok(id)
        } else {
            Err(JsThrow::Type(format!("receiver is not a {expected}")))
        }
    }

    pub fn this_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::Element], "Element")
    }

    /// `this` as an element whose tag maps to the per-tag interface `iface`.
    fn this_html_iface(&self, value: &JsValue, iface: &str) -> Result<NodeId, JsThrow> {
        let id = self.this_element(value)?;
        let matches = self
            .state
            .dom
            .borrow()
            .node(id)
            .as_element()
            .is_some_and(|el| el.is_html_element() && html_interface_for(&el.name.local) == iface);
        if matches {
            Ok(id)
        } else {
            Err(JsThrow::Type(format!("receiver is not an {iface}")))
        }
    }

    pub fn this_html_script_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLScriptElement")
    }

    pub fn this_html_anchor_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLAnchorElement")
    }

    pub fn this_html_area_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLAreaElement")
    }

    pub fn this_html_image_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLImageElement")
    }

    pub fn this_html_link_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLLinkElement")
    }

    pub fn this_html_form_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLFormElement")
    }

    pub fn this_html_input_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLInputElement")
    }

    pub fn this_html_text_area_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLTextAreaElement")
    }

    pub fn this_html_select_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLSelectElement")
    }

    pub fn this_html_option_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLOptionElement")
    }

    pub fn this_html_opt_group_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLOptGroupElement")
    }

    pub fn this_html_button_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLButtonElement")
    }

    pub fn this_html_label_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLLabelElement")
    }

    pub fn this_html_field_set_element(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_html_iface(value, "HTMLFieldSetElement")
    }

    pub fn this_document(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::Document], "Document")
    }

    pub fn this_document_type(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::Doctype], "DocumentType")
    }

    pub fn this_document_fragment(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::DocumentFragment], "DocumentFragment")
    }

    pub fn this_shadow_root(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        let id = self.this_node_of_kinds(value, &[NodeKind::DocumentFragment], "ShadowRoot")?;
        if self.state.dom.borrow().is_shadow_root(id) {
            Ok(id)
        } else {
            Err(JsThrow::Type("receiver is not a ShadowRoot".into()))
        }
    }

    pub fn this_character_data(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(
            value,
            &[
                NodeKind::Text,
                NodeKind::CdataSection,
                NodeKind::Comment,
                NodeKind::ProcessingInstruction,
            ],
            "CharacterData",
        )
    }

    /// `CDATASection : Text`, so `splitText`/`wholeText` accept one.
    pub fn this_text(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::Text, NodeKind::CdataSection], "Text")
    }

    pub fn this_cdata_section(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::CdataSection], "CDATASection")
    }

    pub fn this_comment(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(value, &[NodeKind::Comment], "Comment")
    }

    pub fn this_processing_instruction(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.this_node_of_kinds(
            value,
            &[NodeKind::ProcessingInstruction],
            "ProcessingInstruction",
        )
    }

    /// `this` as an event target: a node wrapper or the window (global).
    pub fn this_event_target(&self, value: &JsValue) -> Result<EventTargetKey, JsThrow> {
        // WebIDL's ES binding substitutes the realm's global object for a `null`
        // or `undefined` receiver, which is what an unqualified call passes —
        // `addEventListener("error", …)`, exactly how `testharness.js` installs
        // its own error handler. Without this the call throws mid-file, the
        // harness is left half-initialized, and every later `setup({…})` dies on
        // a null output handler. `this_window` has always applied the same rule.
        if matches!(value, JsValue::Undefined | JsValue::Null) {
            return Ok(EventTargetKey::Window);
        }
        if let Some((TAG_NODE, _)) = self.payload(value) {
            return Ok(EventTargetKey::Node(self.this_node(value)?));
        }
        if let Some((TAG_SLAB, key)) = self.payload(value)
            && matches!(
                self.state.slab.borrow().get(key),
                Some(HostData::MediaQueryList(_))
            )
        {
            return Ok(EventTargetKey::MediaQueryList(key));
        }
        if let Some((TAG_SLAB, key)) = self.payload(value)
            && matches!(
                self.state.slab.borrow().get(key),
                Some(HostData::AbortSignal(_))
            )
        {
            return Ok(EventTargetKey::AbortSignal(key));
        }
        if let Some((TAG_SLAB, key)) = self.payload(value)
            && matches!(
                self.state.slab.borrow().get(key),
                Some(HostData::EventTarget(_))
            )
        {
            return Ok(EventTargetKey::Host(key));
        }
        let global = self.with_js(|js| js.global.clone())?;
        if self.scope.strict_equals(value, &JsValue::Object(global)) {
            return Ok(EventTargetKey::Window);
        }
        Err(JsThrow::Type("receiver is not an EventTarget".into()))
    }

    /// `this` as the realm's one Window/global object.
    pub(crate) fn this_window(&self, value: &JsValue) -> Result<EventTargetKey, JsThrow> {
        // WebIDL's ES binding substitutes the current realm's global object for
        // a `null` or `undefined` receiver, which is what an unqualified call
        // (`matchMedia(...)`) passes. Any other foreign receiver still fails the
        // brand check.
        if matches!(value, JsValue::Undefined | JsValue::Null) {
            return Ok(EventTargetKey::Window);
        }
        let global = self.with_js(|js| js.global.clone())?;
        if self.scope.strict_equals(value, &JsValue::Object(global)) {
            Ok(EventTargetKey::Window)
        } else {
            Err(JsThrow::Type("receiver is not a Window".into()))
        }
    }

    fn slab_data<T>(
        &self,
        value: &JsValue,
        expected: &str,
        f: impl FnOnce(&HostData) -> Option<T>,
    ) -> Result<T, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type(format!("receiver is not a {expected}")));
        };
        let slab = self.state.slab.borrow();
        slab.get(key)
            .and_then(f)
            .ok_or_else(|| JsThrow::Type(format!("receiver is not a {expected}")))
    }

    pub fn this_event(
        &self,
        value: &JsValue,
    ) -> Result<Rc<std::cell::RefCell<EventData>>, JsThrow> {
        self.slab_data(value, "Event", |data| match data {
            HostData::Event(ev) => Some(Rc::clone(ev)),
            _ => None,
        })
    }

    /// Collection handle (`NodeList`/`HTMLCollection` share storage).
    pub fn this_node_list(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a NodeList".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::Collection(_)) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a NodeList".into())),
        }
    }

    pub fn this_html_collection(&self, value: &JsValue) -> Result<u64, JsThrow> {
        self.this_node_list(value)
    }

    pub fn this_token_list(&self, value: &JsValue) -> Result<u64, JsThrow> {
        self.this_node_list(value)
    }

    pub(crate) fn this_svg_animated_string(
        &self,
        value: &JsValue,
    ) -> Result<(NodeId, oxidepage_dom::LocalName), JsThrow> {
        self.slab_data(value, "SVGAnimatedString", |data| match data {
            HostData::SvgAnimatedString { element, attr } => Some((*element, attr.clone())),
            _ => None,
        })
    }

    pub(crate) fn this_named_node_map(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.slab_data(value, "NamedNodeMap", |data| match data {
            HostData::NamedNodeMap(owner) => Some(*owner),
            _ => None,
        })
    }

    pub(crate) fn this_attr(&self, value: &JsValue) -> Result<Rc<AttrData>, JsThrow> {
        self.slab_data(value, "Attr", |data| match data {
            HostData::Attr(attr) => Some(Rc::clone(attr)),
            _ => None,
        })
    }

    pub(crate) fn this_navigator(&self, value: &JsValue) -> Result<Rc<NavigatorData>, JsThrow> {
        self.slab_data(value, "Navigator", |data| match data {
            HostData::Navigator(navigator) => Some(Rc::clone(navigator)),
            _ => None,
        })
    }

    pub(crate) fn this_screen(&self, value: &JsValue) -> Result<Rc<ScreenData>, JsThrow> {
        self.slab_data(value, "Screen", |data| match data {
            HostData::Screen(screen) => Some(Rc::clone(screen)),
            _ => None,
        })
    }

    pub(crate) fn this_performance(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a Performance".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::Performance) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a Performance".into())),
        }
    }

    pub(crate) fn this_performance_timing(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a PerformanceTiming".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::PerformanceTiming) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a PerformanceTiming".into())),
        }
    }

    pub(crate) fn new_performance_timing(&self) -> Result<JsValue, JsThrow> {
        self.new_slab_object("PerformanceTiming", HostData::PerformanceTiming)
    }

    pub(crate) fn this_font_face_set(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a FontFaceSet".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::FontFaceSet) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a FontFaceSet".into())),
        }
    }

    pub(crate) fn new_font_face_set(&self) -> Result<JsValue, JsThrow> {
        self.new_slab_object("FontFaceSet", HostData::FontFaceSet)
    }

    /// Brand-checks the `customElements` receiver. The registry state lives in
    /// [`PageState::custom_elements`], so the slab key is returned only as a
    /// brand token (the imp functions ignore it).
    pub(crate) fn this_custom_element_registry(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type(
                "receiver is not a CustomElementRegistry".into(),
            ));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::CustomElementRegistry) => Ok(key),
            _ => Err(JsThrow::Type(
                "receiver is not a CustomElementRegistry".into(),
            )),
        }
    }

    pub(crate) fn this_media_query_list(
        &self,
        value: &JsValue,
    ) -> Result<Rc<MediaQueryListData>, JsThrow> {
        self.slab_data(value, "MediaQueryList", |data| match data {
            HostData::MediaQueryList(list) => Some(Rc::clone(list)),
            _ => None,
        })
    }

    pub(crate) fn this_abort_signal(
        &self,
        value: &JsValue,
    ) -> Result<Rc<AbortSignalData>, JsThrow> {
        self.slab_data(value, "AbortSignal", |data| match data {
            HostData::AbortSignal(data) => Some(Rc::clone(data)),
            _ => None,
        })
    }

    pub(crate) fn this_abort_controller(
        &self,
        value: &JsValue,
    ) -> Result<Rc<AbortSignalData>, JsThrow> {
        self.slab_data(value, "AbortController", |data| match data {
            HostData::AbortController(data) => Some(Rc::clone(data)),
            _ => None,
        })
    }

    pub(crate) fn this_resize_observer(
        &self,
        value: &JsValue,
    ) -> Result<Rc<ResizeObserverData>, JsThrow> {
        self.slab_data(value, "ResizeObserver", |data| match data {
            HostData::ResizeObserver(data) => Some(Rc::clone(data)),
            _ => None,
        })
    }

    pub(crate) fn this_resize_observer_entry(
        &self,
        value: &JsValue,
    ) -> Result<Rc<RoEntryView>, JsThrow> {
        self.slab_data(value, "ResizeObserverEntry", |data| match data {
            HostData::ResizeObserverEntry(view) => Some(Rc::clone(view)),
            _ => None,
        })
    }

    /// Creates a `ResizeObserver`, registering it in the delivery registry and
    /// caching its wrapper on the shared data.
    pub(crate) fn new_resize_observer(&self, callback: JsValue) -> Result<JsValue, JsThrow> {
        let data = Rc::new(ResizeObserverData {
            callback,
            wrapper: RefCell::new(None),
            targets: RefCell::new(Vec::new()),
        });
        let wrapper =
            self.new_slab_object("ResizeObserver", HostData::ResizeObserver(Rc::clone(&data)))?;
        *data.wrapper.borrow_mut() = Some(wrapper.clone());
        self.state.resize_observers.borrow_mut().push(data);
        Ok(wrapper)
    }

    /// Creates a `ResizeObserverEntry` wrapper from precomputed member values.
    pub(crate) fn new_resize_observer_entry(&self, view: RoEntryView) -> Result<JsValue, JsThrow> {
        self.new_slab_object(
            "ResizeObserverEntry",
            HostData::ResizeObserverEntry(Rc::new(view)),
        )
    }

    pub(crate) fn this_intersection_observer(
        &self,
        value: &JsValue,
    ) -> Result<Rc<IntersectionObserverData>, JsThrow> {
        self.slab_data(value, "IntersectionObserver", |data| match data {
            HostData::IntersectionObserver(data) => Some(Rc::clone(data)),
            _ => None,
        })
    }

    pub(crate) fn this_intersection_observer_entry(
        &self,
        value: &JsValue,
    ) -> Result<Rc<IoEntryView>, JsThrow> {
        self.slab_data(value, "IntersectionObserverEntry", |data| match data {
            HostData::IntersectionObserverEntry(view) => Some(Rc::clone(view)),
            _ => None,
        })
    }

    /// Creates an `IntersectionObserver` from already-parsed init, registering
    /// it in the delivery registry and caching its wrapper.
    pub(crate) fn new_intersection_observer(
        &self,
        data: IntersectionObserverData,
    ) -> Result<JsValue, JsThrow> {
        let data = Rc::new(data);
        let wrapper = self.new_slab_object(
            "IntersectionObserver",
            HostData::IntersectionObserver(Rc::clone(&data)),
        )?;
        *data.wrapper.borrow_mut() = Some(wrapper.clone());
        self.state.intersection_observers.borrow_mut().push(data);
        Ok(wrapper)
    }

    /// Creates an `IntersectionObserverEntry` wrapper from precomputed values.
    pub(crate) fn new_intersection_observer_entry(
        &self,
        view: IoEntryView,
    ) -> Result<JsValue, JsThrow> {
        self.new_slab_object(
            "IntersectionObserverEntry",
            HostData::IntersectionObserverEntry(Rc::new(view)),
        )
    }

    pub(crate) fn this_plugin_array(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a PluginArray".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::PluginArray) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a PluginArray".into())),
        }
    }

    pub(crate) fn this_mime_type_array(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a MimeTypeArray".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::MimeTypeArray) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a MimeTypeArray".into())),
        }
    }

    pub fn this_observer(
        &self,
        value: &JsValue,
    ) -> Result<oxidepage_dom::MutationObserverId, JsThrow> {
        self.slab_data(value, "MutationObserver", |data| match data {
            HostData::Observer(id) => Some(*id),
            _ => None,
        })
    }

    pub(crate) fn this_mutation_record(&self, value: &JsValue) -> Result<Rc<RecordView>, JsThrow> {
        self.slab_data(value, "MutationRecord", |data| match data {
            HostData::MutationRecord(view) => Some(Rc::clone(view)),
            _ => None,
        })
    }

    // === Phase 3 network interface unwraps ===

    pub(crate) fn this_url(&self, value: &JsValue) -> Result<Rc<UrlData>, JsThrow> {
        self.slab_data(value, "URL", |data| match data {
            HostData::Url(u) => Some(Rc::clone(u)),
            _ => None,
        })
    }

    pub(crate) fn this_url_search_params(
        &self,
        value: &JsValue,
    ) -> Result<Rc<UrlSearchParamsData>, JsThrow> {
        self.slab_data(value, "URLSearchParams", |data| match data {
            HostData::UrlSearchParams(p) => Some(Rc::clone(p)),
            _ => None,
        })
    }

    pub(crate) fn this_form_data(&self, value: &JsValue) -> Result<Rc<FormDataData>, JsThrow> {
        self.slab_data(value, "FormData", |data| match data {
            HostData::FormData(f) => Some(Rc::clone(f)),
            _ => None,
        })
    }

    /// A non-throwing brand check, for the places that must *recognise* a
    /// `FormData` among other body types rather than demand one.
    pub(crate) fn as_form_data(&self, value: &JsValue) -> Option<Rc<FormDataData>> {
        self.this_form_data(value).ok()
    }

    pub(crate) fn this_headers(
        &self,
        value: &JsValue,
    ) -> Result<Rc<RefCell<HeadersData>>, JsThrow> {
        self.slab_data(value, "Headers", |data| match data {
            HostData::Headers(h) => Some(Rc::clone(h)),
            _ => None,
        })
    }

    pub(crate) fn this_request(&self, value: &JsValue) -> Result<Rc<RequestData>, JsThrow> {
        self.slab_data(value, "Request", |data| match data {
            HostData::Request(r) => Some(Rc::clone(r)),
            _ => None,
        })
    }

    pub(crate) fn this_response(&self, value: &JsValue) -> Result<Rc<ResponseData>, JsThrow> {
        self.slab_data(value, "Response", |data| match data {
            HostData::Response(r) => Some(Rc::clone(r)),
            _ => None,
        })
    }

    pub(crate) fn this_xhr(&self, value: &JsValue) -> Result<Rc<RefCell<XhrData>>, JsThrow> {
        self.slab_data(value, "XMLHttpRequest", |data| match data {
            HostData::Xhr(x) => Some(Rc::clone(x)),
            _ => None,
        })
    }

    // === Phase 4 CSSOM unwraps / construction ===

    pub(crate) fn this_style_decl(&self, value: &JsValue) -> Result<Rc<StyleDeclData>, JsThrow> {
        self.slab_data(value, "CSSStyleDeclaration", |data| match data {
            HostData::StyleDecl(d) => Some(Rc::clone(d)),
            _ => None,
        })
    }

    /// Creates a `CSSStyleDeclaration` host object wrapped in the style proxy
    /// (camelCase/dashed/indexed property access).
    pub(crate) fn new_style_decl(&self, data: StyleDeclData) -> Result<JsValue, JsThrow> {
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::StyleDecl(Rc::new(data)));
        let proto = self.interface_proto("CSSStyleDeclaration")?;
        let target = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        let proxy_fn = self.with_js(|js| js.style_proxy.clone())?;
        self.call_helper(&proxy_fn, &[JsValue::Object(target)])
    }

    /// Creates the `DOMStringMap` for `element.dataset`: a Proxy over a bare
    /// object with the `DOMStringMap` prototype, closing over the element's
    /// wrapper. All `data-*` reads and writes flow through the element's own
    /// attribute methods (see the `datasetProxy` bootstrap helper), so there is
    /// no host object to unwrap.
    pub(crate) fn new_dataset(&self, node: NodeId) -> Result<JsValue, JsThrow> {
        let element = self.node_to_js(node)?;
        let proto = self.interface_proto("DOMStringMap")?;
        let proxy_fn = self.with_js(|js| js.dataset_proxy.clone())?;
        self.call_helper(&proxy_fn, &[element, JsValue::Object(proto)])
    }

    pub(crate) fn this_style_sheet(&self, value: &JsValue) -> Result<Rc<SheetData>, JsThrow> {
        self.slab_data(value, "CSSStyleSheet", |data| match data {
            HostData::StyleSheet(s) => Some(Rc::clone(s)),
            _ => None,
        })
    }

    pub(crate) fn this_style_sheet_list(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.slab_data(value, "StyleSheetList", |data| match data {
            HostData::StyleSheetList(node) => Some(*node),
            _ => None,
        })
    }

    /// A `DOMImplementation` carries the document it was minted for, so a saved
    /// `implementation` keeps creating documents against its own document.
    pub(crate) fn this_dom_implementation(&self, value: &JsValue) -> Result<NodeId, JsThrow> {
        self.slab_data(value, "DOMImplementation", |data| match data {
            HostData::DomImplementation(node) => Some(*node),
            _ => None,
        })
    }

    pub(crate) fn new_dom_implementation(&self, document: NodeId) -> Result<JsValue, JsThrow> {
        self.new_slab_object("DOMImplementation", HostData::DomImplementation(document))
    }

    /// A `DOMParser` brand. Stateless, so it yields its slab key, matching
    /// `this_performance`/`this_plugin_array`.
    pub(crate) fn this_dom_parser(&self, value: &JsValue) -> Result<u64, JsThrow> {
        let Some((TAG_SLAB, key)) = self.payload(value) else {
            return Err(JsThrow::Type("receiver is not a DOMParser".into()));
        };
        match self.state.slab.borrow().get(key) {
            Some(HostData::DomParser) => Ok(key),
            _ => Err(JsThrow::Type("receiver is not a DOMParser".into())),
        }
    }

    pub(crate) fn this_css_rule(&self, value: &JsValue) -> Result<Rc<RuleData>, JsThrow> {
        self.slab_data(value, "CSSRule", |data| match data {
            HostData::CssRule(r) => Some(Rc::clone(r)),
            _ => None,
        })
    }

    pub(crate) fn this_css_rule_list(&self, value: &JsValue) -> Result<Rc<RuleListData>, JsThrow> {
        self.slab_data(value, "CSSRuleList", |data| match data {
            HostData::CssRuleList(l) => Some(Rc::clone(l)),
            _ => None,
        })
    }

    /// The current author stylesheet owned by `owner` (a `<style>`/`<link>`
    /// node), if the sheet is still attached. Resolved live so a re-parsed
    /// `<style>` (a new underlying `Arc`) is followed, not a stale snapshot.
    pub(crate) fn sheet_for(
        &self,
        owner: NodeId,
    ) -> Option<style::stylesheets::DocumentStyleSheet> {
        self.state.style.borrow().sheet_for_node(owner).cloned()
    }

    /// A clone of the document's shared style lock (for CSSOM reads/writes of
    /// locked stylesheet/rule data).
    pub(crate) fn style_lock(&self) -> style::shared_lock::SharedRwLock {
        self.state.style.borrow().lock().clone()
    }

    /// The document URL as stylo URL data (for parsing CSSOM values/selectors).
    pub(crate) fn doc_url(&self) -> style::stylesheets::UrlExtraData {
        self.state.dom.borrow().url_extra_data().clone()
    }

    /// Creates a `CSSStyleSheet` host object (identity is managed by the caller
    /// via `same_object`).
    pub(crate) fn new_style_sheet(&self, data: SheetData) -> Result<JsValue, JsThrow> {
        self.new_slab_object("CSSStyleSheet", HostData::StyleSheet(Rc::new(data)))
    }

    /// Creates a `StyleSheetList` host object wrapped in the indexing proxy.
    pub(crate) fn new_style_sheet_list(&self, document: NodeId) -> Result<JsValue, JsThrow> {
        self.new_indexed("StyleSheetList", HostData::StyleSheetList(document))
    }

    /// Creates a `CSSRule` host object; `Style` rules get the `CSSStyleRule`
    /// prototype, everything else the base `CSSRule` (v1).
    pub(crate) fn new_css_rule(&self, data: RuleData) -> Result<JsValue, JsThrow> {
        let interface = if matches!(data.rule, style::stylesheets::CssRule::Style(_)) {
            "CSSStyleRule"
        } else {
            "CSSRule"
        };
        self.new_slab_object(interface, HostData::CssRule(Rc::new(data)))
    }

    /// Creates a `CSSRuleList` host object wrapped in the indexing proxy.
    pub(crate) fn new_css_rule_list(&self, data: RuleListData) -> Result<JsValue, JsThrow> {
        self.new_indexed("CSSRuleList", HostData::CssRuleList(Rc::new(data)))
    }

    /// Creates a slab-backed host object wrapped in the (unnamed) collection
    /// indexing proxy — for the CSSOM `*List` interfaces.
    fn new_indexed(&self, interface: &str, data: HostData) -> Result<JsValue, JsThrow> {
        self.new_indexed_with_names(interface, data, false)
    }

    fn new_indexed_with_names(
        &self,
        interface: &str,
        data: HostData,
        named: bool,
    ) -> Result<JsValue, JsThrow> {
        let key = self.state.slab.borrow_mut().insert(data);
        let proto = self.interface_proto(interface)?;
        let target = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        let proxy_fn = self.with_js(|js| js.collection_proxy.clone())?;
        self.call_helper(&proxy_fn, &[JsValue::Object(target), JsValue::Bool(named)])
    }

    pub(crate) fn new_named_node_map(&self, owner: NodeId) -> Result<JsValue, JsThrow> {
        self.new_indexed_with_names("NamedNodeMap", HostData::NamedNodeMap(owner), true)
    }

    pub(crate) fn new_attr(
        &self,
        owner: NodeId,
        name: oxidepage_dom::QualName,
    ) -> Result<JsValue, JsThrow> {
        self.new_slab_object("Attr", HostData::Attr(Rc::new(AttrData { owner, name })))
    }

    pub(crate) fn new_navigator(&self) -> Result<JsValue, JsThrow> {
        self.new_slab_object(
            "Navigator",
            HostData::Navigator(Rc::clone(&self.state.navigator)),
        )
    }

    pub(crate) fn new_screen(&self) -> Result<JsValue, JsThrow> {
        self.new_slab_object("Screen", HostData::Screen(Rc::clone(&self.state.screen)))
    }

    pub(crate) fn new_performance(&self) -> Result<JsValue, JsThrow> {
        self.new_slab_object("Performance", HostData::Performance)
    }

    pub(crate) fn new_media_query_list(
        &self,
        media: String,
        matches: bool,
    ) -> Result<JsValue, JsThrow> {
        let data = Rc::new(MediaQueryListData {
            media,
            matches: std::cell::Cell::new(matches),
            key: std::cell::Cell::new(None),
            wrapper: RefCell::new(None),
        });
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::MediaQueryList(Rc::clone(&data)));
        data.key.set(Some(key));
        let proto = self.interface_proto("MediaQueryList")?;
        let wrapper = JsValue::Object(
            self.scope
                .new_host_object(Some(&proto), TAG_SLAB, key)
                .map_err(JsThrow::from)?,
        );
        *data.wrapper.borrow_mut() = Some(wrapper.clone());
        self.state.media_queries.borrow_mut().push(data);
        Ok(wrapper)
    }

    /// Creates a fresh `AbortController` and its `AbortSignal`, sharing one
    /// [`AbortSignalData`]. Returns the controller wrapper; the signal wrapper
    /// is cached in the shared data (returned by `controller.signal`).
    /// The slab key behind an `AbortSignal` value, for a WebIDL argument typed
    /// `AbortSignal` (non-nullable): anything else — including `null` — is a
    /// conversion failure, i.e. a `TypeError`.
    pub(crate) fn abort_signal_key(&self, value: &JsValue) -> Result<u64, JsThrow> {
        if let Some((TAG_SLAB, key)) = self.payload(value)
            && matches!(
                self.state.slab.borrow().get(key),
                Some(HostData::AbortSignal(_))
            )
        {
            return Ok(key);
        }
        Err(JsThrow::Type("value is not an AbortSignal".into()))
    }

    /// Whether the `AbortSignal` behind `key` has already been aborted.
    pub(crate) fn abort_signal_aborted(&self, key: u64) -> bool {
        matches!(
            self.state.slab.borrow().get(key),
            Some(HostData::AbortSignal(data)) if data.aborted.get()
        )
    }

    /// `new EventTarget()`: a slab-backed target that is in no tree. It holds its
    /// own wrapper so `event.target` returns the object the script constructed.
    pub(crate) fn new_event_target(&self) -> Result<JsValue, JsThrow> {
        let data = Rc::new(crate::state::EventTargetData {
            wrapper: RefCell::new(None),
        });
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::EventTarget(Rc::clone(&data)));
        let proto = self.interface_proto("EventTarget")?;
        let wrapper = JsValue::Object(
            self.scope
                .new_host_object(Some(&proto), TAG_SLAB, key)
                .map_err(JsThrow::from)?,
        );
        *data.wrapper.borrow_mut() = Some(wrapper.clone());
        Ok(wrapper)
    }

    pub(crate) fn new_abort_controller(&self) -> Result<JsValue, JsThrow> {
        let data = Rc::new(AbortSignalData {
            aborted: std::cell::Cell::new(false),
            reason: RefCell::new(JsValue::Undefined),
            key: std::cell::Cell::new(None),
            wrapper: RefCell::new(None),
            pending_fetches: RefCell::new(Vec::new()),
        });
        let signal_key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::AbortSignal(Rc::clone(&data)));
        data.key.set(Some(signal_key));
        let signal_proto = self.interface_proto("AbortSignal")?;
        let signal_wrapper = JsValue::Object(
            self.scope
                .new_host_object(Some(&signal_proto), TAG_SLAB, signal_key)
                .map_err(JsThrow::from)?,
        );
        *data.wrapper.borrow_mut() = Some(signal_wrapper);
        let controller_key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::AbortController(Rc::clone(&data)));
        let controller_proto = self.interface_proto("AbortController")?;
        Ok(JsValue::Object(
            self.scope
                .new_host_object(Some(&controller_proto), TAG_SLAB, controller_key)
                .map_err(JsThrow::from)?,
        ))
    }

    pub(crate) fn new_plugin_array(&self) -> Result<JsValue, JsThrow> {
        self.new_indexed("PluginArray", HostData::PluginArray)
    }

    pub(crate) fn new_mime_type_array(&self) -> Result<JsValue, JsThrow> {
        self.new_indexed("MimeTypeArray", HostData::MimeTypeArray)
    }

    pub(crate) fn freeze(&self, value: &JsValue) -> Result<JsValue, JsThrow> {
        let freeze = self.with_js(|js| js.freeze.clone())?;
        self.call_helper(&freeze, std::slice::from_ref(value))
    }

    // === Phase 5 geometry unwraps / construction ===

    pub(crate) fn this_dom_rect(&self, value: &JsValue) -> Result<Rc<RefCell<RectData>>, JsThrow> {
        self.slab_data(value, "DOMRect", |data| match data {
            HostData::DomRect(r) => Some(Rc::clone(r)),
            _ => None,
        })
    }

    pub(crate) fn this_dom_rect_list(
        &self,
        value: &JsValue,
    ) -> Result<Rc<Vec<Rc<RefCell<RectData>>>>, JsThrow> {
        self.slab_data(value, "DOMRectList", |data| match data {
            HostData::DomRectList(l) => Some(Rc::clone(l)),
            _ => None,
        })
    }

    /// Creates a `DOMRect`/`DOMRectReadOnly` host object with the given
    /// interface prototype (a fresh wrapper; no identity caching).
    pub(crate) fn new_dom_rect(&self, interface: &str, data: RectData) -> Result<JsValue, JsThrow> {
        self.new_slab_object(interface, HostData::DomRect(Rc::new(RefCell::new(data))))
    }

    /// Creates a `DOMRectList` host object wrapped in the indexing proxy
    /// (backs `Element.getClientRects`).
    pub(crate) fn new_dom_rect_list(
        &self,
        rects: Vec<Rc<RefCell<RectData>>>,
    ) -> Result<JsValue, JsThrow> {
        self.new_indexed("DOMRectList", HostData::DomRectList(Rc::new(rects)))
    }

    // === Phase 3 network object construction ===

    /// Creates a slab-backed host object for one of the network interfaces.
    pub(crate) fn new_net_object(
        &self,
        interface: &str,
        data: HostData,
    ) -> Result<JsValue, JsThrow> {
        self.new_slab_object(interface, data)
    }

    /// The realm's global object.
    pub(crate) fn with_global(&self) -> Result<JsObject, JsThrow> {
        self.with_js(|js| js.global.clone())
    }

    /// Installs URLSearchParams pair iteration on `proto`, backed by the
    /// native `snapshot` function.
    pub(crate) fn install_params_iterable(
        &self,
        proto: &JsObject,
        snapshot: JsValue,
    ) -> Result<(), JsThrow> {
        let install = self.with_js(|js| js.install_params_iterable.clone())?;
        self.call_helper(&install, &[JsValue::Object(proto.clone()), snapshot])?;
        Ok(())
    }

    /// Builds a deferred promise: returns `(promise, resolve, reject)`.
    pub(crate) fn make_promise(&self) -> Result<(JsValue, JsValue, JsValue), JsThrow> {
        let make = self.with_js(|js| js.make_promise.clone())?;
        let obj = self.call_helper(&make, &[])?;
        let JsValue::Object(obj) = &obj else {
            return Err(JsThrow::Type("makePromise did not return an object".into()));
        };
        let promise = self.scope.get(obj, "promise").map_err(JsThrow::from)?;
        let resolve = self.scope.get(obj, "resolve").map_err(JsThrow::from)?;
        let reject = self.scope.get(obj, "reject").map_err(JsThrow::from)?;
        Ok((promise, resolve, reject))
    }

    /// `Promise.resolve(value)`.
    pub(crate) fn resolved_promise(&self, value: JsValue) -> Result<JsValue, JsThrow> {
        let make = self.with_js(|js| js.resolved_promise.clone())?;
        self.call_helper(&make, &[value])
    }

    /// A promise already rejected with `value`.
    pub(crate) fn rejected_promise(&self, value: JsValue) -> Result<JsValue, JsThrow> {
        let (promise, _resolve, reject) = self.make_promise()?;
        self.scope
            .call(&reject, &JsValue::Undefined, std::slice::from_ref(&value))
            .map_err(JsThrow::from)?;
        Ok(promise)
    }

    /// Wraps a JS array of byte values into an `ArrayBuffer`
    /// (`new Uint8Array(bytes).buffer`) via the bootstrap helper, so the
    /// `Uint8Array` constructor is invoked with `new` on the JS side.
    pub(crate) fn bytes_to_array_buffer(&self, bytes: JsValue) -> Result<JsValue, JsThrow> {
        let helper = self.with_js(|js| js.bytes_to_array_buffer.clone())?;
        self.call_helper(&helper, &[bytes])
    }

    /// Builds a `TypeError` value (for rejecting a `fetch()` promise).
    pub(crate) fn type_error_value(&self, message: &str) -> JsValue {
        let safe: String = message
            .chars()
            .map(|c| match c {
                '"' => "\\\"".to_owned(),
                '\\' => "\\\\".to_owned(),
                '\n' | '\r' | '\t' => " ".to_owned(),
                c => c.to_string(),
            })
            .collect();
        self.scope
            .eval(
                &format!("new TypeError(\"{safe}\")"),
                "oxidepage:fetch-error",
            )
            .unwrap_or_else(|_| JsValue::String(message.to_owned()))
    }

    /// Normalizes a headers/params init value into `(name, value)` string
    /// pairs (record, array of pairs, or iterable).
    pub(crate) fn entries_of(&self, init: &JsValue) -> Result<Vec<(String, String)>, JsThrow> {
        let helper = self.with_js(|js| js.record_pairs.clone())?;
        let array = self.call_helper(&helper, std::slice::from_ref(init))?;
        let JsValue::Object(array) = &array else {
            return Ok(Vec::new());
        };
        let len = self.scope.array_length(array).map_err(JsThrow::from)?;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let pair = self.scope.array_get(array, i).map_err(JsThrow::from)?;
            if let JsValue::Object(pair) = &pair {
                let name = self.scope.array_get(pair, 0).map_err(JsThrow::from)?;
                let value = self.scope.array_get(pair, 1).map_err(JsThrow::from)?;
                out.push((
                    self.scope.coerce_string(&name).map_err(JsThrow::from)?,
                    self.scope.coerce_string(&value).map_err(JsThrow::from)?,
                ));
            }
        }
        Ok(out)
    }

    // === WebIDL argument conversion ===

    pub fn arg_dom_string(&self, call: &HostCall, i: usize) -> Result<String, JsThrow> {
        self.scope
            .coerce_string(&call.arg(i))
            .map_err(JsThrow::from)
    }

    pub fn arg_nullable_dom_string(
        &self,
        call: &HostCall,
        i: usize,
    ) -> Result<Option<String>, JsThrow> {
        let value = call.arg(i);
        if value.is_nullish() {
            return Ok(None);
        }
        self.scope
            .coerce_string(&value)
            .map(Some)
            .map_err(JsThrow::from)
    }

    pub fn arg_dom_string_or(
        &self,
        call: &HostCall,
        i: usize,
        default: &str,
    ) -> Result<String, JsThrow> {
        match call.arg(i) {
            JsValue::Undefined => Ok(default.to_owned()),
            value => self.scope.coerce_string(&value).map_err(JsThrow::from),
        }
    }

    pub fn arg_opt_dom_string(&self, call: &HostCall, i: usize) -> Result<Option<String>, JsThrow> {
        match call.arg(i) {
            JsValue::Undefined => Ok(None),
            value => self
                .scope
                .coerce_string(&value)
                .map(Some)
                .map_err(JsThrow::from),
        }
    }

    pub fn arg_bool(&self, call: &HostCall, i: usize) -> bool {
        call.arg(i).truthy()
    }

    pub fn arg_bool_or(&self, call: &HostCall, i: usize, default: bool) -> bool {
        match call.arg(i) {
            JsValue::Undefined => default,
            value => value.truthy(),
        }
    }

    pub fn arg_opt_bool(&self, call: &HostCall, i: usize) -> Option<bool> {
        match call.arg(i) {
            JsValue::Undefined => None,
            value => Some(value.truthy()),
        }
    }

    fn to_integer(&self, call: &HostCall, i: usize, modulus: f64) -> Result<f64, JsThrow> {
        let n = self
            .scope
            .coerce_number(&call.arg(i))
            .map_err(JsThrow::from)?;
        if !n.is_finite() {
            return Ok(0.0);
        }
        let n = n.trunc();
        Ok(n.rem_euclid(modulus))
    }

    pub fn arg_u32(&self, call: &HostCall, i: usize) -> Result<u32, JsThrow> {
        Ok(self.to_integer(call, i, 4_294_967_296.0)? as u32)
    }

    /// WebIDL `long`: ECMAScript ToInt32 — wrap modulo 2^32, then reinterpret
    /// the low 32 bits as signed. `select.selectedIndex = -1` is the reason
    /// this exists, and it must survive the round trip.
    pub fn arg_i32(&self, call: &HostCall, i: usize) -> Result<i32, JsThrow> {
        Ok(self.to_integer(call, i, 4_294_967_296.0)? as u32 as i32)
    }

    pub fn arg_u16(&self, call: &HostCall, i: usize) -> Result<u16, JsThrow> {
        Ok(self.to_integer(call, i, 65_536.0)? as u16)
    }

    pub fn arg_u32_or(&self, call: &HostCall, i: usize, default: u32) -> Result<u32, JsThrow> {
        match call.arg(i) {
            JsValue::Undefined => Ok(default),
            _ => self.arg_u32(call, i),
        }
    }

    pub fn arg_f64(&self, call: &HostCall, i: usize) -> Result<f64, JsThrow> {
        self.scope
            .coerce_number(&call.arg(i))
            .map_err(JsThrow::from)
    }

    pub fn arg_f64_or(&self, call: &HostCall, i: usize, default: f64) -> Result<f64, JsThrow> {
        match call.arg(i) {
            JsValue::Undefined => Ok(default),
            _ => self.arg_f64(call, i),
        }
    }

    /// An `Element`-typed argument. A node of any other kind is a `TypeError`,
    /// not a DOMException — `insertAdjacentElement(pos, doctype)` must throw
    /// before it ever reaches the hierarchy check.
    pub fn arg_element(&self, call: &HostCall, i: usize) -> Result<NodeId, JsThrow> {
        let node = self.arg_node(call, i)?;
        if self.state.dom.borrow().node(node).data().kind() == NodeKind::Element {
            Ok(node)
        } else {
            Err(JsThrow::Type(format!(
                "parameter {} is not of type Element",
                i + 1
            )))
        }
    }

    pub fn arg_node(&self, call: &HostCall, i: usize) -> Result<NodeId, JsThrow> {
        let value = call.arg(i);
        match self.payload(&value) {
            Some((TAG_NODE, _)) => self.this_node(&value),
            _ => Err(JsThrow::Type(format!(
                "parameter {} is not of type Node",
                i + 1
            ))),
        }
    }

    pub fn arg_nullable_node(&self, call: &HostCall, i: usize) -> Result<Option<NodeId>, JsThrow> {
        let value = call.arg(i);
        if value.is_nullish() {
            return Ok(None);
        }
        self.arg_node(call, i).map(Some)
    }

    /// Brand-checks an `Event` argument, returning the wrapper value itself.
    pub fn arg_event_value(&self, call: &HostCall, i: usize) -> Result<JsValue, JsThrow> {
        let value = call.arg(i);
        self.this_event(&value)?;
        Ok(value)
    }

    pub fn arg_rest_dom_strings(
        &self,
        call: &HostCall,
        start: usize,
    ) -> Result<Vec<String>, JsThrow> {
        call.args
            .get(start..)
            .unwrap_or_default()
            .iter()
            .map(|v| self.scope.coerce_string(v).map_err(JsThrow::from))
            .collect()
    }

    // === Host-object construction ===

    /// Creates an event host object with the given interface prototype.
    pub(crate) fn new_event_object(
        &self,
        interface: &str,
        data: EventData,
    ) -> Result<(JsValue, Rc<std::cell::RefCell<EventData>>), JsThrow> {
        let data = Rc::new(std::cell::RefCell::new(data));
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::Event(Rc::clone(&data)));
        let proto = self.interface_proto(interface)?;
        let wrapper = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        Ok((JsValue::Object(wrapper), data))
    }

    /// Creates a collection host object wrapped in the indexing proxy.
    pub(crate) fn new_collection(
        &self,
        interface: &str,
        data: CollectionData,
    ) -> Result<JsValue, JsThrow> {
        let named = matches!(
            data,
            CollectionData::Children(_)
                | CollectionData::ByTagName { .. }
                | CollectionData::ByTagNameNS { .. }
                | CollectionData::ByClassName { .. }
        );
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::Collection(data));
        let proto = self.interface_proto(interface)?;
        let target = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        let proxy_fn = self.with_js(|js| js.collection_proxy.clone())?;
        self.call_helper(&proxy_fn, &[JsValue::Object(target), JsValue::Bool(named)])
    }

    /// Creates a `MutationRecord` host object.
    pub(crate) fn new_mutation_record(&self, view: RecordView) -> Result<JsValue, JsThrow> {
        let key = self
            .state
            .slab
            .borrow_mut()
            .insert(HostData::MutationRecord(Rc::new(view)));
        let proto = self.interface_proto("MutationRecord")?;
        let wrapper = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        Ok(JsValue::Object(wrapper))
    }

    /// Returns the `[SameObject]` wrapper for `(node, member)`, creating it
    /// with `make` on first access.
    pub(crate) fn same_object(
        &self,
        node: NodeId,
        member: &'static str,
        make: impl FnOnce(&Self) -> Result<JsValue, JsThrow>,
    ) -> Result<JsValue, JsThrow> {
        let key = (node.index(), node.generation().get(), member);
        if let Some(cached) = self.state.same_object.borrow().get(&key) {
            return Ok(cached.clone());
        }
        let value = make(self)?;
        self.state
            .same_object
            .borrow_mut()
            .insert(key, value.clone());
        Ok(value)
    }

    /// The `[SameObject]` value cached for `node`'s `member`, without minting one.
    fn same_object_peek(&self, node: NodeId, member: &'static str) -> Option<JsValue> {
        let key = (node.index(), node.generation().get(), member);
        self.state.same_object.borrow().get(&key).cloned()
    }

    /// Drops the per-index wrapper cache of `owner`'s `cssRules` list.
    ///
    /// `CSSRuleList::item` caches one wrapper per index, and the list itself is
    /// `[SameObject]`, so after `insertRule`/`deleteRule` shifts the underlying
    /// rules those wrappers would keep answering for the rules that *used* to sit
    /// at those indices.
    pub(crate) fn invalidate_css_rule_list(&self, owner: NodeId) {
        let Some(list) = self.same_object_peek(owner, CSS_RULES_MEMBER) else {
            return;
        };
        if let Ok(data) = self.this_css_rule_list(&list) {
            data.items.borrow_mut().clear();
        }
    }

    // === Interface registration (called by generated code) ===

    pub(crate) fn begin_interface(
        &self,
        name: &str,
        parent: Option<&str>,
    ) -> Result<JsObject, JsThrow> {
        self.state.pending_consts.borrow_mut().clear();
        // WebIDL: an interface prototype object whose interface has no
        // inherited interface has `%Object.prototype%` as its prototype — not
        // `null`, which would cut `Object.prototype`'s methods out of the
        // chain of every root interface (`window.hasOwnProperty`, …).
        let parent_proto = match parent {
            Some(p) => self.interface_proto(p)?,
            None => self.with_js(|js| js.object_prototype.clone())?,
        };
        let proto = self
            .scope
            .new_object_with_proto(Some(&parent_proto))
            .map_err(JsThrow::from)?;
        self.set_to_string_tag(&proto, name)?;
        Ok(proto)
    }

    pub(crate) fn set_to_string_tag(&self, object: &JsObject, name: &str) -> Result<(), JsThrow> {
        let tag_fn = self.with_js(|js| js.set_to_string_tag.clone())?;
        self.call_helper(
            &tag_fn,
            &[
                JsValue::Object(object.clone()),
                JsValue::String(name.to_owned()),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn define_method(
        &self,
        proto: &JsObject,
        name: &str,
        length: u32,
        f: NativeFn,
    ) -> Result<(), JsThrow> {
        self.define_method_with(proto, name, length, native(f))
    }

    /// [`Self::define_method`] for a `[CEReactions]` operation — see [`native_ce`].
    pub(crate) fn define_method_ce(
        &self,
        proto: &JsObject,
        name: &str,
        length: u32,
        f: NativeFn,
    ) -> Result<(), JsThrow> {
        self.define_method_with(proto, name, length, native_ce(f))
    }

    fn define_method_with(
        &self,
        proto: &JsObject,
        name: &str,
        length: u32,
        host: HostFn,
    ) -> Result<(), JsThrow> {
        let func = self
            .scope
            .new_function(name, length, host)
            .map_err(JsThrow::from)?;
        self.scope
            .define_property(
                proto,
                name,
                PropertyDef::Value {
                    value: &JsValue::Object(func),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            )
            .map_err(JsThrow::from)
    }

    pub(crate) fn define_getter(
        &self,
        proto: &JsObject,
        name: &str,
        getter: NativeFn,
    ) -> Result<(), JsThrow> {
        let get = self
            .scope
            .new_function(&format!("get {name}"), 0, native(getter))
            .map_err(JsThrow::from)?;
        self.scope
            .define_property(
                proto,
                name,
                PropertyDef::Accessor {
                    getter: Some(&JsValue::Object(get)),
                    setter: None,
                    enumerable: true,
                    configurable: true,
                },
            )
            .map_err(JsThrow::from)
    }

    pub(crate) fn define_accessor(
        &self,
        proto: &JsObject,
        name: &str,
        getter: NativeFn,
        setter: NativeFn,
    ) -> Result<(), JsThrow> {
        self.define_accessor_with(proto, name, native(getter), native(setter))
    }

    /// [`Self::define_accessor`] for a `[CEReactions]` attribute. Only the
    /// setter is scoped — a getter enqueues no reactions.
    pub(crate) fn define_accessor_ce(
        &self,
        proto: &JsObject,
        name: &str,
        getter: NativeFn,
        setter: NativeFn,
    ) -> Result<(), JsThrow> {
        self.define_accessor_with(proto, name, native(getter), native_ce(setter))
    }

    fn define_accessor_with(
        &self,
        proto: &JsObject,
        name: &str,
        getter: HostFn,
        setter: HostFn,
    ) -> Result<(), JsThrow> {
        let get = self
            .scope
            .new_function(&format!("get {name}"), 0, getter)
            .map_err(JsThrow::from)?;
        let set = self
            .scope
            .new_function(&format!("set {name}"), 1, setter)
            .map_err(JsThrow::from)?;
        self.scope
            .define_property(
                proto,
                name,
                PropertyDef::Accessor {
                    getter: Some(&JsValue::Object(get)),
                    setter: Some(&JsValue::Object(set)),
                    enumerable: true,
                    configurable: true,
                },
            )
            .map_err(JsThrow::from)
    }

    pub(crate) fn define_constant(
        &self,
        proto: &JsObject,
        name: &str,
        value: f64,
    ) -> Result<(), JsThrow> {
        self.state
            .pending_consts
            .borrow_mut()
            .push((name.to_owned(), value));
        self.scope
            .define_property(
                proto,
                name,
                PropertyDef::Value {
                    value: &JsValue::Number(value),
                    writable: false,
                    enumerable: true,
                    configurable: false,
                },
            )
            .map_err(JsThrow::from)
    }

    pub(crate) fn install_iterable(&self, proto: &JsObject) -> Result<(), JsThrow> {
        let install = self.with_js(|js| js.install_iterable.clone())?;
        self.call_helper(&install, &[JsValue::Object(proto.clone())])?;
        Ok(())
    }

    /// Installs @@iterator = %Array.prototype.values% only (WebIDL rule for
    /// indexed-getter interfaces without `iterable<>`).
    pub(crate) fn install_value_iterator(&self, proto: &JsObject) -> Result<(), JsThrow> {
        let install = self.with_js(|js| js.install_value_iterator.clone())?;
        self.call_helper(&install, &[JsValue::Object(proto.clone())])?;
        Ok(())
    }

    /// Mints the ObservableArray stand-in for `adoptedStyleSheets`: a Proxy
    /// over a plain array whose in-place mutations re-sync the style engine
    /// for `owner` (the ShadowRoot/Document wrapper). `initial` seeds the
    /// backing array without firing the sync.
    pub(crate) fn new_adopted_sheets_array(
        &self,
        owner: &JsValue,
        initial: Option<&JsValue>,
    ) -> Result<JsValue, JsThrow> {
        let helper = self.with_js(|js| js.adopted_sheets_proxy.clone())?;
        let sync = self
            .scope
            .new_function(
                "syncAdoptedStyleSheets",
                2,
                native(crate::imp::shadow_root::adopted_sheets_sync),
            )
            .map_err(JsThrow::from)?;
        let initial = initial.cloned().unwrap_or(JsValue::Undefined);
        self.call_helper(&helper, &[owner.clone(), JsValue::Object(sync), initial])
    }

    pub(crate) fn finish_interface(
        &self,
        name: &str,
        proto: &JsObject,
        ctor: CtorSpec,
    ) -> Result<(), JsThrow> {
        let (length, host_fn): (u32, HostFn) = match ctor {
            CtorSpec::Illegal => {
                let name = name.to_owned();
                (
                    0,
                    Rc::new(move |_scope, _call| -> Result<JsValue, JsThrow> {
                        Err(JsThrow::Type(format!("Illegal constructor: {name}")))
                    }),
                )
            }
            CtorSpec::Native { length, construct } => {
                let inner = native(construct);
                (
                    length,
                    Rc::new(move |scope, call| {
                        // Constructors must be invoked with `new`. The QuickJS
                        // trampoline passes new.target as `this`, so a function
                        // `this` is our proxy for "constructed". rquickjs 0.12
                        // exposes no separate new.target, so `Event.call(fn, …)`
                        // (where `fn` is any function) slips past this check and
                        // is treated as construction. That is a spec deviation
                        // only: the resulting object still fails interface brand
                        // checks, so it cannot be passed off as a real instance.
                        if !scope.is_function(&call.this) {
                            return Err(JsThrow::Type("Constructor requires 'new'".into()));
                        }
                        inner(scope, call)
                    }),
                )
            }
        };
        let ctor_obj = self
            .scope
            .new_constructor(name, length, proto, host_fn)
            .map_err(JsThrow::from)?;
        // Constants live on the constructor as well as the prototype.
        for (const_name, value) in self.state.pending_consts.borrow().iter() {
            self.scope
                .define_property(
                    &ctor_obj,
                    const_name,
                    PropertyDef::Value {
                        value: &JsValue::Number(*value),
                        writable: false,
                        enumerable: true,
                        configurable: false,
                    },
                )
                .map_err(JsThrow::from)?;
        }
        self.state.pending_consts.borrow_mut().clear();
        // Expose the interface object on the global.
        let global = self.with_js(|js| js.global.clone())?;
        self.scope
            .define_property(
                &global,
                name,
                PropertyDef::Value {
                    value: &JsValue::Object(ctor_obj.clone()),
                    writable: true,
                    enumerable: false,
                    configurable: true,
                },
            )
            .map_err(JsThrow::from)?;
        self.state.interfaces.borrow_mut().insert(
            name.to_owned(),
            InterfaceEntry {
                proto: proto.clone(),
                ctor: ctor_obj,
            },
        );
        Ok(())
    }

    /// Milliseconds since the page's time origin.
    pub fn now_ms(&self) -> f64 {
        self.state.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Creates a slab-backed host object with the given interface prototype.
    pub(crate) fn new_slab_object(
        &self,
        interface: &str,
        data: HostData,
    ) -> Result<JsValue, JsThrow> {
        let key = self.state.slab.borrow_mut().insert(data);
        let proto = self.interface_proto(interface)?;
        let wrapper = self
            .scope
            .new_host_object(Some(&proto), TAG_SLAB, key)
            .map_err(JsThrow::from)?;
        Ok(JsValue::Object(wrapper))
    }

    /// Frees detached, unpinned trees left behind by a mutation, unless the
    /// parser is active or mutation records may still reference them.
    pub(crate) fn free_detached(&self, nodes: &[NodeId]) {
        if self.state.parsing.get() {
            return;
        }
        let mut dom = self.state.dom.borrow_mut();
        if dom.observers().has_pending_records() {
            return;
        }
        for &node in nodes {
            dom.free_detached_tree_if_unpinned(node);
        }
    }

    // === Error reporting ===

    /// Reports a callback exception without unwinding (spec "report the
    /// exception").
    pub(crate) fn report_callback_error(&self, error: oxidepage_js::JsError) {
        self.state.hooks.report_error(error.to_string());
    }
}
