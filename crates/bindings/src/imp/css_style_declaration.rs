//! `CSSStyleDeclaration` implementation: `element.style` (inline, writable),
//! `getComputedStyle()` results (computed, read-only), and `CSSStyleRule.style`
//! (a rule's locked declaration block, writable).
//!
//! Inline declarations are backed by the element's `style` attribute — reads
//! parse it, writes serialize back through the normal attribute-mutation path,
//! so snapshots and restyle happen for free. Rule declarations mutate the
//! locked block and notify the stylist. Computed declarations resolve the
//! cascade on demand and expose the longhand set (design doc §10, ADR-0005).

use std::rc::Rc;

use oxidepage_base::{DomExceptionKind, NodeId};
use oxidepage_dom::node::attr_name;
use oxidepage_dom::{LocalName, StyleUpdate};
use oxidepage_js::{JsThrow, JsValue};
use oxidepage_style::cssom;
use oxidepage_style::{computed_style_for, longhand_names_sorted, serialize_property};
use std::cell::RefCell;

use style::properties::PropertyDeclarationBlock;
use style::shared_lock::SharedRwLock;
use style::stylesheets::{CssRule, UrlExtraData};

use crate::cssdata::{ComputedCache, InlineCache, RuleData, StyleDeclData, style_attr_name};
use crate::cx::BindCx;

/// The shared lock plus the locked declaration block of a `CSSStyleRule.style`
/// view, resolved live from the rule (not stored on the view).
fn rule_lock_block(cx: &BindCx<'_>, rule: &CssRule) -> Option<(SharedRwLock, cssom::LockedBlock)> {
    let lock = cx.style_lock();
    let block = cssom::style_rule_block(&lock, rule)?;
    Some((lock, block))
}

/// The element's current `style` attribute text and the document's URL data.
fn inline_source(cx: &BindCx<'_>, element: NodeId) -> (String, UrlExtraData) {
    let dom = cx.state.dom.borrow();
    let css = dom
        .get(element)
        .and_then(|n| n.as_element())
        .and_then(|el| el.attr(&style_attr_name()))
        .map(|v| v.to_string())
        .unwrap_or_default();
    (css, dom.url_extra_data().clone())
}

/// Parses the inline declaration block from the element's `style` attribute
/// (fresh; used by the write paths, which need an owned mutable block).
fn inline_block(cx: &BindCx<'_>, element: NodeId) -> PropertyDeclarationBlock {
    let (css, url) = inline_source(cx, element);
    cssom::parse_inline_block(&css, &url)
}

/// The inline block for a read, reusing the cached parse when the `style`
/// attribute text is unchanged.
fn inline_block_cached(
    cx: &BindCx<'_>,
    element: NodeId,
    cache: &InlineCache,
) -> Rc<PropertyDeclarationBlock> {
    let (css, url) = inline_source(cx, element);
    if let Some((cached_css, block)) = &*cache.borrow()
        && *cached_css == css
    {
        return Rc::clone(block);
    }
    let block = Rc::new(cssom::parse_inline_block(&css, &url));
    *cache.borrow_mut() = Some((css, Rc::clone(&block)));
    block
}

/// Writes a declaration block back to the element's `style` attribute (drives
/// the standard mutation path: snapshot + restyle).
fn write_inline(cx: &BindCx<'_>, element: NodeId, block: &PropertyDeclarationBlock) {
    let text = cssom::block_to_css(block);
    cx.state
        .dom
        .borrow_mut()
        .set_attribute(element, style_attr_name(), text.into());
}

/// Applies pending inline `<style>` updates so a `getComputedStyle` read
/// synchronously reflects sheets added earlier in the same script (a style
/// flush, as browsers do). `<link>` loads need the network and stay queued for
/// the page event loop; a dynamically-added inline `@import` is likewise left
/// for the loop (no blocking fetch on this path).
pub(crate) fn flush_inline_styles(cx: &BindCx<'_>) {
    let updates = cx.state.dom.borrow_mut().take_style_updates();
    for update in updates {
        match update {
            StyleUpdate::StyleElement(node) => apply_inline_sheet(cx, node),
            StyleUpdate::StyleElementRemoved(node) | StyleUpdate::LinkElementRemoved(node) => {
                // The engine of the frame that renders this node's document,
                // not the accessing realm's: a `<style>` in an iframe belongs
                // to the iframe (ADR-0035 D1).
                cx.frame_for(node)
                    .style
                    .borrow_mut()
                    .remove_sheet_for_node(node);
            }
            link @ StyleUpdate::LinkElement(_) => {
                cx.state.dom.borrow_mut().push_style_update(link);
            }
        }
    }
}

