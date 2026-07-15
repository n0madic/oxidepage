//! `IntersectionObserver`: constructor + init parsing, accessors, and the
//! `observe`/`unobserve`/`disconnect`/`takeRecords` operations.

use std::cell::Cell;
use std::rc::Rc;

use oxidepage_base::NodeId;
use oxidepage_js::{HostCall, JsThrow, JsValue};

use crate::cx::BindCx;
use crate::state::{IntersectionObserverData, IoMargin, IoTarget};

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    callback: JsValue,
    options: JsValue,
) -> Result<JsValue, JsThrow> {
    if !cx.scope.is_function(&callback) {
        return Err(JsThrow::Type(
            "IntersectionObserver constructor requires a callback function".into(),
        ));
    }
    let root = parse_root(cx, &options)?;
    let root_margin = parse_root_margin(cx, &options)?;
    let thresholds = parse_thresholds(cx, &options)?;
    cx.new_intersection_observer(IntersectionObserverData {
        callback,
        wrapper: std::cell::RefCell::new(None),
        root,
        root_margin,
        thresholds,
        targets: std::cell::RefCell::new(Vec::new()),
    })
}

/// Parses `options.root`: an `Element` roots there; a `Document`, `null`, or an
/// omitted root means the viewport. Any other value is a `TypeError` (WebIDL
/// `(Element or Document)?`).
fn parse_root(cx: &BindCx<'_>, options: &JsValue) -> Result<Option<NodeId>, JsThrow> {
    let JsValue::Object(obj) = options else {
        return Ok(None);
    };
    let root = cx.scope.get(obj, "root").unwrap_or(JsValue::Undefined);
    if root.is_nullish() {
        return Ok(None);
    }
    if let Ok(element) = cx.this_element(&root) {
        return Ok(Some(element));
    }
    // A Document root is treated as the viewport (v1); anything else is invalid.
    if cx.this_document(&root).is_ok() {
        return Ok(None);
    }
    Err(JsThrow::Type(
        "IntersectionObserver root must be an Element or Document".into(),
    ))
}

/// Parses `options.rootMargin` (default `"0px"`) with CSS-shorthand expansion.
fn parse_root_margin(cx: &BindCx<'_>, options: &JsValue) -> Result<[IoMargin; 4], JsThrow> {
    let raw = match options {
        JsValue::Object(obj) => match cx.scope.get(obj, "rootMargin") {
            Ok(JsValue::Undefined) | Err(_) => "0px".to_owned(),
            Ok(value) => cx.scope.coerce_string(&value).map_err(JsThrow::from)?,
        },
        _ => "0px".to_owned(),
    };
    let parts: Vec<IoMargin> = raw
        .split_whitespace()
        .map(parse_margin_component)
        .collect::<Result<_, _>>()
        .map_err(|_| {
            cx.dom_throw(
                oxidepage_base::DomExceptionKind::SyntaxError,
                "rootMargin must be 1–4 px/% values",
            )
        })?;
    let expand = |values: &[IoMargin]| -> Result<[IoMargin; 4], ()> {
        Ok(match values.len() {
            1 => [values[0], values[0], values[0], values[0]],
            2 => [values[0], values[1], values[0], values[1]],
            3 => [values[0], values[1], values[2], values[1]],
            4 => [values[0], values[1], values[2], values[3]],
            _ => return Err(()),
        })
    };
    expand(&parts).map_err(|()| {
        cx.dom_throw(
            oxidepage_base::DomExceptionKind::SyntaxError,
            "rootMargin must have 1–4 values",
        )
    })
}

fn parse_margin_component(token: &str) -> Result<IoMargin, ()> {
    if let Some(px) = token.strip_suffix("px") {
        px.parse::<f32>().map(IoMargin::Px).map_err(|_| ())
    } else if let Some(pct) = token.strip_suffix('%') {
        pct.parse::<f32>().map(IoMargin::Percent).map_err(|_| ())
    } else {
        Err(())
    }
}

/// Parses `options.threshold` (number or sequence) into a sorted, `[0,1]`
/// checked, non-empty list.
fn parse_thresholds(cx: &BindCx<'_>, options: &JsValue) -> Result<Vec<f64>, JsThrow> {
    let value = match options {
        JsValue::Object(obj) => cx.scope.get(obj, "threshold").unwrap_or(JsValue::Undefined),
        _ => JsValue::Undefined,
    };
    let mut thresholds = Vec::new();
    match &value {
        JsValue::Undefined => {}
        JsValue::Object(obj) => {
            if let Ok(len) = cx.scope.array_length(obj) {
                for i in 0..len {
                    let el = cx.scope.array_get(obj, i).map_err(JsThrow::from)?;
                    thresholds.push(cx.scope.coerce_number(&el).map_err(JsThrow::from)?);
                }
            } else {
                thresholds.push(cx.scope.coerce_number(&value).map_err(JsThrow::from)?);
            }
        }
        other => thresholds.push(cx.scope.coerce_number(other).map_err(JsThrow::from)?),
    }
    for &n in &thresholds {
        if !(0.0..=1.0).contains(&n) {
            return Err(JsThrow::Range(
                "IntersectionObserver threshold must be in [0, 1]".into(),
            ));
        }
    }
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if thresholds.is_empty() {
        thresholds.push(0.0);
    }
    Ok(thresholds)
}

pub(crate) fn root(
    cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
) -> Result<JsValue, JsThrow> {
    cx.opt_node_to_js(this.root)
}

pub(crate) fn root_margin(
    _cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
) -> Result<String, JsThrow> {
    let m = &this.root_margin;
    Ok(format!("{} {} {} {}", m[0], m[1], m[2], m[3]))
}

pub(crate) fn thresholds(
    cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
) -> Result<JsValue, JsThrow> {
    let items: Vec<JsValue> = this
        .thresholds
        .iter()
        .copied()
        .map(JsValue::Number)
        .collect();
    let array = cx.scope.new_array(&items).map_err(JsThrow::from)?;
    cx.freeze(&JsValue::Object(array))
}

pub(crate) fn observe(
    cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
    target: NodeId,
) -> Result<(), JsThrow> {
    let mut targets = this.targets.borrow_mut();
    // Observing an already-observed target is a no-op.
    if targets.iter().any(|t| t.node == target) {
        return Ok(());
    }
    targets.push(IoTarget {
        node: target,
        last: Cell::new(None),
        initial_pending: Cell::new(true),
    });
    // Force one delivery pass even if the layout gate is unchanged.
    cx.state.obs_dirty.set(true);
    Ok(())
}

pub(crate) fn unobserve(
    _cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
    target: NodeId,
) -> Result<(), JsThrow> {
    this.targets.borrow_mut().retain(|t| t.node != target);
    Ok(())
}

pub(crate) fn disconnect(
    _cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
) -> Result<(), JsThrow> {
    this.targets.borrow_mut().clear();
    Ok(())
}

pub(crate) fn take_records(
    cx: &BindCx<'_>,
    this: Rc<IntersectionObserverData>,
) -> Result<JsValue, JsThrow> {
    crate::io_take_records(cx, &this)
}
