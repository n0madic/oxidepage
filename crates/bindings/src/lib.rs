//! JS bindings: DOM ↔ JavaScript glue (design doc §5.3).
//!
//! - [`install`] wires a realm: bootstrap script (DOMException, wrapper
//!   cache, collection proxies), generated interface registration
//!   ([`generated`]), and the global scope (window/document/console/timers).
//! - [`microtask_checkpoint`] drains promise jobs and delivers
//!   `MutationObserver` records; the page event loop calls it after every
//!   task and every callback into JS.
//! - [`process_finalized`] consumes GC'd wrapper notifications, maintaining
//!   the pin contract: unpinned detached trees are freed.

mod collections;
pub mod console;
mod cssdata;
mod customreg;
pub mod cx;
pub mod dialog;
pub mod events;
mod generated;
mod handlers;
pub mod imp;
mod netdata;
pub mod preview;
mod script;
pub mod state;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxidepage_js::{HostCall, JsError, JsObject, JsRealm, JsScope, JsThrow, JsValue, PropertyDef};
use oxidepage_net::{Credentials, NetEvent, NetRequest, RequestMode, ResponseType};
use oxidepage_style::Viewport;

pub use console::{ConsoleLevel, ConsoleMessage, ScriptError, ScriptErrorKind};
pub use cx::BindCx;
pub use dialog::{DialogEvent, DialogHandler, DialogKind, DialogRequest, DialogResponse};
pub use events::{EventTargetKey, Modifiers, dispatch_event, fire_pop_state, fire_simple_event};
pub use imp::input_synth::{
    KeyEventKind, KeyInput, MouseEventKind, MouseInput, WheelInput,
    dispatch_key as imp_dispatch_key, dispatch_mouse as imp_dispatch_mouse,
    dispatch_wheel as imp_dispatch_wheel, insert_text as imp_insert_text,
};
pub use preview::{
    PREVIEW_MAX_DEPTH, PREVIEW_MAX_ENTRIES, PREVIEW_MAX_NODES, PREVIEW_MAX_STRING, ValuePreview,
    format_message, render as render_preview, render_top as render_preview_top,
};
pub use script::is_classic_script_type;
pub use state::{
    HostHooks, MAX_HISTORY_ENTRIES, NavigationBody, NavigatorData, PageState, PendingNavigation,
    ReadyState, ScreenData, SessionHistory, TimingMilestone,
};

use netdata::{HeadersData, PendingNet, PendingResponse, ResponseData};
use state::{AbortSignalData, HostData, JsRefs, TAG_NODE, TAG_SLAB};

const BOOTSTRAP_JS: &str = include_str!("bootstrap.js");

fn engine_error(throw: JsThrow) -> JsError {
    match throw {
        JsThrow::Type(m) | JsThrow::Range(m) => JsError::Engine(m),
        JsThrow::Value(_) => JsError::Engine("exception during bindings install".into()),
    }
}

/// Installs the full bindings surface into `realm` over `dom`.
///
/// The returned [`PageState`] is also installed as the realm's host state;
/// the embedder must drop its own strong reference before the realm.
pub fn install<R: JsRealm>(
    realm: &R,
    dom: Rc<std::cell::RefCell<oxidepage_dom::DomTree>>,
    hooks: Rc<dyn HostHooks>,
    viewport: Viewport,
) -> Result<Rc<PageState>, JsError> {
    install_with_profiles(
        realm,
        dom,
        hooks,
        viewport,
        NavigatorData::default(),
        ScreenData::from_viewport(viewport),
    )
}

/// Installs bindings with an embedder-provided immutable Navigator profile.
pub fn install_with_navigator<R: JsRealm>(
    realm: &R,
    dom: Rc<std::cell::RefCell<oxidepage_dom::DomTree>>,
    hooks: Rc<dyn HostHooks>,
    viewport: Viewport,
    navigator: NavigatorData,
) -> Result<Rc<PageState>, JsError> {
    install_with_profiles(
        realm,
        dom,
        hooks,
        viewport,
        navigator,
        ScreenData::from_viewport(viewport),
    )
}

/// Installs bindings with embedder-provided immutable Navigator and Screen profiles.
pub fn install_with_profiles<R: JsRealm>(
    realm: &R,
    dom: Rc<std::cell::RefCell<oxidepage_dom::DomTree>>,
    hooks: Rc<dyn HostHooks>,
    viewport: Viewport,
    navigator: NavigatorData,
    screen: ScreenData,
) -> Result<Rc<PageState>, JsError> {
    let state = Rc::new(PageState::new(dom, hooks, viewport, navigator, screen));
    realm.set_state(Rc::clone(&state) as Rc<dyn std::any::Any>);
    realm.with_scope(|scope| -> Result<(), JsError> {
        install_bootstrap(scope, &state)?;
        let cx = BindCx {
            scope,
            state: Rc::clone(&state),
        };
        generated::register_interfaces(&cx).map_err(engine_error)?;
        install_value_iterators(&cx).map_err(engine_error)?;
        install_window(&cx).map_err(engine_error)?;
        // Native helper functions the late globals build on (randomness for
        // `crypto`, same-origin URL updates for `history`). Installed before
        // `installLateGlobals`, which captures and deletes them.
        install_native_helpers(&cx).map_err(engine_error)?;
        // Globals that need the generated classes and other globals in place
        // (`AbortSignal.abort`/`timeout`, `performance` entries).
        install_late_globals(&cx).map_err(engine_error)?;
        Ok(())
    })?;
    Ok(state)
}

fn install_bootstrap(scope: &dyn JsScope, state: &Rc<PageState>) -> Result<(), JsError> {
    let helpers = scope.eval(BOOTSTRAP_JS, "oxidepage:bootstrap.js")?;
    let JsValue::Object(helpers) = helpers else {
        return Err(JsError::Engine(
            "bootstrap script did not return an object".into(),
        ));
    };
    let get = |name: &str| scope.get(&helpers, name);
    let new_map = get("newWrapperMap")?;
    let wrapper_map = scope.call(&new_map, &JsValue::Undefined, &[])?;

    // Seed the `styleProxy` property map: every supported CSS property name in
    // both its dashed and camelCase (IDL-attribute) forms, mapped to the dashed
    // name `getPropertyValue`/`setProperty` expect. Built once, shared by all
    // `CSSStyleDeclaration` proxies.
    {
        let init = get("initStyleProps")?;
        let mut pairs: Vec<JsValue> = Vec::new();
        for css in oxidepage_style::supported_property_names() {
            // Dashed (the property name itself), camel-cased, and — for
            // `-webkit-` properties — the extra lowercase-first webkit form.
            let mut keys = vec![css.to_owned(), oxidepage_style::css_to_idl_attribute(css)];
            keys.extend(oxidepage_style::webkit_idl_attribute(css));
            for key in keys {
                let pair =
                    scope.new_array(&[JsValue::String(key), JsValue::String(css.to_owned())])?;
                pairs.push(JsValue::Object(pair));
            }
        }
        let pairs = scope.new_array(&pairs)?;
        scope.call(&init, &JsValue::Undefined, &[JsValue::Object(pairs)])?;
    }

    let JsValue::Object(object_prototype) = get("objectPrototype")? else {
        return Err(JsError::Engine(
            "bootstrap did not expose Object.prototype".into(),
        ));
    };

    *state.js.borrow_mut() = Some(JsRefs {
        global: scope.global(),
        wrapper_map,
        cache_get: get("cacheGet")?,
        cache_set: get("cacheSet")?,
        collection_proxy: get("collectionProxy")?,
        install_iterable: get("installIterable")?,
        install_value_iterator: get("installValueIterator")?,
        adopted_sheets_proxy: get("adoptedSheetsProxy")?,
        set_to_string_tag: get("setToStringTag")?,
        make_dom_exception: get("makeDomException")?,
        structured_clone: get("structuredClone")?,
        make_promise: get("makePromise")?,
        resolved_promise: get("resolvedPromise")?,
        record_pairs: get("recordPairs")?,
        install_params_iterable: get("installParamsIterable")?,
        freeze: get("freeze")?,
        style_proxy: get("styleProxy")?,
        dataset_proxy: get("datasetProxy")?,
        delete_property: get("deleteProperty")?,
        object_prototype,
        ce_construct: get("ceConstruct")?,
        install_late_globals: get("installLateGlobals")?,
        enqueue_microtask: get("enqueueMicrotask")?,
        mutation_notify: JsValue::Object(scope.new_function(
            "notifyMutationObservers",
            0,
            cx::native(mutation_notify_glue),
        )?),
    });
    Ok(())
}

/// Global scope: window/self/document/location, EventTarget-ness of the
/// window, console, and timers.
/// WebIDL: an interface with an indexed property getter but no `iterable<>`
/// declaration still exposes `@@iterator` = %Array.prototype.values% (and
/// nothing else). Sites spread these (`[...element.attributes]` in Swiper).
/// Runs the bootstrap's `installLateGlobals`, wiring globals that depend on the
/// generated interface classes or on other globals installed by `install_window`.
fn install_late_globals(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let helper = {
        let js = cx.state.js.borrow();
        js.as_ref()
            .map(|refs| refs.install_late_globals.clone())
            .ok_or_else(|| JsThrow::Type("bootstrap not installed".into()))?
    };
    cx.scope
        .call(&helper, &JsValue::Undefined, &[])
        .map_err(JsThrow::from)?;
    Ok(())
}

/// Installs the native helper functions the late globals (`installLateGlobals`)
/// build on: `__oxide_randomBytes` (entropy for `crypto`) and
/// `__oxide_setDocumentUrl` (same-origin URL replacement for `history`). The
/// bootstrap captures these into locals and deletes them from the global, so
/// page script never sees the `__oxide_*` surface.
fn install_native_helpers(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let global = {
        let js = cx.state.js.borrow();
        js.as_ref().expect("bootstrap installed").global.clone()
    };

    // `__oxide_randomBytes(n)` → `[byte, …]` of `n` uniformly random bytes.
    // `crypto.getRandomValues`/`randomUUID` copy these into typed arrays JS-side,
    // so this never needs native typed-array access. Capped at the 65536-byte
    // `getRandomValues` quota.
    define_fn(cx, &global, "__oxide_randomBytes", 1, |cx, call| {
        let n = call.arg(0).as_number().unwrap_or(0.0);
        let n = if n.is_finite() { n as i64 } else { 0 };
        let n = n.clamp(0, 65536) as usize;
        let mut buf = vec![0u8; n];
        getrandom::fill(&mut buf)
            .map_err(|e| JsThrow::Type(format!("random source unavailable: {e}")))?;
        let items: Vec<JsValue> = buf.into_iter().map(|b| JsValue::Number(b as f64)).collect();
        Ok(JsValue::Object(
            cx.scope.new_array(&items).map_err(JsThrow::from)?,
        ))
    })?;

    Ok(())
}

fn install_value_iterators(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    for interface in ["NamedNodeMap", "HTMLCollection"] {
        let proto = cx.interface_proto(interface)?;
        cx.install_value_iterator(&proto)?;
    }
    Ok(())
}