/// Builds and installs the stylesheet for a connected `<style>` node from its
/// text and `media` attribute (no `@import` loader on this synchronous path).
fn apply_inline_sheet(cx: &BindCx<'_>, node: NodeId) {
    let (css, media, url) = {
        let dom = cx.state.dom.borrow();
        if !dom.node(node).is_connected() {
            return;
        }
        let media = dom
            .node(node)
            .as_element()
            .and_then(|el| el.attr(&attr_name(LocalName::from("media"))))
            .map(|v| v.to_string());
        (
            dom.text_content(node),
            media,
            dom.url_extra_data_of_node(node).clone(),
        )
    };
    // The node's own frame's engine — see `flush_inline_styles`.
    let style = &cx.frame_for(node).style;
    let sheet = style
        .borrow()
        .make_stylesheet_with_loader(&css, &url, media.as_deref(), None);
    let dom = cx.state.dom.borrow();
    style.borrow_mut().add_sheet_for_node(&dom, node, sheet);
}

/// Serializes a used length as CSS px (browsers report fractional px for
/// used values; trim float noise to 3 decimals).
fn used_px(value: f32) -> String {
    let rounded = (f64::from(value) * 1000.0).round() / 1000.0;
    format!("{rounded}px")
}

/// CSSOM resolved values (Phase 5): for the layout-backed property set,
/// `getComputedStyle` reports the **used** value when the element generates
/// a box (ADR-0006 §9). Elements without boxes (e.g. `display: none`) fall
/// back to the computed value, per spec.
fn resolved_layout_value(cx: &BindCx<'_>, element: NodeId, name: &str) -> Option<String> {
    let is_inset = matches!(name, "top" | "right" | "bottom" | "left");
    let is_layout_backed = is_inset
        || matches!(
            name,
            "width"
                | "height"
                | "margin-top"
                | "margin-right"
                | "margin-bottom"
                | "margin-left"
                | "padding-top"
                | "padding-right"
                | "padding-bottom"
                | "padding-left"
        );
    if !is_layout_backed {
        return None;
    }

    let used = crate::imp::geometry_support::flush_layout(cx, element, |_, layout| {
        layout.used_box_values(element)
    })?;

    // An inset with no used value reports the computed one: a static box (the
    // property does not apply), or an `auto` inset on a sticky box. `used_insets`
    // decides which; `None` here means "fall back to the computed value".
    if is_inset {
        let index = match name {
            "top" => 0,
            "right" => 1,
            "bottom" => 2,
            "left" => 3,
            _ => unreachable!("checked by `is_inset`"),
        };
        return used.inset[index].map(used_px);
    }

    Some(match name {
        "width" => used_px(used.width),
        "height" => used_px(used.height),
        "margin-top" => used_px(used.margin[0]),
        "margin-right" => used_px(used.margin[1]),
        "margin-bottom" => used_px(used.margin[2]),
        "margin-left" => used_px(used.margin[3]),
        "padding-top" => used_px(used.padding[0]),
        "padding-right" => used_px(used.padding[1]),
        "padding-bottom" => used_px(used.padding[2]),
        "padding-left" => used_px(used.padding[3]),
        _ => unreachable!("insets returned above; the rest is checked by `is_layout_backed`"),
    })
}