fn install_window(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let global = {
        let js = cx.state.js.borrow();
        js.as_ref().expect("bootstrap installed").global.clone()
    };
    let global_value = JsValue::Object(global.clone());

    // The realm global is the Window object. Window.prototype already chains
    // through EventTarget.prototype, so event methods resolve through the
    // standard interface hierarchy.
    {
        let interfaces = cx.state.interfaces.borrow();
        if let Some(entry) = interfaces.get("Window") {
            cx.scope
                .set_prototype(&global, Some(&entry.proto))
                .map_err(JsThrow::from)?;
        }
    }
    cx.set_to_string_tag(&global, "Window")?;

    cx.scope
        .set(&global, "window", &global_value)
        .map_err(JsThrow::from)?;
    cx.scope
        .set(&global, "self", &global_value)
        .map_err(JsThrow::from)?;
    for name in ["frames", "parent", "top"] {
        cx.scope
            .set(&global, name, &global_value)
            .map_err(JsThrow::from)?;
    }

    let navigator = cx.new_navigator()?;
    *cx.state.navigator_js.borrow_mut() = Some(navigator);
    for name in ["navigator", "clientInformation"] {
        cx.define_getter(&global, name, window_navigator)?;
    }

    let screen = cx.new_screen()?;
    *cx.state.screen_js.borrow_mut() = Some(screen);
    cx.define_getter(&global, "screen", window_screen)?;

    let performance = cx.new_performance()?;
    *cx.state.performance_js.borrow_mut() = Some(performance);
    cx.define_getter(&global, "performance", window_performance)?;

    let document = {
        let id = cx.state.dom.borrow().document();
        cx.node_to_js(id)?
    };
    cx.scope
        .define_property(
            &global,
            "document",
            PropertyDef::Value {
                value: &document,
                writable: false,
                enumerable: true,
                configurable: false,
            },
        )
        .map_err(JsThrow::from)?;

    // `document.cookie` (get/set through the page cookie jar; HttpOnly hidden).
    {
        let doc_proto = {
            let interfaces = cx.state.interfaces.borrow();
            interfaces.get("Document").map(|e| e.proto.clone())
        };
        if let Some(proto) = doc_proto {
            cx.define_accessor(&proto, "cookie", cookie_get, cookie_set)?;
        }
    }

    install_location(cx, &global)?;
    install_console(cx, &global)?;
    install_timers(cx, &global)?;
    install_fetch(cx, &global)?;
    install_url_statics(cx)?;
    install_dom_rect_statics(cx)?;
    install_pair_iteration(cx)?;
    install_cssom(cx, &global)?;
    install_viewport(cx, &global)?;
    install_named_properties(cx)?;
    install_custom_elements(cx, &global)?;
    Ok(())
}

/// Installs `window.customElements` — a single `CustomElementRegistry` brand
/// object (its state lives in `PageState::custom_elements`). Non-writable, like
/// `document`.
fn install_custom_elements(cx: &BindCx<'_>, global: &JsObject) -> Result<(), JsThrow> {
    let registry = cx.new_slab_object("CustomElementRegistry", HostData::CustomElementRegistry)?;
    cx.scope
        .define_property(
            global,
            "customElements",
            PropertyDef::Value {
                value: &registry,
                writable: false,
                enumerable: true,
                configurable: true,
            },
        )
        .map_err(JsThrow::from)?;
    Ok(())
}

/// Creates the HTML *named properties object* and splices it into the window's
/// prototype chain: `window` → `Window.prototype` → *named properties* →
/// `EventTarget.prototype`.
///
/// That position gives the spec's priority for free — an own property of the
/// window (`document`, `location`) shadows an element id that collides with it,
/// because own properties are found before the chain is walked — while
/// `Window.prototype instanceof EventTarget` still holds.
///
/// A plain object, deliberately: a `Proxy` here breaks bare-identifier
/// resolution. QuickJS resolves a global identifier with `[[Get]]` and never
/// consults a `has` trap on the chain, so an undeclared variable would silently
/// evaluate to `undefined` instead of throwing `ReferenceError`.
fn install_named_properties(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let event_target_proto = cx.interface_proto("EventTarget")?;
    let named_props = cx
        .scope
        .new_object_with_proto(Some(&event_target_proto))
        .map_err(JsThrow::from)?;

    let window_proto = cx.interface_proto("Window")?;
    cx.scope
        .set_prototype(&window_proto, Some(&named_props))
        .map_err(JsThrow::from)?;

    *cx.state.named_props.borrow_mut() = Some(named_props);
    // The document may already be parsed by the time bindings are installed.
    sync_named_properties(cx)
}

/// Brings the named properties object in step with the document's element ids.
///
/// Cheap to call: the tree's `id_version` is a `u64` cache key, so a document
/// whose ids did not change costs one comparison. Called at every host-call
/// boundary and on every entry into JS from the page's event loop, which
/// together cover both script-driven and parser-driven mutations.
pub fn sync_named_properties(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let version = cx.state.dom.borrow().id_version();
    if cx.state.named_props_version.get() == Some(version) {
        return Ok(());
    }
    let Some(named_props) = cx.state.named_props.borrow().clone() else {
        return Ok(());
    };

    let wanted: std::collections::HashSet<String> = cx
        .state
        .dom
        .borrow()
        .id_names()
        // `<div id="">` carries an id atom but names nothing.
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    let (added, removed) = {
        let live = cx.state.named_prop_keys.borrow();
        let added: Vec<String> = wanted.difference(&live).cloned().collect();
        let removed: Vec<String> = live.difference(&wanted).cloned().collect();
        (added, removed)
    };

    for name in added {
        define_named_property(cx, &named_props, &name)?;
    }
    for name in removed {
        cx.delete_property(&named_props, &name)?;
    }

    *cx.state.named_prop_keys.borrow_mut() = wanted;
    cx.state.named_props_version.set(Some(version));
    Ok(())
}

/// Defines one named-access accessor for element id `name`.
///
/// Non-enumerable, per WebIDL. The getter is lazy, so materializing a name does
/// not mint a wrapper — and therefore does not pin the element.
///
/// Spec deviation: when several elements share `name`, HTML hands back an
/// `HTMLCollection`. This returns the first element in tree order, matching
/// `getElementById`.
fn define_named_property(
    cx: &BindCx<'_>,
    named_props: &JsObject,
    name: &str,
) -> Result<(), JsThrow> {
    let id = name.to_owned();
    let getter = cx
        .scope
        .new_function(
            &format!("get {name}"),
            0,
            Rc::new(move |scope: &dyn JsScope, _call| {
                let cx = BindCx {
                    scope,
                    state: cx::page_state(scope)?,
                };
                let node = cx.state.dom.borrow().element_by_id(&id);
                match node {
                    Some(node) => cx.node_to_js(node),
                    None => Ok(JsValue::Undefined),
                }
            }),
        )
        .map_err(JsThrow::from)?;

    let key = name.to_owned();
    let setter = cx
        .scope
        .new_function(
            &format!("set {name}"),
            1,
            Rc::new(move |scope: &dyn JsScope, call: HostCall| {
                // `[[GetOwnProperty]]` on the spec's named properties object
                // yields a *data* descriptor, so `window.someId = 1` creates an
                // own property on the window that shadows the name. An accessor
                // in the prototype chain would otherwise swallow the write, so
                // the setter installs that own property itself.
                let JsValue::Object(receiver) = &call.this else {
                    return Ok(JsValue::Undefined);
                };
                scope.define_property(
                    receiver,
                    &key,
                    PropertyDef::Value {
                        value: &call.arg(0),
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    },
                )?;
                Ok(JsValue::Undefined)
            }),
        )
        .map_err(JsThrow::from)?;

    cx.scope
        .define_property(
            named_props,
            name,
            PropertyDef::Accessor {
                getter: Some(&JsValue::Object(getter)),
                setter: Some(&JsValue::Object(setter)),
                enumerable: false,
                configurable: true,
            },
        )
        .map_err(JsThrow::from)
}

fn window_navigator(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.state
        .navigator_js
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("Navigator is not installed".into()))
}

fn window_screen(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.state
        .screen_js
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("Screen is not installed".into()))
}

fn window_performance(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.state
        .performance_js
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("Performance is not installed".into()))
}

/// Installs the CSSOM-View window surface: viewport metrics, scroll
/// positions, and the `scroll`/`scrollTo`/`scrollBy` methods (WP-G2).
fn install_viewport(cx: &BindCx<'_>, global: &JsObject) -> Result<(), JsThrow> {
    cx.define_getter(global, "innerWidth", window_inner_width)?;
    cx.define_getter(global, "innerHeight", window_inner_height)?;
    cx.define_getter(global, "devicePixelRatio", window_device_pixel_ratio)?;
    cx.define_getter(global, "scrollX", window_scroll_x)?;
    cx.define_getter(global, "scrollY", window_scroll_y)?;
    cx.define_getter(global, "pageXOffset", window_scroll_x)?;
    cx.define_getter(global, "pageYOffset", window_scroll_y)?;
    define_fn(cx, global, "scroll", 2, window_scroll_to)?;
    define_fn(cx, global, "scrollTo", 2, window_scroll_to)?;
    define_fn(cx, global, "scrollBy", 2, window_scroll_by)?;
    Ok(())
}

fn window_inner_width(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Number(f64::from(
        cx.state.layout.borrow().viewport().width,
    )))
}

fn window_inner_height(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Number(f64::from(
        cx.state.layout.borrow().viewport().height,
    )))
}

fn window_device_pixel_ratio(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    Ok(JsValue::Number(f64::from(
        cx.state.layout.borrow().viewport().dpr,
    )))
}

fn window_scroll_x(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let x = imp::geometry_support::flush_layout(cx, |_, layout| layout.viewport_scroll().x);
    Ok(JsValue::Number(f64::from(x)))
}

fn window_scroll_y(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    let y = imp::geometry_support::flush_layout(cx, |_, layout| layout.viewport_scroll().y);
    Ok(JsValue::Number(f64::from(y)))
}

/// Reads the `(x, y)` target of `scroll()`/`scrollTo()`/`scrollBy()`,
/// following the spec's two-overload resolution by argument *count* (not by
/// the shape of the first argument): exactly two arguments is the `(x, y)`
/// numeric overload, each normalized (non-finite becomes 0); zero or one
/// argument is the `ScrollToOptions` overload, whose `left`/`top` — missing,
/// or the argument absent or not an object — default to `fallback`.
///
/// A lone non-object argument (e.g. `scroll(5)`) is spec'd to throw a
/// `TypeError` from failed overload resolution; that exception path is not
/// implemented, so it falls back like an empty options object instead of
/// being misread as an x with no y.
fn scroll_args(
    cx: &BindCx<'_>,
    call: &HostCall,
    fallback: (f32, f32),
) -> Result<(f32, f32), JsThrow> {
    let as_f32 = |v: &JsValue| -> Option<f32> {
        match v {
            JsValue::Number(n) if n.is_finite() => Some(*n as f32),
            _ => None,
        }
    };
    if call.args.len() >= 2 {
        let x = cx.arg_f64(call, 0).unwrap_or(0.0);
        let y = cx.arg_f64(call, 1).unwrap_or(0.0);
        return Ok((
            if x.is_finite() { x as f32 } else { 0.0 },
            if y.is_finite() { y as f32 } else { 0.0 },
        ));
    }
    if let JsValue::Object(options) = &call.arg(0) {
        let left = cx.scope.get(options, "left").ok();
        let top = cx.scope.get(options, "top").ok();
        Ok((
            left.as_ref().and_then(as_f32).unwrap_or(fallback.0),
            top.as_ref().and_then(as_f32).unwrap_or(fallback.1),
        ))
    } else {
        Ok(fallback)
    }
}

fn window_scroll_to(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let current = imp::geometry_support::flush_layout(cx, |_, layout| layout.viewport_scroll());
    let (x, y) = scroll_args(cx, call, (current.x, current.y))?;
    let changed =
        imp::geometry_support::flush_layout_mut(cx, |_, layout| layout.set_viewport_scroll(x, y))
            .changed;
    imp::geometry_support::note_scroll(cx, None, changed);
    Ok(JsValue::Undefined)
}

fn window_scroll_by(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let current = imp::geometry_support::flush_layout(cx, |_, layout| layout.viewport_scroll());
    // For scrollBy, options members default to a **delta** of zero.
    let (dx, dy) = scroll_args(cx, call, (0.0, 0.0))?;
    let changed = imp::geometry_support::flush_layout_mut(cx, |_, layout| {
        layout.set_viewport_scroll(current.x + dx, current.y + dy)
    })
    .changed;
    imp::geometry_support::note_scroll(cx, None, changed);
    Ok(JsValue::Undefined)
}

/// The `(x, y)` scroll offset behind `Element.scrollTop`/`scrollLeft` and the
/// method forms `scroll`/`scrollTo`/`scrollBy`: the viewport's scroll for the
/// document element (CSSOM-View's `documentElement` special case), else the
/// element's own scroll-container offset. `imp::element::scroll_top`/
/// `scroll_left` make the same split one axis at a time; this is the
/// two-axis-at-once version the method forms need.
fn element_scroll_offset(
    dom: &oxidepage_dom::DomTree,
    layout: &oxidepage_layout::LayoutEngine,
    this: oxidepage_base::NodeId,
) -> (f32, f32) {
    let p = if imp::geometry_support::is_document_element(dom, this) {
        layout.viewport_scroll()
    } else {
        layout.scroll_offset(this)
    };
    (p.x, p.y)
}

/// Sets both axes of `this`'s scroll offset in a single reflow, reusing the
/// same viewport/element split as [`element_scroll_offset`]. Returns the
/// `note_scroll` target (`None` for the viewport) and whether it changed.
fn set_element_scroll_offset(
    dom: &oxidepage_dom::DomTree,
    layout: &mut oxidepage_layout::LayoutEngine,
    this: oxidepage_base::NodeId,
    x: f32,
    y: f32,
) -> (Option<oxidepage_base::NodeId>, bool) {
    if imp::geometry_support::is_document_element(dom, this) {
        (None, layout.set_viewport_scroll(x, y).changed)
    } else {
        (Some(this), layout.set_scroll_offset(this, x, y).changed)
    }
}

/// `Element.scroll()`/`scrollTo()`: sets the element's scroll offsets
/// absolutely. Hand-registered like `Window.scrollTo` (see its comment
/// above `install_viewport`) since the two-form overload (numbers or a
/// `ScrollToOptions` dict) isn't codegen-expressible; `behavior` is read by
/// `scroll_args`' object-form branch but ignored (smooth scrolling is out of
/// scope, ADR-0006).
fn element_scroll_to(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let this = cx.this_element(&call.this)?;
    let current = imp::geometry_support::flush_layout(cx, |dom, layout| {
        element_scroll_offset(dom, layout, this)
    });
    let (x, y) = scroll_args(cx, call, current)?;
    let (target, changed) = imp::geometry_support::flush_layout_mut(cx, |dom, layout| {
        set_element_scroll_offset(dom, layout, this, x, y)
    });
    imp::geometry_support::note_scroll(cx, target, changed);
    Ok(JsValue::Undefined)
}

/// `Element.scrollBy()`: adds to the element's current scroll offsets.
fn element_scroll_by(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let this = cx.this_element(&call.this)?;
    let current = imp::geometry_support::flush_layout(cx, |dom, layout| {
        element_scroll_offset(dom, layout, this)
    });
    // For scrollBy, options members default to a **delta** of zero.
    let (dx, dy) = scroll_args(cx, call, (0.0, 0.0))?;
    let (target, changed) = imp::geometry_support::flush_layout_mut(cx, |dom, layout| {
        set_element_scroll_offset(dom, layout, this, current.0 + dx, current.1 + dy)
    });
    imp::geometry_support::note_scroll(cx, target, changed);
    Ok(JsValue::Undefined)
}

/// Installs the CSSOM entry points that the codegen cannot express: the
/// `Element.style` accessor (with its `PutForwards=cssText` string setter),
/// `Element.scroll`/`scrollTo`/`scrollBy` (two-form overloads, mirroring
/// `Window.scroll`/`scrollTo`/`scrollBy`), and the global `getComputedStyle`.
fn install_cssom(cx: &BindCx<'_>, global: &JsObject) -> Result<(), JsThrow> {
    let el_proto = {
        let interfaces = cx.state.interfaces.borrow();
        interfaces.get("Element").map(|e| e.proto.clone())
    };
    if let Some(proto) = el_proto {
        cx.define_accessor(&proto, "style", element_style_get, element_style_set)?;
        define_fn(cx, &proto, "scroll", 2, element_scroll_to)?;
        define_fn(cx, &proto, "scrollTo", 2, element_scroll_to)?;
        define_fn(cx, &proto, "scrollBy", 2, element_scroll_by)?;
    }
    define_fn(cx, global, "getComputedStyle", 1, get_computed_style)?;
    Ok(())
}

fn element_style_get(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let el = cx.this_element(&call.this)?;
    cx.same_object(el, "style", |cx| {
        cx.new_style_decl(cssdata::StyleDeclData::Inline {
            element: el,
            block: std::cell::RefCell::new(None),
        })
    })
}

/// `Element.style = "..."` forwards the string to `cssText` (PutForwards).
fn element_style_set(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let el = cx.this_element(&call.this)?;
    let value = cx.arg_dom_string(call, 0)?;
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(el, cssdata::style_attr_name(), value.into());
    Ok(JsValue::Undefined)
}

fn get_computed_style(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let arg = call.arg(0);
    let element = cx.this_element(&arg)?;
    // `pseudoElt` is `optional DOMString? = null`: both an absent argument and
    // an explicit `null` select the element's own style. Coercing `null` to the
    // string "null" would instead parse as an unsupported pseudo-element, and
    // `getComputedStyle(el, null)` is what several widely used libraries call.
    let pseudo_arg = match call.arg(1) {
        JsValue::Undefined | JsValue::Null => None,
        value => Some(cx.scope.coerce_string(&value).map_err(JsThrow::from)?),
    };
    let pseudo = match oxidepage_style::cssom::parse_pseudo(pseudo_arg.as_deref()) {
        Ok(pseudo) => pseudo,
        // An unsupported pseudo-element yields an empty declaration (v1: null).
        Err(_) => return Ok(JsValue::Null),
    };
    cx.new_style_decl(cssdata::StyleDeclData::Computed {
        element,
        pseudo,
        cache: std::cell::RefCell::new(None),
    })
}

/// Hand-registers the `iterable<name, value>` pair iteration that both
/// `URLSearchParams` and `FormData` declare (the generic `iterable<>` codegen
/// only supports single-value lists). Each is backed by its own native
/// `snapshot(obj)` → `[[name, value], …]`.
fn install_pair_iteration(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    for (interface, snapshot_fn) in [
        (
            "URLSearchParams",
            cx::native(imp::url_search_params::snapshot),
        ),
        ("FormData", cx::native(imp::form_data::snapshot)),
    ] {
        let proto = {
            let interfaces = cx.state.interfaces.borrow();
            interfaces.get(interface).map(|e| e.proto.clone())
        };
        let Some(proto) = proto else {
            continue;
        };
        let snapshot = cx
            .scope
            .new_function("snapshot", 1, snapshot_fn)
            .map_err(JsThrow::from)?;
        cx.install_params_iterable(&proto, JsValue::Object(snapshot))?;
    }
    Ok(())
}

fn cookie_get(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.this_document(&call.this)?;
    let url = cx.state.dom.borrow().document_url().to_owned();
    Ok(JsValue::String(cx.state.hooks.get_cookie(&url)))
}

fn cookie_set(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.this_document(&call.this)?;
    let value = cx.arg_dom_string(call, 0)?;
    let url = cx.state.dom.borrow().document_url().to_owned();
    cx.state.hooks.set_cookie(&url, &value);
    Ok(JsValue::Undefined)
}

/// Installs the global `fetch()` function.
fn install_fetch(cx: &BindCx<'_>, global: &JsObject) -> Result<(), JsThrow> {
    define_fn(cx, global, "fetch", 1, fetch_impl)
}

fn fetch_impl(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let input = call.arg(0);
    let init = call.arg(1);

    // A `Request` as input seeds the whole request; a URL string / `URL` seeds
    // only the URL and takes the rest from `init`. When both a `Request` and an
    // `init` are given, `init` overrides (v1: only the URL is inherited from the
    // request — an omitted `init` member falls back to the RequestInit default,
    // not the request's value).
    let (method, url, headers, body, credentials, mode, signal) = if let Ok(req) =
        cx.this_request(&input)
    {
        if init.is_nullish() {
            // v1: `Request.signal` is not yet plumbed, so an omitted `init`
            // carries no abort signal (documented follow-up).
            (
                req.method.clone(),
                req.url.clone(),
                req.headers.borrow().entries.clone(),
                req.body.clone(),
                request_credentials(&req.credentials),
                request_mode(&req.mode),
                None,
            )
        } else {
            let (method, headers, body, credentials, mode, signal) = parse_request_init(cx, &init)?;
            (
                method,
                req.url.clone(),
                headers,
                body,
                credentials,
                mode,
                signal,
            )
        }
    } else {
        let url = if let Ok(u) = cx.this_url(&input) {
            u.borrow().to_string()
        } else {
            cx.arg_dom_string(call, 0)?
        };
        let (method, headers, body, credentials, mode, signal) = parse_request_init(cx, &init)?;
        (method, url, headers, body, credentials, mode, signal)
    };

    let doc_url = cx.state.dom.borrow().document_url().to_owned();
    let absolute = match url::Url::parse(&url) {
        Ok(u) => u.to_string(),
        Err(_) => url::Url::parse(&doc_url)
            .and_then(|base| base.join(&url))
            .map(|u| u.to_string())
            .map_err(|_| JsThrow::Type(format!("fetch: invalid URL `{url}`")))?,
    };
    let initiator = url::Url::parse(&doc_url)
        .ok()
        .map(|u| u.origin().ascii_serialization());

    let request = NetRequest {
        method,
        url: absolute,
        headers,
        body,
        credentials,
        mode,
        referrer: Some(doc_url),
        initiator_origin: initiator,
        bypass_cache: false,
    };
    let (promise, resolve, reject) = cx.make_promise()?;
    // An already-aborted signal rejects synchronously; the request never
    // starts.
    if let Some(signal) = &signal
        && signal.aborted.get()
    {
        let reason = signal.reason.borrow().clone();
        let _ = cx
            .scope
            .call(&reject, &JsValue::Undefined, std::slice::from_ref(&reason));
        return Ok(promise);
    }
    let id = cx.state.hooks.start_fetch(request);
    cx.state.pending_net.borrow_mut().insert(
        id,
        PendingNet::Fetch {
            resolve,
            reject,
            response: PendingResponse::default(),
            signal: signal.clone(),
        },
    );
    // Register the in-flight id so a later `signal.abort()` can cancel it.
    if let Some(signal) = &signal {
        signal.pending_fetches.borrow_mut().push(id);
    }
    Ok(promise)
}