/// A computed value serialized for `getComputedStyle` reads. The resolved
/// `ComputedValues` are cached on the view and reused until the author-style
/// (`engine.version`) or DOM (`style_version`) counters change, so reading many
/// properties off one `getComputedStyle` object resolves the cascade once.
fn computed_value(
    cx: &BindCx<'_>,
    element: NodeId,
    pseudo: Option<style::selector_parser::PseudoElement>,
    cache: &ComputedCache,
    name: &str,
) -> String {
    flush_inline_styles(cx);
    if pseudo.is_none()
        && let Some(resolved) = resolved_layout_value(cx, element, name)
    {
        return resolved;
    }
    // The element's *own* frame's author styles. Reading the accessing realm's
    // made `getComputedStyle(iframe.contentDocument.querySelector(…))` resolve
    // the cascade against the embedder's stylist, which has none of the frame's
    // sheets — it happened to agree whenever the frame's own reflow had already
    // cached the answer on the node, and disagreed the moment a sheet arrived
    // after that (ADR-0035 D1).
    let style = &cx.frame_for(element).style;
    let engine_version = style.borrow().version();
    let dom_version = cx.state.dom.borrow().style_version();
    if let Some((ev, dv, cv)) = &*cache.borrow()
        && *ev == engine_version
        && *dv == dom_version
    {
        return serialize_property(cv, name);
    }
    let resolved = {
        let mut engine = style.borrow_mut();
        let mut dom = cx.state.dom.borrow_mut();
        computed_style_for(&mut engine, &mut dom, element, pseudo)
    };
    match resolved {
        Some(cv) => {
            let out = serialize_property(&cv, name);
            *cache.borrow_mut() = Some((engine_version, dom_version, cv));
            out
        }
        None => String::new(),
    }
}

/// Notifies the stylist that a rule's declaration block changed in place,
/// resolving the current sheet from the rule's owner node.
fn note_rule_changed(cx: &BindCx<'_>, owner: NodeId, rule: &CssRule) {
    if let Some(sheet) = cx.sheet_for(owner) {
        cx.state
            .style
            .borrow_mut()
            .note_style_rule_declarations_changed(&sheet, rule);
    }
}

fn read_only_error(cx: &BindCx<'_>) -> JsThrow {
    cx.dom_throw(
        DomExceptionKind::NoModificationAllowedError,
        "computed style declarations are read-only",
    )
}

pub(crate) fn css_text(cx: &BindCx<'_>, this: Rc<StyleDeclData>) -> Result<String, JsThrow> {
    Ok(match &*this {
        StyleDeclData::Inline { element, block } => {
            cssom::block_to_css(&inline_block_cached(cx, *element, block))
        }
        StyleDeclData::Rule { rule, .. } => rule_lock_block(cx, rule)
            .map(|(lock, block)| cssom::locked_block_to_css(&lock, &block))
            .unwrap_or_default(),
        // Computed declarations serialize cssText to "" (CSSOM).
        StyleDeclData::Computed { .. } => String::new(),
    })
}

pub(crate) fn set_css_text(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    value: String,
) -> Result<(), JsThrow> {
    match &*this {
        StyleDeclData::Inline { element, .. } => {
            cx.state
                .dom
                .borrow_mut()
                .set_attribute(*element, style_attr_name(), value.into());
            Ok(())
        }
        StyleDeclData::Rule { owner, rule } => {
            if let Some((lock, block)) = rule_lock_block(cx, rule) {
                cssom::locked_block_set_text(&lock, &block, &value, &cx.doc_url());
                note_rule_changed(cx, *owner, rule);
            }
            Ok(())
        }
        StyleDeclData::Computed { .. } => Err(read_only_error(cx)),
    }
}

pub(crate) fn length(cx: &BindCx<'_>, this: Rc<StyleDeclData>) -> Result<f64, JsThrow> {
    let n = match &*this {
        StyleDeclData::Inline { element, block } => {
            cssom::block_names(&inline_block_cached(cx, *element, block)).len()
        }
        StyleDeclData::Rule { rule, .. } => rule_lock_block(cx, rule)
            .map(|(lock, block)| cssom::locked_block_names(&lock, &block).len())
            .unwrap_or(0),
        StyleDeclData::Computed { .. } => longhand_names_sorted().len(),
    };
    Ok(n as f64)
}

pub(crate) fn item(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    index: u32,
) -> Result<String, JsThrow> {
    let i = index as usize;
    let name = match &*this {
        StyleDeclData::Inline { element, block } => {
            cssom::block_names(&inline_block_cached(cx, *element, block))
                .get(i)
                .cloned()
        }
        StyleDeclData::Rule { rule, .. } => rule_lock_block(cx, rule)
            .and_then(|(lock, block)| cssom::locked_block_names(&lock, &block).get(i).cloned()),
        StyleDeclData::Computed { .. } => longhand_names_sorted().get(i).map(|s| (*s).to_owned()),
    };
    Ok(name.unwrap_or_default())
}