type RequestInit = (
    String,
    Vec<(String, String)>,
    Option<Vec<u8>>,
    Credentials,
    RequestMode,
    Option<Rc<AbortSignalData>>,
);

/// Removes a settled fetch's id from its `AbortSignal`'s pending list, keeping
/// the list bounded when one signal is reused across many fetches.
fn prune_signal_fetch(signal: &Option<Rc<AbortSignalData>>, id: oxidepage_base::RequestId) {
    if let Some(signal) = signal {
        signal.pending_fetches.borrow_mut().retain(|&i| i != id);
    }
}

/// Maps a serialized request-credentials mode back onto the net enum.
fn request_credentials(mode: &str) -> Credentials {
    match mode {
        "include" => Credentials::Include,
        "omit" => Credentials::Omit,
        _ => Credentials::SameOrigin,
    }
}

/// Maps a serialized request mode back onto the net enum.
fn request_mode(mode: &str) -> RequestMode {
    match mode {
        "no-cors" => RequestMode::NoCors,
        "same-origin" => RequestMode::SameOrigin,
        _ => RequestMode::Cors,
    }
}

fn parse_request_init(cx: &BindCx<'_>, init: &JsValue) -> Result<RequestInit, JsThrow> {
    let JsValue::Object(obj) = init else {
        return Ok((
            "GET".to_owned(),
            Vec::new(),
            None,
            Credentials::SameOrigin,
            RequestMode::Cors,
            None,
        ));
    };
    let method = cx
        .scope
        .get(obj, "method")
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_ascii_uppercase()))
        .unwrap_or_else(|| "GET".to_owned());
    let mut headers = {
        let h = cx.scope.get(obj, "headers").unwrap_or(JsValue::Undefined);
        if h.is_nullish() {
            Vec::new()
        } else if let Ok(hd) = cx.this_headers(&h) {
            hd.borrow().entries.clone()
        } else {
            // A plain object or record: run it through the same validation the
            // `Headers` constructor applies, so an invalid name/value is a
            // synchronous `TypeError` (per Fetch) rather than a rejected promise
            // from the net layer's own header check.
            let mut data = HeadersData::default();
            for (name, value) in cx.entries_of(&h)? {
                data.append(&name, &value)?;
            }
            data.entries
        }
    };
    let body = {
        let b = cx.scope.get(obj, "body").unwrap_or(JsValue::Undefined);
        if method == "GET" || method == "HEAD" {
            None
        } else {
            match crate::imp::body::extract(cx, &b)? {
                None => None,
                Some(extracted) => {
                    // The body's default `Content-Type` loses to one the caller
                    // supplied — but for `FormData` it is the only place the
                    // multipart boundary can come from.
                    if let Some(content_type) = extracted.content_type
                        && !headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    {
                        headers.push(("content-type".to_owned(), content_type));
                    }
                    Some(extracted.bytes)
                }
            }
        }
    };
    let credentials = match cx
        .scope
        .get(obj, "credentials")
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .as_deref()
    {
        Some("include") => Credentials::Include,
        Some("omit") => Credentials::Omit,
        _ => Credentials::SameOrigin,
    };
    let mode = match cx
        .scope
        .get(obj, "mode")
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .as_deref()
    {
        Some("no-cors") => RequestMode::NoCors,
        Some("same-origin") => RequestMode::SameOrigin,
        _ => RequestMode::Cors,
    };
    // `signal`: an `AbortSignal`, `null`/`undefined` (none), or a type error.
    let signal = {
        let s = cx.scope.get(obj, "signal").unwrap_or(JsValue::Undefined);
        if s.is_nullish() {
            None
        } else if let Ok(data) = cx.this_abort_signal(&s) {
            Some(data)
        } else {
            return Err(JsThrow::Type(
                "fetch: `signal` is not an AbortSignal".into(),
            ));
        }
    };
    Ok((method, headers, body, credentials, mode, signal))
}

/// Adds the static `URL.parse` / `URL.canParse` (the codegen registers the
/// `URL` constructor and instance members; statics are hand-registered).
fn install_url_statics(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let global = cx.with_global()?;
    let ctor = match cx.scope.get(&global, "URL") {
        Ok(JsValue::Object(o)) => o,
        _ => return Ok(()),
    };
    define_fn(cx, &ctor, "parse", 1, url_parse_static)?;
    define_fn(cx, &ctor, "canParse", 1, url_can_parse_static)?;
    Ok(())
}

fn parse_with_base(url: &str, base: Option<String>) -> Result<url::Url, url::ParseError> {
    match base {
        Some(base) => url::Url::parse(&base).and_then(|b| b.join(url)),
        None => url::Url::parse(url),
    }
}

fn url_parse_static(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let url = cx.arg_dom_string(call, 0)?;
    let base = cx.arg_opt_dom_string(call, 1)?;
    match parse_with_base(&url, base) {
        Ok(u) => cx.new_net_object(
            "URL",
            HostData::Url(Rc::new(crate::netdata::UrlData::new(u))),
        ),
        Err(_) => Ok(JsValue::Null),
    }
}

fn url_can_parse_static(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let url = cx.arg_dom_string(call, 0)?;
    let base = cx.arg_opt_dom_string(call, 1)?;
    Ok(JsValue::Bool(parse_with_base(&url, base).is_ok()))
}

/// Adds the static `DOMRect.fromRect` / `DOMRectReadOnly.fromRect` (the codegen
/// registers constructors and instance members, but not static ops).
fn install_dom_rect_statics(cx: &BindCx<'_>) -> Result<(), JsThrow> {
    let global = cx.with_global()?;
    for (iface, from_rect) in [
        ("DOMRect", dom_rect_from_rect_static as cx::NativeFn),
        ("DOMRectReadOnly", dom_rect_read_only_from_rect_static),
    ] {
        if let Ok(JsValue::Object(ctor)) = cx.scope.get(&global, iface) {
            define_fn(cx, &ctor, "fromRect", 0, from_rect)?;
        }
    }
    Ok(())
}

fn dom_rect_from_rect_static(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    imp::dom_rect::from_rect(cx, &call.arg(0), "DOMRect")
}

fn dom_rect_read_only_from_rect_static(
    cx: &BindCx<'_>,
    call: &HostCall,
) -> Result<JsValue, JsThrow> {
    imp::dom_rect::from_rect(cx, &call.arg(0), "DOMRectReadOnly")
}

fn define_fn(
    cx: &BindCx<'_>,
    target: &oxidepage_js::JsObject,
    name: &str,
    length: u32,
    f: cx::NativeFn,
) -> Result<(), JsThrow> {
    let func = cx
        .scope
        .new_function(name, length, cx::native(f))
        .map_err(JsThrow::from)?;
    cx.scope
        .set(target, name, &JsValue::Object(func))
        .map_err(JsThrow::from)
}

/// Installs `window.location` / `window.history`, and the `Document.location`
/// alias.
///
/// Both are real IDL interfaces (`imp::location`, `imp::history`); what is left
/// here is realm plumbing — minting the one wrapper each (the `navigator_js`
/// pattern, so object identity survives navigation) and defining the window
/// properties as **accessors**.
///
/// `location` needs its setter because `window.location = "/x"` is a common
/// idiom and means `location.assign("/x")`, not "replace the Location object".
fn install_location(cx: &BindCx<'_>, global: &oxidepage_js::JsObject) -> Result<(), JsThrow> {
    let location = cx.new_location()?;
    *cx.state.location_js.borrow_mut() = Some(location);
    cx.define_accessor(global, "location", window_location, window_set_location)?;

    let history = cx.new_history()?;
    *cx.state.history_js.borrow_mut() = Some(history);
    cx.define_getter(global, "history", window_history)?;

    // `document.location` returns the same Location object as `window.location`
    // — but only for the document that *has* a browsing context. A document from
    // `createDocument`/`createHTMLDocument`/`new Document()` reports `null`, and
    // `DOMImplementation-createDocument.html` asserts exactly that.
    let doc_proto = {
        let interfaces = cx.state.interfaces.borrow();
        interfaces.get("Document").map(|entry| entry.proto.clone())
    };
    if let Some(proto) = doc_proto {
        cx.define_getter(&proto, "location", |cx, call| {
            let this = cx.this_document(&call.this)?;
            if this != cx.state.dom.borrow().document() {
                return Ok(JsValue::Null);
            }
            window_location(cx, call)
        })?;
    }
    Ok(())
}

fn window_location(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.state
        .location_js
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("Location is not installed".into()))
}

/// `window.location = "/x"` — HTML's `[PutForwards=href]`, i.e. an
/// `assign()`, not a rebinding of the property.
fn window_set_location(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    let url = cx.arg_dom_string(call, 0)?;
    imp::location::assign(cx, 0, url)?;
    Ok(JsValue::Undefined)
}

fn window_history(cx: &BindCx<'_>, _call: &HostCall) -> Result<JsValue, JsThrow> {
    cx.state
        .history_js
        .borrow()
        .clone()
        .ok_or_else(|| JsThrow::Type("History is not installed".into()))
}

/// Snapshots every argument, renders the line, and records it.
///
/// `formatted` is false for the methods the console spec does not run its
/// Formatter over (`console.dir`), where a leading `"%s"` is data, not a
/// directive.
fn console_write(
    cx: &BindCx<'_>,
    args: &[JsValue],
    level: ConsoleLevel,
    formatted: bool,
) -> Result<JsValue, JsThrow> {
    let previews: Vec<ValuePreview> = args.iter().map(|arg| preview::encode(cx, arg)).collect();
    let message = if formatted {
        preview::format_message(&previews)
    } else {
        previews
            .iter()
            .map(preview::render)
            .collect::<Vec<_>>()
            .join(" ")
    };
    record_console(cx, level, message, previews);
    Ok(JsValue::Undefined)
}

/// Records one console line at the call site's location and group depth.
fn record_console(cx: &BindCx<'_>, level: ConsoleLevel, message: String, args: Vec<ValuePreview>) {
    // The innermost *script* frame: the console host function itself is a
    // native frame, which the capture already drops. Only one frame is kept,
    // so only one is parsed — this runs on every console call.
    let location = cx.scope.capture_location();
    cx.state.hooks.console_message(ConsoleMessage {
        level,
        message,
        args,
        location,
        group_depth: cx.state.console_group_depth.get(),
        timestamp: cx.state.epoch_now_ms(),
    });
}

fn install_console(cx: &BindCx<'_>, global: &oxidepage_js::JsObject) -> Result<(), JsThrow> {
    let console = cx.scope.new_object().map_err(JsThrow::from)?;
    define_fn(cx, &console, "log", 0, |cx, call| {
        console_write(cx, &call.args, ConsoleLevel::Log, true)
    })?;
    define_fn(cx, &console, "info", 0, |cx, call| {
        console_write(cx, &call.args, ConsoleLevel::Info, true)
    })?;
    define_fn(cx, &console, "warn", 0, |cx, call| {
        console_write(cx, &call.args, ConsoleLevel::Warn, true)
    })?;
    define_fn(cx, &console, "error", 0, |cx, call| {
        console_write(cx, &call.args, ConsoleLevel::Error, true)
    })?;
    define_fn(cx, &console, "debug", 0, |cx, call| {
        console_write(cx, &call.args, ConsoleLevel::Debug, true)
    })?;
    // `trace` is `log` at its own level: the stack every message now carries
    // is exactly what the method exists to show.
    define_fn(cx, &console, "trace", 0, |cx, call| {
        let args = if call.args.is_empty() {
            vec![JsValue::String("console.trace".to_owned())]
        } else {
            call.args.clone()
        };
        console_write(cx, &args, ConsoleLevel::Trace, true)
    })?;
    define_fn(cx, &console, "assert", 0, |cx, call| {
        if call.arg(0).truthy() {
            return Ok(JsValue::Undefined);
        }
        // Spec: "Assertion failed" alone, or with the rest appended after a
        // colon — and when the first of the rest is a format string, it is
        // *prefixed*, not passed through as an argument.
        let rest = call.args.get(1..).unwrap_or_default();
        let previews: Vec<ValuePreview> = rest.iter().map(|arg| preview::encode(cx, arg)).collect();
        let message = if previews.is_empty() {
            "Assertion failed".to_owned()
        } else {
            format!("Assertion failed: {}", preview::format_message(&previews))
        };
        record_console(cx, ConsoleLevel::Error, message, previews);
        Ok(JsValue::Undefined)
    })?;
    // `dir` shows the object's structure, so no format pass and no
    // string shortcut.
    define_fn(cx, &console, "dir", 0, |cx, call| {
        console_write(
            cx,
            &call.args[..call.args.len().min(1)],
            ConsoleLevel::Log,
            false,
        )
    })?;
    define_fn(cx, &console, "group", 0, console_group)?;
    define_fn(cx, &console, "groupCollapsed", 0, console_group)?;
    define_fn(cx, &console, "groupEnd", 0, |cx, _call| {
        let depth = cx.state.console_group_depth.get();
        cx.state.console_group_depth.set(depth.saturating_sub(1));
        Ok(JsValue::Undefined)
    })?;
    cx.scope
        .set(global, "console", &JsValue::Object(console))
        .map_err(JsThrow::from)
}