pub(crate) fn get_property_value(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    property: String,
) -> Result<String, JsThrow> {
    Ok(match &*this {
        StyleDeclData::Inline { element, block } => {
            cssom::block_get(&inline_block_cached(cx, *element, block), &property)
        }
        StyleDeclData::Rule { rule, .. } => rule_lock_block(cx, rule)
            .map(|(lock, block)| cssom::locked_block_get(&lock, &block, &property))
            .unwrap_or_default(),
        StyleDeclData::Computed {
            element,
            pseudo,
            cache,
        } => computed_value(cx, *element, *pseudo, cache, &property),
    })
}

pub(crate) fn get_property_priority(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    property: String,
) -> Result<String, JsThrow> {
    let important = match &*this {
        StyleDeclData::Inline { element, block } => {
            cssom::block_is_important(&inline_block_cached(cx, *element, block), &property)
        }
        StyleDeclData::Rule { rule, .. } => {
            rule_lock_block(cx, rule).is_some_and(|(lock, block)| {
                cssom::locked_block_is_important(&lock, &block, &property)
            })
        }
        // Computed values carry no priority.
        StyleDeclData::Computed { .. } => false,
    };
    Ok(if important { "important" } else { "" }.to_owned())
}

pub(crate) fn set_property(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    property: String,
    value: String,
    priority: String,
) -> Result<(), JsThrow> {
    // Computed declarations are read-only regardless of arguments.
    if matches!(&*this, StyleDeclData::Computed { .. }) {
        return Err(read_only_error(cx));
    }
    // CSSOM setProperty step 3: an empty value removes the property — and this
    // happens *before* the priority validation of step 4.
    if value.is_empty() {
        remove_property(cx, this, property)?;
        return Ok(());
    }
    // Step 4: priority must be "" or an ASCII case-insensitive "important".
    let important = if priority.is_empty() {
        false
    } else if priority.eq_ignore_ascii_case("important") {
        true
    } else {
        return Ok(());
    };
    match &*this {
        StyleDeclData::Inline { element, .. } => {
            let (css, url) = inline_source(cx, *element);
            let mut block = cssom::parse_inline_block(&css, &url);
            cssom::block_set(&mut block, &property, &value, important, &url);
            write_inline(cx, *element, &block);
            Ok(())
        }
        StyleDeclData::Rule { owner, rule } => {
            if let Some((lock, block)) = rule_lock_block(cx, rule) {
                cssom::locked_block_set(&lock, &block, &property, &value, important, &cx.doc_url());
                note_rule_changed(cx, *owner, rule);
            }
            Ok(())
        }
        StyleDeclData::Computed { .. } => Err(read_only_error(cx)),
    }
}

pub(crate) fn remove_property(
    cx: &BindCx<'_>,
    this: Rc<StyleDeclData>,
    property: String,
) -> Result<String, JsThrow> {
    match &*this {
        StyleDeclData::Inline { element, .. } => {
            let mut block = inline_block(cx, *element);
            let old = cssom::block_remove(&mut block, &property);
            write_inline(cx, *element, &block);
            Ok(old)
        }
        StyleDeclData::Rule { owner, rule } => {
            let old = match rule_lock_block(cx, rule) {
                Some((lock, block)) => {
                    let old = cssom::locked_block_remove(&lock, &block, &property);
                    note_rule_changed(cx, *owner, rule);
                    old
                }
                None => String::new(),
            };
            Ok(old)
        }
        StyleDeclData::Computed { .. } => Err(read_only_error(cx)),
    }
}

pub(crate) fn parent_rule(cx: &BindCx<'_>, this: Rc<StyleDeclData>) -> Result<JsValue, JsThrow> {
    match &*this {
        StyleDeclData::Rule { owner, rule, .. } => cx.new_css_rule(RuleData {
            owner: *owner,
            rule: rule.clone(),
            style: RefCell::new(None),
        }),
        _ => Ok(JsValue::Null),
    }
}