/// `console.group` / `groupCollapsed`: emit the label at the *outer* depth,
/// then indent. Collapsing is a devtools affordance with no headless meaning,
/// so the two methods are the same method.
fn console_group(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {
    if !call.args.is_empty() {
        console_write(cx, &call.args, ConsoleLevel::Log, true)?;
    }
    let depth = cx.state.console_group_depth.get();
    cx.state.console_group_depth.set(depth.saturating_add(1));
    Ok(JsValue::Undefined)
}

fn schedule_timer(cx: &BindCx<'_>, call: &HostCall, repeat: bool) -> Result<JsValue, JsThrow> {
    let callback = call.arg(0);
    let delay = match call.arg(1) {
        JsValue::Undefined => 0.0,
        value => {
            let n = cx.scope.coerce_number(&value).unwrap_or(0.0);
            if n.is_finite() { n.max(0.0) } else { 0.0 }
        }
    };
    let args: Vec<JsValue> = call
        .args
        .get(2..)
        .map(<[JsValue]>::to_vec)
        .unwrap_or_default();
    let id = cx.state.hooks.schedule_timer(callback, args, delay, repeat);
    Ok(JsValue::Number(id))
}

fn install_timers(cx: &BindCx<'_>, global: &oxidepage_js::JsObject) -> Result<(), JsThrow> {
    define_fn(cx, global, "setTimeout", 1, |cx, call| {
        schedule_timer(cx, call, false)
    })?;
    define_fn(cx, global, "setInterval", 1, |cx, call| {
        schedule_timer(cx, call, true)
    })?;
    define_fn(cx, global, "clearTimeout", 1, |cx, call| {
        if let Ok(id) = cx.arg_f64(call, 0) {
            cx.state.hooks.clear_timer(id);
        }
        Ok(JsValue::Undefined)
    })?;
    define_fn(cx, global, "clearInterval", 1, |cx, call| {
        if let Ok(id) = cx.arg_f64(call, 0) {
            cx.state.hooks.clear_timer(id);
        }
        Ok(JsValue::Undefined)
    })?;
    define_fn(cx, global, "requestAnimationFrame", 1, |cx, call| {
        let callback = call.arg(0);
        let id = cx.state.hooks.request_animation_frame(callback);
        Ok(JsValue::Number(id))
    })?;
    define_fn(cx, global, "cancelAnimationFrame", 1, |cx, call| {
        if let Ok(id) = cx.arg_f64(call, 0) {
            cx.state.hooks.cancel_animation_frame(id);
        }
        Ok(JsValue::Undefined)
    })?;
    Ok(())
}

/// Invokes one animation-frame `callback` with the current `timestamp`
/// (milliseconds), then runs a microtask checkpoint (as after a task).
pub fn fire_raf_callback(cx: &BindCx<'_>, callback: &JsValue, timestamp: f64) {
    if cx.scope.is_function(callback) {
        let result = cx
            .scope
            .call(callback, &JsValue::Undefined, &[JsValue::Number(timestamp)]);
        if let Err(error) = result {
            cx.report_callback_error(&error);
        }
    }
    microtask_checkpoint(cx);
}

/// Spec "perform a microtask checkpoint": drains engine promise jobs, invokes
/// custom-element reactions, and delivers pending `MutationObserver` records,
/// looping until all three settle.
pub fn microtask_checkpoint(cx: &BindCx<'_>) {
    loop {
        let outcome = cx.scope.pump_jobs();
        for error in outcome.errors {
            cx.report_callback_error(&error);
        }
        // Reactions run before observer delivery: an upgrade or lifecycle
        // callback may mutate the DOM, and those mutations must be visible to
        // the observers delivered in the same checkpoint.
        let reacted = drain_custom_element_reactions(cx);
        let delivered = deliver_mutation_observers(cx);
        if !reacted && !delivered {
            break;
        }
    }
}

/// Applies queued connectedness changes for pinned (JS-wrapped) nodes to the
/// strong `connected_wrappers` retention: a connected node's wrapper is held so
/// its author-set expando properties survive GC (jQuery/Angular store data
/// there), and the hold is dropped on disconnect so detached subtrees still
/// free. Returns whether any change was applied.
pub fn drain_pinned_connectivity(cx: &BindCx<'_>) -> bool {
    let changes = cx.state.dom.borrow_mut().take_pinned_connectivity();
    if changes.is_empty() {
        return false;
    }
    for (id, connected) in changes {
        if connected {
            // Revalidate at the drain boundary (L3): retain only if the node is
            // still connected and still has a live wrapper to hold.
            let still_connected = cx
                .state
                .dom
                .borrow()
                .get(id)
                .is_some_and(|node| node.is_connected());
            if still_connected && let Some(wrapper) = cx.peek_node_wrapper(id) {
                cx.state.connected_wrappers.borrow_mut().insert(id, wrapper);
            }
        } else {
            cx.state.connected_wrappers.borrow_mut().remove(&id);
        }
    }
    true
}

/// Invokes the **backup element queue**: every reaction queued with no
/// `[CEReactions]` operation on the stack (the parser's). Returns whether any
/// reaction ran (the caller loops, since reactions may enqueue further work).
pub fn drain_custom_element_reactions(cx: &BindCx<'_>) -> bool {
    invoke_custom_element_reactions(cx, 0)
}

/// The spec's "invoke custom element reactions": runs every reaction the
/// element queue opened at `mark` accumulated, in FIFO order, until the queue
/// drains back down to `mark`. Returns whether any reaction ran.
///
/// Called from two places (ADR-0021):
///
/// - the `[CEReactions]` trampoline ([`cx::native_ce`]) with the mark it took on
///   entry, which is what makes the reactions an operation enqueued run *before
///   that operation returns to script*;
/// - the microtask checkpoint and the event loop with `mark == 0`, which is the
///   spec's **backup element queue** — reactions enqueued with no `[CEReactions]`
///   operation on the stack (the parser's).
///
/// A reaction may enqueue more work (a `connectedCallback` that appends another
/// custom element), so this pops rather than takes: the new entries land above
/// the mark and are handled by the same loop.
pub fn invoke_custom_element_reactions(cx: &BindCx<'_>, mark: usize) -> bool {
    let mut ran = false;
    loop {
        // Bind in its own statement: a `while let` scrutinee holds the borrow
        // guard for the whole body, and every reaction below re-enters the DOM.
        let next = cx.state.dom.borrow_mut().pop_custom_reaction_from(mark);
        let Some(reaction) = next else { break };
        ran = true;
        match reaction {
            oxidepage_dom::CustomElementReaction::Upgrade(node) => upgrade_element(cx, node),
            oxidepage_dom::CustomElementReaction::Connected(node) => {
                deliver_lifecycle(cx, node, Lifecycle::Connected);
            }
            oxidepage_dom::CustomElementReaction::Disconnected(node) => {
                deliver_lifecycle(cx, node, Lifecycle::Disconnected);
            }
            oxidepage_dom::CustomElementReaction::AttributeChanged {
                node,
                name,
                namespace,
                old,
                new,
            } => deliver_attribute_changed(cx, node, &name, namespace.as_deref(), old, new),
        }
    }
    ran
}

enum Lifecycle {
    Connected,
    Disconnected,
}

/// The local name of a live element node, or `None` if it is gone / not an
/// element.
fn element_local_name(cx: &BindCx<'_>, node: oxidepage_base::NodeId) -> Option<String> {
    cx.state
        .dom
        .borrow()
        .get(node)
        .and_then(oxidepage_dom::Node::as_element)
        .map(|el| el.name.local.to_string())
}

/// Runs a defined constructor against a pre-created `Undefined` element.
///
/// Also invoked synchronously by `document.createElement` for a defined custom
/// element (spec "synchronous custom elements flag").
pub(crate) fn upgrade_element(cx: &BindCx<'_>, node: oxidepage_base::NodeId) {
    use oxidepage_dom::custom_element::CustomElementState;

    // Revalidate: the node may be gone, or already handled by another intent.
    if cx.state.dom.borrow().custom_state(node) != CustomElementState::Undefined {
        return;
    }
    let Some(local) = element_local_name(cx, node) else {
        return;
    };
    let Some(def) = cx.state.custom_elements.borrow().by_name(&local) else {
        return;
    };
    let Some(ce_construct) = cx
        .state
        .js
        .borrow()
        .as_ref()
        .map(|js| js.ce_construct.clone())
    else {
        return;
    };

    cx.state
        .custom_elements
        .borrow_mut()
        .construction_stack
        .push(node);
    let result = cx.scope.call(
        &ce_construct,
        &JsValue::Undefined,
        std::slice::from_ref(&def.constructor),
    );
    // Whether or not the base constructor popped it, ensure our entry is gone.
    {
        let mut reg = cx.state.custom_elements.borrow_mut();
        if reg.construction_stack.last() == Some(&node) {
            reg.construction_stack.pop();
        }
    }

    if let Err(error) = result {
        cx.state
            .dom
            .borrow_mut()
            .set_custom_state(node, CustomElementState::Failed);
        cx.report_callback_error(&error);
        return;
    }
    cx.state
        .dom
        .borrow_mut()
        .set_custom_state(node, CustomElementState::Custom);
    // The upgraded wrapper carries JS state that cannot be rebuilt; hold it
    // strongly so the weak node-wrapper cache can't drop it (see the field doc).
    retain_custom_wrapper(cx, node);

    // Initial reactions after a successful upgrade: attributeChanged for each
    // present observed attribute (in any namespace), then connectedCallback if
    // connected. Iterate the element's actual attribute list so a namespaced
    // attribute whose local name is observed gets its initial callback with the
    // correct namespace; looking up only the null namespace would miss it.
    let observed: Vec<(String, Option<String>, String)> = {
        let dom = cx.state.dom.borrow();
        dom.get(node)
            .and_then(oxidepage_dom::Node::as_element)
            .map(|el| {
                el.attrs()
                    .iter()
                    .filter(|a| {
                        def.observed_attributes
                            .iter()
                            .any(|observed| observed.as_str() == &*a.name.local)
                    })
                    .map(|a| {
                        let ns = a.name.ns.to_string();
                        let namespace = (!ns.is_empty()).then_some(ns);
                        (a.name.local.to_string(), namespace, a.value.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    for (name, namespace, value) in observed {
        deliver_attribute_changed(cx, node, &name, namespace.as_deref(), None, Some(value));
    }
    if cx
        .state
        .dom
        .borrow()
        .get(node)
        .is_some_and(|n| n.is_connected())
    {
        deliver_lifecycle(cx, node, Lifecycle::Connected);
    }
}

/// Strongly retains `node`'s JS wrapper (an upgraded/constructed custom
/// element) so the weak node-wrapper cache cannot free it, taking its
/// subclass prototype and instance fields with it.
pub(crate) fn retain_custom_wrapper(cx: &BindCx<'_>, node: oxidepage_base::NodeId) {
    if let Ok(wrapper) = cx.node_to_js(node) {
        cx.state.custom_wrappers.borrow_mut().insert(node, wrapper);
    }
}

/// Invokes a `connectedCallback`/`disconnectedCallback` on a custom element.
fn deliver_lifecycle(cx: &BindCx<'_>, node: oxidepage_base::NodeId, which: Lifecycle) {
    let Some(local) = element_local_name(cx, node) else {
        return;
    };
    let Some(def) = cx.state.custom_elements.borrow().by_name(&local) else {
        return;
    };
    let callback = match which {
        Lifecycle::Connected => def.connected.clone(),
        Lifecycle::Disconnected => def.disconnected.clone(),
    };
    let Some(callback) = callback else { return };
    let Ok(wrapper) = cx.node_to_js(node) else {
        return;
    };
    if let Err(error) = cx.scope.call(&callback, &wrapper, &[]) {
        cx.report_callback_error(&error);
    }
}

/// Invokes `attributeChangedCallback(name, oldValue, newValue, namespace)` when
/// the element observes `name`.
fn deliver_attribute_changed(
    cx: &BindCx<'_>,
    node: oxidepage_base::NodeId,
    name: &str,
    namespace: Option<&str>,
    old: Option<String>,
    new: Option<String>,
) {
    let Some(local) = element_local_name(cx, node) else {
        return;
    };
    let Some(def) = cx.state.custom_elements.borrow().by_name(&local) else {
        return;
    };
    if !def.observes(name) {
        return;
    }
    let Some(callback) = def.attribute_changed.clone() else {
        return;
    };
    let Ok(wrapper) = cx.node_to_js(node) else {
        return;
    };
    let to_js = |v: Option<String>| v.map_or(JsValue::Null, JsValue::String);
    let args = [
        JsValue::String(name.to_owned()),
        to_js(old),
        to_js(new),
        namespace.map_or(JsValue::Null, |ns| JsValue::String(ns.to_owned())),
    ];
    if let Err(error) = cx.scope.call(&callback, &wrapper, &args) {
        cx.report_callback_error(&error);
    }
}

/// Re-evaluates every live `MediaQueryList` after the embedding viewport
/// changes and dispatches `change` for results that flipped.
pub fn reevaluate_media_queries(cx: &BindCx<'_>) {
    let lists = cx.state.media_queries.borrow().clone();
    for list in lists {
        let next = cx.state.style.borrow().media_query_matches(&list.media);
        if next == list.matches.replace(next) {
            continue;
        }
        let Some(key) = list.key.get() else {
            continue;
        };
        if let Err(error) =
            fire_simple_event(cx, EventTargetKey::MediaQueryList(key), "change", false)
        {
            cx.report_engine_error(format!("MediaQueryList change dispatch failed: {error:?}"));
        }
    }
}

/// One `ResizeObserverEntry`'s geometry, computed under the layout borrow (no
/// JS): the observed node plus its content rect and the three box sizes.
struct RoEntryData {
    node: oxidepage_base::NodeId,
    content_rect: oxidepage_base::Rect,
    border_box: (f32, f32),
    content_box: (f32, f32),
    device_pixel: (f32, f32),
}

/// The current O(1) delivery gate (live DOM versions + the layout paint stamp).
fn compute_obs_gate(cx: &BindCx<'_>) -> state::ObsGate {
    let (style_version, structure_version) = {
        let dom = cx.state.dom.borrow();
        (dom.style_version(), dom.structure_version())
    };
    let (paint, document_scroll_version) = {
        let layout = cx.state.layout.borrow();
        (layout.paint_stamp(), layout.document_scroll_version())
    };
    (
        style_version,
        structure_version,
        paint,
        document_scroll_version,
    )
}

/// True if any observer has at least one target.
fn has_observer_targets(cx: &BindCx<'_>) -> bool {
    cx.state
        .resize_observers
        .borrow()
        .iter()
        .any(|o| !o.targets.borrow().is_empty())
        || cx
            .state
            .intersection_observers
            .borrow()
            .iter()
            .any(|o| !o.targets.borrow().is_empty())
}

/// Delivers pending `ResizeObserver` (and, in Phase 5, `IntersectionObserver`)
/// notifications. Returns whether any callback ran, so the page event loop can
/// keep iterating until the observations converge.
///
/// Three phases keep JS out of the layout borrow (borrow-discipline risk):
/// gate → geometry (reflow + compute plain-Rust entry data, updating each
/// target's `last`/`initial_pending`) → JS (build wrappers, invoke callbacks).
/// The gate is re-stamped after the reflow so an unchanged next call fast-outs.
pub fn deliver_observations(cx: &BindCx<'_>) -> bool {
    // (a) Fast-out: nothing observed.
    if !has_observer_targets(cx) {
        return false;
    }
    // (b) Gate: skip when neither the DOM nor the layout changed since the last
    // pass and no `observe()` forced a fresh pass. A boxless observed target
    // does not keep the gate bypassed — `obs_dirty` is a one-shot, cleared after
    // the geometry pass below, and the target's eventual box appearance is a
    // layout change that moves the gate.
    let gate = compute_obs_gate(cx);
    if !cx.state.obs_dirty.get() && Some(gate) == cx.state.obs_gate.get() {
        return false;
    }

    // Snapshot observer handles outside the layout borrow.
    let observers: Vec<Rc<state::ResizeObserverData>> = cx
        .state
        .resize_observers
        .borrow()
        .iter()
        .map(Rc::clone)
        .collect();
    let io_observers: Vec<Rc<state::IntersectionObserverData>> = cx
        .state
        .intersection_observers
        .borrow()
        .iter()
        .map(Rc::clone)
        .collect();
    let now = cx.now_ms();

    // (c) Geometry phase: reflow, then compute entry data with no JS.
    let mut work: Vec<(Rc<state::ResizeObserverData>, Vec<RoEntryData>)> = Vec::new();
    let mut io_work: Vec<(Rc<state::IntersectionObserverData>, Vec<IoEntryData>)> = Vec::new();
    imp::geometry_support::flush_layout(cx, |dom, layout| {
        let dpr = layout.viewport().dpr;
        for observer in &io_observers {
            let entries = io_compute_entries(&cx.state, observer, dom, layout, now);
            if !entries.is_empty() {
                io_work.push((Rc::clone(observer), entries));
            }
        }
        for observer in &observers {
            // Drop targets whose node has been freed (removed + wrapper GC'd):
            // querying layout with a stale NodeId would panic.
            observer
                .targets
                .borrow_mut()
                .retain(|t| dom.get(t.node).is_some());
            let mut entries = Vec::new();
            for target in observer.targets.borrow().iter() {
                // `border_box_size`, not `border_box`: a ResizeObserver observes
                // the *untransformed* border box, so `scale(2)` must not double
                // the reported size and a transform change must not notify
                // (ADR-0026).
                match (
                    layout.content_box(target.node),
                    layout.border_box_size(target.node),
                ) {
                    (Some(content), Some(border_size)) => {
                        let content_size = (content.size.width, content.size.height);
                        let device = (content_size.0 * dpr, content_size.1 * dpr);
                        let chosen = match target.box_kind {
                            state::RoBoxKind::ContentBox => content_size,
                            state::RoBoxKind::BorderBox => border_size,
                            state::RoBoxKind::DevicePixelContentBox => device,
                        };
                        if target.initial_pending.get() || target.last.get() != Some(chosen) {
                            target.last.set(Some(chosen));
                            target.initial_pending.set(false);
                            entries.push(RoEntryData {
                                node: target.node,
                                content_rect: content,
                                border_box: border_size,
                                content_box: content_size,
                                device_pixel: device,
                            });
                        }
                    }
                    _ => {
                        // No box (display:none / removed): report 0×0 once, only
                        // if the element previously had a non-zero box. An
                        // initial observation on a boxless element waits for a
                        // box to appear (matching browsers).
                        if let Some((w, h)) = target.last.get()
                            && (w > 0.0 || h > 0.0)
                        {
                            target.last.set(Some((0.0, 0.0)));
                            entries.push(RoEntryData {
                                node: target.node,
                                content_rect: oxidepage_base::Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
                                border_box: (0.0, 0.0),
                                content_box: (0.0, 0.0),
                                device_pixel: (0.0, 0.0),
                            });
                        }
                    }
                }
            }
            if !entries.is_empty() {
                work.push((Rc::clone(observer), entries));
            }
        }
    });

    // (d) The one-shot observe() force is spent; re-stamp the gate against the
    // post-reflow state.
    cx.state.obs_dirty.set(false);
    cx.state.obs_gate.set(Some(compute_obs_gate(cx)));

    if work.is_empty() && io_work.is_empty() {
        return false;
    }

    // (e) JS phase: build entry wrappers and invoke callbacks (no borrows held).
    let mut delivered = false;
    for (observer, entries) in io_work {
        let mut entry_values = Vec::with_capacity(entries.len());
        let mut build_failed = false;
        for entry in &entries {
            match build_io_entry(cx, entry) {
                Ok(value) => entry_values.push(value),
                Err(_) => {
                    build_failed = true;
                    break;
                }
            }
        }
        if build_failed {
            cx.report_engine_error("failed to build IntersectionObserverEntry".into());
            continue;
        }
        let array = match cx.scope.new_array(&entry_values) {
            Ok(array) => JsValue::Object(array),
            Err(_) => continue,
        };
        let wrapper = observer
            .wrapper
            .borrow()
            .clone()
            .unwrap_or(JsValue::Undefined);
        delivered = true;
        if let Err(error) = cx
            .scope
            .call(&observer.callback, &wrapper, &[array, wrapper.clone()])
        {
            cx.report_callback_error(&error);
        }
    }
    for (observer, entries) in work {
        let mut entry_values = Vec::with_capacity(entries.len());
        let mut build_failed = false;
        for entry in &entries {
            match build_ro_entry(cx, entry) {
                Ok(value) => entry_values.push(value),
                Err(_) => {
                    build_failed = true;
                    break;
                }
            }
        }
        if build_failed {
            cx.report_engine_error("failed to build ResizeObserverEntry".into());
            continue;
        }
        let array = match cx.scope.new_array(&entry_values) {
            Ok(array) => JsValue::Object(array),
            Err(_) => continue,
        };
        let wrapper = observer
            .wrapper
            .borrow()
            .clone()
            .unwrap_or(JsValue::Undefined);
        delivered = true;
        if let Err(error) = cx
            .scope
            .call(&observer.callback, &wrapper, &[array, wrapper.clone()])
        {
            cx.report_callback_error(&error);
        }
    }
    delivered
}

/// Builds a `ResizeObserverSize` frozen array (`[{inlineSize, blockSize}]`).
fn ro_size_array(cx: &BindCx<'_>, inline_size: f32, block_size: f32) -> Result<JsValue, JsThrow> {
    let obj = cx.scope.new_object().map_err(JsThrow::from)?;
    cx.scope
        .set(&obj, "inlineSize", &JsValue::Number(f64::from(inline_size)))
        .map_err(JsThrow::from)?;
    cx.scope
        .set(&obj, "blockSize", &JsValue::Number(f64::from(block_size)))
        .map_err(JsThrow::from)?;
    let size = cx.freeze(&JsValue::Object(obj))?;
    let array = cx
        .scope
        .new_array(std::slice::from_ref(&size))
        .map_err(JsThrow::from)?;
    cx.freeze(&JsValue::Object(array))
}

/// Builds a `ResizeObserverEntry` wrapper from precomputed geometry.
fn build_ro_entry(cx: &BindCx<'_>, entry: &RoEntryData) -> Result<JsValue, JsThrow> {
    let target = cx.node_to_js(entry.node)?;
    let content_rect = cx.new_dom_rect(
        "DOMRectReadOnly",
        imp::geometry_support::rect_data(entry.content_rect),
    )?;
    let border_box_size = ro_size_array(cx, entry.border_box.0, entry.border_box.1)?;
    let content_box_size = ro_size_array(cx, entry.content_box.0, entry.content_box.1)?;
    let device_pixel_content_box_size =
        ro_size_array(cx, entry.device_pixel.0, entry.device_pixel.1)?;
    cx.new_resize_observer_entry(state::RoEntryView {
        target,
        content_rect,
        border_box_size,
        content_box_size,
        device_pixel_content_box_size,
    })
}

/// One `IntersectionObserverEntry`'s geometry, computed under the layout borrow.
struct IoEntryData {
    node: oxidepage_base::NodeId,
    time: f64,
    root_bounds: oxidepage_base::Rect,
    bounding: oxidepage_base::Rect,
    intersection: oxidepage_base::Rect,
    is_intersecting: bool,
    ratio: f64,
}

/// The intersection root rectangle (viewport for an implicit/`Document` root,
/// else the root element's padding box), in viewport coordinates.
///
/// Under [`PageState::whole_document_visible`] the implicit root spans the whole
/// document instead: the embedder is rendering all of it, so none of it is
/// below a fold.
fn io_root_rect(
    state: &PageState,
    dom: &oxidepage_dom::DomTree,
    layout: &oxidepage_layout::LayoutEngine,
    root: Option<oxidepage_base::NodeId>,
) -> Option<oxidepage_base::Rect> {
    match root {
        None => {
            let vp = layout.viewport();
            let (mut width, mut height) = (vp.width, vp.height);
            if state.whole_document_visible.get()
                && let Some(root) = dom.document_element()
                && let Some((scroll_w, scroll_h)) = layout.scroll_size(root)
            {
                // The document never scrolls here, so the root rect grows to the
                // scrollable content instead — clamped up to the viewport, which
                // is the floor for a document shorter than one screen.
                width = width.max(scroll_w);
                height = height.max(scroll_h);
            }
            Some(oxidepage_base::Rect::from_xywh(0.0, 0.0, width, height))
        }
        Some(node) => {
            // padding box = border box inset by the border widths.
            let border = layout.border_box(node)?;
            let client = layout.client_box(node)?;
            Some(oxidepage_base::Rect::from_xywh(
                border.origin.x + client.left,
                border.origin.y + client.top,
                client.width,
                client.height,
            ))
        }
    }
}

/// Computes the intersection entries for one observer, updating each target's
/// `last`/`initial_pending`. No JS is called (runs under the layout borrow).
fn io_compute_entries(
    state: &PageState,
    observer: &state::IntersectionObserverData,
    dom: &oxidepage_dom::DomTree,
    layout: &oxidepage_layout::LayoutEngine,
    now: f64,
) -> Vec<IoEntryData> {
    // Drop targets whose node has been freed: `bounding_client_rect` walks the
    // DOM and would panic on a stale NodeId.
    observer
        .targets
        .borrow_mut()
        .retain(|t| dom.get(t.node).is_some());
    let Some(root_rect) = io_root_rect(state, dom, layout, observer.root) else {
        return Vec::new();
    };
    // Expand the root rect by `rootMargin` (% resolved against the root axis).
    let top = observer.root_margin[0].resolve(root_rect.size.height);
    let right = observer.root_margin[1].resolve(root_rect.size.width);
    let bottom = observer.root_margin[2].resolve(root_rect.size.height);
    let left = observer.root_margin[3].resolve(root_rect.size.width);
    let r_x1 = root_rect.origin.x - left;
    let r_y1 = root_rect.origin.y - top;
    let r_x2 = root_rect.origin.x + root_rect.size.width + right;
    let r_y2 = root_rect.origin.y + root_rect.size.height + bottom;
    let root_bounds =
        oxidepage_base::Rect::from_xywh(r_x1, r_y1, (r_x2 - r_x1).max(0.0), (r_y2 - r_y1).max(0.0));

    let mut entries = Vec::new();
    for target in observer.targets.borrow().iter() {
        let (t_rect, rendered) = match layout.bounding_client_rect(dom, target.node) {
            Some(r) => (r, true),
            None => (oxidepage_base::Rect::from_xywh(0.0, 0.0, 0.0, 0.0), false),
        };
        let t_x1 = t_rect.origin.x;
        let t_y1 = t_rect.origin.y;
        let t_x2 = t_rect.origin.x + t_rect.size.width;
        let t_y2 = t_rect.origin.y + t_rect.size.height;
        let ix1 = t_x1.max(r_x1);
        let iy1 = t_y1.max(r_y1);
        let ix2 = t_x2.min(r_x2);
        let iy2 = t_y2.min(r_y2);
        let iw = ix2 - ix1;
        let ih = iy2 - iy1;
        // Touching edges (a zero-width/height overlap) still counts as
        // intersecting, per spec.
        let is_intersecting = rendered && iw >= 0.0 && ih >= 0.0;
        let intersection = if is_intersecting {
            oxidepage_base::Rect::from_xywh(ix1, iy1, iw.max(0.0), ih.max(0.0))
        } else {
            oxidepage_base::Rect::from_xywh(0.0, 0.0, 0.0, 0.0)
        };
        let target_area = f64::from(t_rect.size.width) * f64::from(t_rect.size.height);
        let ratio = if !is_intersecting {
            0.0
        } else if target_area <= 0.0 {
            // A zero-area target that touches the root is fully "intersecting".
            1.0
        } else {
            (f64::from(iw.max(0.0)) * f64::from(ih.max(0.0))) / target_area
        };
        let bucket = observer.thresholds.iter().filter(|&&t| t <= ratio).count();
        let state = (is_intersecting, bucket);
        if target.initial_pending.get() || target.last.get() != Some(state) {
            target.last.set(Some(state));
            target.initial_pending.set(false);
            entries.push(IoEntryData {
                node: target.node,
                time: now,
                root_bounds,
                bounding: t_rect,
                intersection,
                is_intersecting,
                ratio,
            });
        }
    }
    entries
}

/// Builds an `IntersectionObserverEntry` wrapper from precomputed geometry.
fn build_io_entry(cx: &BindCx<'_>, entry: &IoEntryData) -> Result<JsValue, JsThrow> {
    let target = cx.node_to_js(entry.node)?;
    let rect = |r: oxidepage_base::Rect| {
        cx.new_dom_rect("DOMRectReadOnly", imp::geometry_support::rect_data(r))
    };
    cx.new_intersection_observer_entry(state::IoEntryView {
        time: entry.time,
        root_bounds: rect(entry.root_bounds)?,
        bounding_client_rect: rect(entry.bounding)?,
        intersection_rect: rect(entry.intersection)?,
        is_intersecting: entry.is_intersecting,
        intersection_ratio: entry.ratio,
        target,
    })
}

/// `IntersectionObserver.takeRecords()`: runs this observer's geometry phase
/// (updating `last`, without invoking the callback) and returns the entries.
pub(crate) fn io_take_records(
    cx: &BindCx<'_>,
    observer: &state::IntersectionObserverData,
) -> Result<JsValue, JsThrow> {
    let now = cx.now_ms();
    let entries = imp::geometry_support::flush_layout(cx, |dom, layout| {
        io_compute_entries(&cx.state, observer, dom, layout, now)
    });
    let mut values = Vec::with_capacity(entries.len());
    for entry in &entries {
        values.push(build_io_entry(cx, entry)?);
    }
    cx.scope
        .new_array(&values)
        .map(JsValue::Object)
        .map_err(JsThrow::from)
}

/// Notifies mutation observers with queued records. Returns whether any
/// callback ran (more microtasks may now be pending).
/// Body of the mutation-observer compound microtask (queued by
/// [`BindCx::queue_mutation_microtask`]).
fn mutation_notify_glue(
    cx: &BindCx<'_>,
    _call: &oxidepage_js::HostCall,
) -> Result<JsValue, JsThrow> {
    // Cleared first: a record queued by an observer callback below must be able
    // to queue a *fresh* microtask rather than be folded into this one.
    cx.state.mutation_microtask_queued.set(false);
    deliver_mutation_observers(cx);
    Ok(JsValue::Undefined)
}

fn deliver_mutation_observers(cx: &BindCx<'_>) -> bool {
    if !cx.state.dom.borrow().observers().has_pending_records() {
        return false;
    }
    let entries: Vec<(oxidepage_dom::MutationObserverId, JsValue, JsValue)> = cx
        .state
        .observers
        .borrow()
        .iter()
        .map(|e| (e.id, e.wrapper.clone(), e.callback.clone()))
        .collect();
    let mut delivered = false;
    for (id, wrapper, callback) in entries {
        let records = cx
            .state
            .dom
            .borrow_mut()
            .observers_mut()
            .take_records_for_notify(id);
        if records.is_empty() {
            continue;
        }
        delivered = true;
        match imp::mutation_observer::records_to_js(cx, records) {
            Ok(array) => {
                if let Err(error) = cx
                    .scope
                    .call(&callback, &wrapper, &[array, wrapper.clone()])
                {
                    cx.report_callback_error(&error);
                }
            }
            Err(_) => cx.report_engine_error("failed to build MutationRecord array".into()),
        }
    }
    delivered
}

/// Consumes finalized-wrapper notifications from the engine, decrementing
/// pins and freeing fully-unpinned detached trees (unless the parser holds
/// tree handles or observers still reference removed nodes).
pub fn process_finalized(state: &Rc<PageState>, finalized: Vec<(u32, u64)>) {
    for (tag, data) in finalized {
        match tag {
            TAG_NODE => {
                let Some(id) = cx::unpack_node(data) else {
                    continue;
                };
                {
                    let mut dom = state.dom.borrow_mut();
                    dom.unpin(id);
                    if !state.parsing.get() && !dom.observers().has_pending_records() {
                        dom.free_detached_tree_if_unpinned(id);
                    }
                }
                // Purge this node's `[SameObject]` children only once the exact
                // node (index AND generation) is gone from the tree. A finalized
                // wrapper for a still-live node — or a newer node that reused the
                // slot with a different generation — keeps its cached children.
                if state.dom.borrow().get(id).is_none() {
                    state
                        .same_object
                        .borrow_mut()
                        .retain(|(index, generation, _), _| {
                            !(*index == id.index() && *generation == id.generation().get())
                        });
                }
            }
            TAG_SLAB => {
                state.slab.borrow_mut().remove(data);
                // A host event target (`new EventTarget()`, `XMLHttpRequest`)
                // keeps its listeners and `onX` handlers in the shared
                // registries under its slab key. Nothing else will ever ask for
                // them once the object is gone, and slab keys are never
                // recycled, so they would simply accumulate.
                let key = crate::events::EventTargetKey::Host(data);
                state.listeners.borrow_mut().remove_target(key);
                state
                    .event_handlers
                    .borrow_mut()
                    .retain(|(target, _), _| *target != key);
            }
            _ => {}
        }
    }
}

/// Routes a completed net event to the fetch/XHR waiting on it: accumulates
/// the response, resolves/rejects the fetch promise, or advances the XHR
/// readyState and fires its events. Runs a microtask checkpoint after (so
/// promise reactions run).
pub fn deliver_net_event(cx: &BindCx<'_>, event: NetEvent) {
    let id = event.request_id();
    match event {
        NetEvent::Headers {
            status,
            status_text,
            headers,
            final_url,
            redirected,
            response_type,
            ..
        } => {
            let xhr = {
                let mut pending = cx.state.pending_net.borrow_mut();
                match pending.get_mut(&id) {
                    Some(PendingNet::Fetch { response, .. }) => {
                        response.status = status;
                        response.status_text = status_text;
                        response.headers = headers;
                        response.url = final_url;
                        response.redirected = redirected;
                        response.response_type = response_type;
                        None
                    }
                    Some(PendingNet::Xhr { xhr }) => {
                        let mut x = xhr.borrow_mut();
                        x.status = status;
                        x.status_text = status_text;
                        // `responseURL` is the final post-redirect URL with its
                        // fragment stripped; it used to be discarded here.
                        x.response_url = strip_fragment(&final_url);
                        x.total = content_length_total(&headers);
                        x.response_headers = headers;
                        x.ready_state = 2; // HEADERS_RECEIVED
                        Some(Rc::clone(xhr))
                    }
                    None => None,
                }
            };
            if let Some(xhr) = xhr
                && let Some(xhr) = imp::xml_http_request::rehydrate(&xhr)
            {
                // The response head is the earliest proof the request body went
                // out, so the upload half completes here.
                imp::xml_http_request::upload_finished(cx, &xhr);
                imp::xml_http_request::fire_plain(cx, &xhr, "readystatechange");
            }
        }
        NetEvent::Chunk { data, .. } => {
            let xhr = {
                let mut pending = cx.state.pending_net.borrow_mut();
                match pending.get_mut(&id) {
                    Some(PendingNet::Fetch { response, .. }) => {
                        response.body.extend_from_slice(&data);
                        None
                    }
                    Some(PendingNet::Xhr { xhr }) => Some(Rc::clone(xhr)),
                    None => None,
                }
            };
            if let Some(xhr) = xhr
                && let Some(xhr) = imp::xml_http_request::rehydrate(&xhr)
            {
                imp::xml_http_request::chunk_received(cx, &xhr, &data);
            }
        }
        NetEvent::Done { .. } => {
            let entry = cx.state.pending_net.borrow_mut().remove(&id);
            match entry {
                Some(PendingNet::Fetch {
                    resolve,
                    reject,
                    response,
                    signal,
                }) => {
                    // Settled: drop this id from its signal's pending list.
                    prune_signal_fetch(&signal, id);
                    match finalize_fetch_response(cx, response) {
                        Ok(resp) => {
                            let _ = cx.scope.call(&resolve, &JsValue::Undefined, &[resp]);
                        }
                        // Building the Response failed: reject rather than leave the
                        // promise pending forever.
                        Err(e) => {
                            let msg = format!("{e:?}");
                            let err =
                                cx.type_error_value(&format!("Failed to build response: {msg}"));
                            let _ = cx.scope.call(&reject, &JsValue::Undefined, &[err]);
                        }
                    }
                }
                Some(PendingNet::Xhr { xhr }) => {
                    if let Some(xhr) = imp::xml_http_request::rehydrate(&xhr) {
                        // The entry is already gone from `pending_net`, so
                        // `terminate` only has the timeout timer left to disarm.
                        imp::xml_http_request::terminate(cx, &xhr);
                        imp::xml_http_request::upload_finished(cx, &xhr);
                        imp::xml_http_request::request_done(cx, &xhr);
                    }
                }
                None => {}
            }
        }
        NetEvent::Error { error, .. } => {
            let entry = cx.state.pending_net.borrow_mut().remove(&id);
            match entry {
                Some(PendingNet::Fetch { reject, signal, .. }) => {
                    prune_signal_fetch(&signal, id);
                    let err = cx.type_error_value(&format!("Failed to fetch: {error}"));
                    let _ = cx.scope.call(&reject, &JsValue::Undefined, &[err]);
                }
                Some(PendingNet::Xhr { xhr }) => {
                    if let Some(xhr) = imp::xml_http_request::rehydrate(&xhr) {
                        imp::xml_http_request::terminate(cx, &xhr);
                        imp::xml_http_request::request_error(cx, &xhr, "error");
                    }
                }
                None => {}
            }
        }
    }
    microtask_checkpoint(cx);
}

/// A URL with its fragment removed — what `XMLHttpRequest.responseURL` is
/// defined to expose.
fn strip_fragment(url: &str) -> String {
    match url.split_once('#') {
        Some((head, _)) => head.to_owned(),
        None => url.to_owned(),
    }
}

/// `ProgressEvent.total` for a download: the response's `Content-Length`, and
/// nothing else. A cached or compressed response has none, and then `total`
/// stays absent (`lengthComputable === false`) rather than being invented from
/// the bytes that happen to have arrived.
///
/// Parsed as the `unsigned long long` the IDL says it is. Parsing it as `f64`
/// accepted `NaN`, `inf`, `-1` and `1e999` straight off a hostile or broken
/// wire and then reported `lengthComputable === true` for them, so every
/// `e.loaded / e.total` progress bar read `NaN`.
fn content_length_total(headers: &[(String, String)]) -> Option<f64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map(|len| len as f64)
}

/// Builds a `Response` object from an accumulated fetch. A cross-origin opaque
/// response exposes nothing to script (status 0, empty status text/url/headers,
/// no body); a CORS response is `type: "cors"` with headers already pruned by
/// the net layer.
fn finalize_fetch_response(cx: &BindCx<'_>, resp: PendingResponse) -> Result<JsValue, JsThrow> {
    let data = match resp.response_type {
        ResponseType::Opaque => ResponseData {
            status: 0,
            status_text: String::new(),
            url: String::new(),
            redirected: false,
            resp_type: "opaque".to_owned(),
            headers: Rc::new(RefCell::new(HeadersData::default())),
            body: Vec::new(),
            body_used: Cell::new(false),
        },
        ty => ResponseData {
            status: resp.status,
            status_text: resp.status_text,
            url: resp.url,
            redirected: resp.redirected,
            resp_type: if ty == ResponseType::Cors {
                "cors"
            } else {
                "basic"
            }
            .to_owned(),
            headers: Rc::new(RefCell::new(HeadersData::from_pairs(&resp.headers))),
            body: resp.body,
            body_used: Cell::new(false),
        },
    };
    cx.new_net_object("Response", HostData::Response(Rc::new(data)))
}

/// Resolves every stashed `document.fonts.ready` promise (`imp::font_face_set::ready`
/// stashes one whenever it cannot resolve synchronously) with the cached
/// `FontFaceSet`. Called by the page event loop once `fonts_loading` goes
/// false and parsing has finished — see `Page::settle_font_ready`. A no-op
/// (no JS entered) when nothing is waiting or `document.fonts` was never
/// read.
pub fn resolve_font_ready(cx: &BindCx<'_>) {
    let ready = std::mem::take(&mut *cx.state.font_ready_resolvers.borrow_mut());
    let load = std::mem::take(&mut *cx.state.font_load_resolvers.borrow_mut());
    if ready.is_empty() && load.is_empty() {
        return;
    }
    // `ready` resolves with the `FontFaceSet` itself; `load` with an (empty)
    // `sequence<FontFace>`.
    if let Some(value) = cx.state.font_face_set_js.borrow().clone() {
        for resolve in ready {
            if let Err(error) =
                cx.scope
                    .call(&resolve, &JsValue::Undefined, std::slice::from_ref(&value))
            {
                cx.report_callback_error(&error);
            }
        }
    }
    for resolve in load {
        let empty = cx
            .scope
            .eval("[]", "oxidepage:font-load")
            .unwrap_or(JsValue::Undefined);
        if let Err(error) =
            cx.scope
                .call(&resolve, &JsValue::Undefined, std::slice::from_ref(&empty))
        {
            cx.report_callback_error(&error);
        }
    }
    microtask_checkpoint(cx);
}

/// Fires a timer callback: a function is called; a string is evaluated
/// (legacy `setTimeout("code", ms)` behavior).
pub fn fire_timer_callback(cx: &BindCx<'_>, callback: &JsValue, args: &[JsValue]) {
    let result = match callback {
        JsValue::String(code) => cx.scope.eval(code, "oxidepage:timer"),
        value if cx.scope.is_function(value) => cx.scope.call(value, &JsValue::Undefined, args),
        _ => Ok(JsValue::Undefined),
    };
    if let Err(error) = result {
        cx.report_callback_error(&error);
    }
    microtask_checkpoint(cx);
}

#[cfg(test)]
mod tests {
    use super::content_length_total;

    fn headers(value: &str) -> Vec<(String, String)> {
        vec![("Content-Length".to_owned(), value.to_owned())]
    }

    #[test]
    fn content_length_accepts_only_an_unsigned_integer() {
        assert_eq!(content_length_total(&headers(" 1024 ")), Some(1024.0));
        assert_eq!(content_length_total(&headers("0")), Some(0.0));
        assert_eq!(content_length_total(&[]), None);

        // Everything a `f64` parse used to wave through, each of which made
        // `lengthComputable` true with an unusable `total`.
        for hostile in ["NaN", "inf", "Infinity", "-1", "1e999", "1.5", "0x10", ""] {
            assert_eq!(
                content_length_total(&headers(hostile)),
                None,
                "`Content-Length: {hostile}` must not be reported as a length"
            );
        }
    }
}
