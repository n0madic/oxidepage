//! Bounded, owned snapshots of JS values, for the console stream.
//!
//! **Why eager and owned.** The obvious implementation keeps the `JsValue`s
//! and renders them when the embedder drains the stream. It cannot work here:
//! a `JsObject` must be dropped before its realm is torn down, the console
//! stream survives navigation, and a retained DOM wrapper would pin a node of
//! a document that no longer exists. Snapshotting is also the only way to show
//! what the argument *was* — script routinely mutates an object on the line
//! after logging it.
//!
//! **Why here and not in `oxidepage-js`.** The encoder has to recognise DOM
//! wrappers and name them (`<div id="app">`) instead of walking a whole
//! document, and that knowledge — the `TAG_NODE` payload and the tree — lives
//! in this crate. `oxidepage-js` stays the narrow engine abstraction.
//!
//! Everything the encoder needs from the engine beyond that is the primitives
//! added for it: `value_kind`, `own_enumerable_keys` and `symbol_description`
//! (ADR-0025, "Three new engine primitives").

use std::fmt::Write as _;

use oxidepage_base::NodeId;
use oxidepage_dom::DomTree;
use oxidepage_dom::node::NodeData;
use oxidepage_js::{JsObject, JsValue, PromiseState, StackFrame, ValueKind};

use crate::cx::{BindCx, unpack_node};
use crate::state::TAG_NODE;

/// Levels of nesting the encoder descends before eliding.
pub const PREVIEW_MAX_DEPTH: usize = 4;
/// Array items / object entries kept per level.
pub const PREVIEW_MAX_ENTRIES: usize = 100;
/// Characters kept of one string.
pub const PREVIEW_MAX_STRING: usize = 8192;
/// Total nodes encoded across one argument's whole tree.
///
/// The per-level caps alone do **not** bound the work: depth 4 × 100 entries
/// is 10^8 property reads, and cycle detection only rejects a back-edge on the
/// current *path*, so a shallow graph whose nodes point at each other is
/// re-walked exponentially. Nothing rescues that from outside either — the
/// `ScriptBudget` is enforced through the engine's interrupt callback, which
/// plain data-property reads never reach. This is the budget that makes the
/// encoder's cost flat.
pub const PREVIEW_MAX_NODES: usize = 2048;

/// An owned, bounded, cycle-safe snapshot of one JS value.
#[derive(Clone, Debug, PartialEq)]
pub enum ValuePreview {
    Undefined,
    Null,
    Bool(bool),
    /// `NaN`, `±Infinity` and `-0.0` all survive.
    Number(f64),
    /// Decimal digits, without the `n` suffix: Rust has no bigint here.
    BigInt(String),
    String {
        value: String,
        /// True when [`PREVIEW_MAX_STRING`] cut it.
        truncated: bool,
    },
    /// A symbol's description. Its own variant because `ToString` on a symbol
    /// *throws* — which is why the pre-ADR-0025 console printed
    /// `<unprintable>`.
    ///
    /// `Symbol()` and `Symbol("")` both give the empty string here, so both
    /// render `Symbol()`, as every browser console does. The engine primitive
    /// (`JsScope::symbol_description`) keeps the two apart for callers that
    /// need the distinction; the preview deliberately does not.
    Symbol(String),
    /// `name` is empty for an anonymous function.
    Function {
        name: String,
    },
    Array {
        items: Vec<ValuePreview>,
        /// The array's length before [`PREVIEW_MAX_ENTRIES`] truncation.
        length: usize,
        truncated: bool,
    },
    Object {
        /// `constructor.name`; empty for a null-prototype object.
        class: String,
        entries: Vec<(String, ValuePreview)>,
        truncated: bool,
        /// The value's own `toString`, when it has a meaningful one — a
        /// `Date`, a `RegExp`, a `URL`. Their content lives in internal slots,
        /// not in enumerable properties, so without this they would preview as
        /// an empty object and say *less* than the string rendering they
        /// replaced. `None` when `toString` is `Object.prototype`'s (an
        /// uninformative `[object X]`) or throws.
        description: Option<String>,
    },
    /// An `Error`, whose useful properties are all non-enumerable.
    Error {
        name: String,
        message: String,
        stack: Vec<StackFrame>,
    },
    Promise {
        state: PromiseState,
    },
    /// A DOM wrapper, *named* rather than walked: a node's own enumerable
    /// properties are uninteresting and its graph is the whole document.
    ///
    /// No `NodeId`: an id in a stream the embedder drains later has no
    /// re-validation boundary to be checked at. The generation check happens
    /// here, at encode time, inside the task that logged it.
    Node {
        /// The node name (`"DIV"`, `"#text"`).
        name: String,
        /// The open tag with its attributes, or the node name for non-elements.
        description: String,
    },
    /// A reference back to an object already on the encoding path.
    Cyclic,
    /// [`PREVIEW_MAX_DEPTH`] cut the walk here.
    Elided,
    /// Reading the value ran page code that threw (a getter, a proxy trap).
    Threw {
        message: String,
    },
}

/// Snapshots `value`.
///
/// Reading object properties invokes getters and proxy traps — page code —
/// exactly as the old `coerce_string` path did. It runs under the armed
/// `ScriptBudget`, and a throw is contained as [`ValuePreview::Threw`] rather
/// than turned into a page error: `console.log` must not fail.
pub(crate) fn encode(cx: &BindCx<'_>, value: &JsValue) -> ValuePreview {
    let mut walk = Walk {
        path: Vec::new(),
        budget: PREVIEW_MAX_NODES,
    };
    encode_at(cx, value, 0, &mut walk)
}

/// Encoder state threaded through the recursion: the objects on the current
/// path (for cycle detection) and the shared node budget.
struct Walk {
    path: Vec<JsObject>,
    budget: usize,
}

fn encode_at(cx: &BindCx<'_>, value: &JsValue, depth: usize, walk: &mut Walk) -> ValuePreview {
    let scope = cx.scope;
    if walk.budget == 0 {
        return ValuePreview::Elided;
    }
    walk.budget -= 1;
    // Asked once: it is an engine round trip, and the three stages below
    // (primitive, identity, container) all branch on the same answer.
    let kind = scope.value_kind(value);
    match kind {
        ValueKind::Undefined => return ValuePreview::Undefined,
        ValueKind::Null => return ValuePreview::Null,
        ValueKind::Bool => return ValuePreview::Bool(value.truthy()),
        ValueKind::Number => return ValuePreview::Number(value.as_number().unwrap_or(f64::NAN)),
        ValueKind::String => {
            let text = value.as_str().unwrap_or_default();
            return truncated_string(text);
        }
        ValueKind::BigInt => {
            // `ToString` on a BigInt is defined and exact, unlike on a symbol.
            return match scope.coerce_string(value) {
                Ok(digits) => ValuePreview::BigInt(digits),
                Err(error) => ValuePreview::Threw {
                    message: error.to_string(),
                },
            };
        }
        ValueKind::Symbol => {
            return ValuePreview::Symbol(scope.symbol_description(value).unwrap_or_default());
        }
        _ => {}
    }
    let Some(object) = value.as_object() else {
        // Every remaining kind has object identity, so this is unreachable in
        // practice; degrade rather than panic.
        return ValuePreview::Elided;
    };

    // A DOM wrapper is named, not walked — and named from the payload that is
    // already on the wrapper, never by minting one.
    if let Some((TAG_NODE, data)) = scope.host_payload(value)
        && let Some(id) = unpack_node(data)
        && let Some(node) = describe_node(&cx.state.dom.borrow(), id)
    {
        return node;
    }

    if walk
        .path
        .iter()
        .any(|seen| scope.strict_equals(&JsValue::Object(seen.clone()), value))
    {
        return ValuePreview::Cyclic;
    }

    match kind {
        ValueKind::Function => {
            let name = scope
                .get(object, "name")
                .ok()
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
                .unwrap_or_default();
            return ValuePreview::Function { name };
        }
        ValueKind::Error => {
            let text = |key: &str| {
                scope
                    .get(object, key)
                    .ok()
                    .and_then(|v| scope.coerce_string(&v).ok())
                    .unwrap_or_default()
            };
            return ValuePreview::Error {
                name: text("name"),
                message: text("message"),
                stack: scope
                    .get(object, "stack")
                    .ok()
                    .and_then(|v| v.as_str().map(oxidepage_js::parse_stack))
                    .unwrap_or_default(),
            };
        }
        ValueKind::Promise => {
            return ValuePreview::Promise {
                state: scope.promise_state(value).unwrap_or(PromiseState::Pending),
            };
        }
        _ => {}
    }

    // Beyond the depth cap the shape is reported, not the contents.
    if depth >= PREVIEW_MAX_DEPTH {
        return ValuePreview::Elided;
    }

    walk.path.push(object.clone());
    let preview = match kind {
        ValueKind::Array => encode_array(cx, object, depth, walk),
        _ => encode_object(cx, value, object, depth, walk),
    };
    walk.path.pop();
    preview
}

fn encode_array(cx: &BindCx<'_>, object: &JsObject, depth: usize, walk: &mut Walk) -> ValuePreview {
    let scope = cx.scope;
    let length = scope.array_length(object).unwrap_or(0);
    let cap = length.min(PREVIEW_MAX_ENTRIES);
    let mut items = Vec::with_capacity(cap.min(walk.budget));
    for index in 0..cap {
        // Stop when the shared budget runs out rather than padding the rest
        // with `Elided`: the retained tree must be bounded too, not only the
        // work that built it.
        if walk.budget == 0 {
            break;
        }
        items.push(match scope.array_get(object, index) {
            Ok(item) => encode_at(cx, &item, depth + 1, walk),
            Err(error) => ValuePreview::Threw {
                message: error.to_string(),
            },
        });
    }
    let truncated = length > items.len();
    ValuePreview::Array {
        items,
        length,
        truncated,
    }
}

fn encode_object(
    cx: &BindCx<'_>,
    value: &JsValue,
    object: &JsObject,
    depth: usize,
    walk: &mut Walk,
) -> ValuePreview {
    let scope = cx.scope;
    let class = scope
        .get(object, "constructor")
        .ok()
        .and_then(|ctor| ctor.as_object().and_then(|c| scope.get(c, "name").ok()))
        .and_then(|name| name.as_str().map(ToOwned::to_owned))
        .unwrap_or_default();
    let kept_cap = PREVIEW_MAX_ENTRIES.min(walk.budget);
    let (keys, total) = scope
        .own_enumerable_keys(object, kept_cap)
        .unwrap_or_default();
    let mut entries = Vec::with_capacity(keys.len().min(walk.budget));
    for key in &keys {
        // As in `encode_array`: stop, do not pad.
        if walk.budget == 0 {
            break;
        }
        let value = match scope.get(object, key) {
            Ok(value) => encode_at(cx, &value, depth + 1, walk),
            Err(error) => ValuePreview::Threw {
                message: error.to_string(),
            },
        };
        entries.push((key.clone(), value));
    }
    let truncated = total > entries.len();
    ValuePreview::Object {
        class,
        entries,
        truncated,
        description: own_description(cx, value),
    }
}

/// The value's own `toString`, when it says more than `Object.prototype`'s
/// does.
///
/// `Date`, `RegExp` and `URL` keep their content in internal slots, so an
/// enumerable-property walk sees nothing at all. Filtering on the
/// `[object X]` shape is what separates "this type has a real string form"
/// from "this is the default that tells you only what the class already did".
fn own_description(cx: &BindCx<'_>, value: &JsValue) -> Option<String> {
    let text = cx.scope.coerce_string(value).ok()?;
    if text.starts_with("[object ") || text.is_empty() {
        return None;
    }
    Some(match text.char_indices().nth(PREVIEW_MAX_STRING) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    })
}

fn truncated_string(text: &str) -> ValuePreview {
    match text.char_indices().nth(PREVIEW_MAX_STRING) {
        Some((at, _)) => ValuePreview::String {
            value: text[..at].to_owned(),
            truncated: true,
        },
        None => ValuePreview::String {
            value: text.to_owned(),
            truncated: false,
        },
    }
}

/// Names a node the way devtools does: the open tag for an element, the node
/// name for everything else. `None` when the id is stale, so the caller falls
/// back to describing the wrapper as an ordinary object.
fn describe_node(dom: &DomTree, id: NodeId) -> Option<ValuePreview> {
    let node = dom.get(id)?;
    let name = match node.data() {
        NodeData::Element(el) => {
            let name = crate::imp::names::qualified_name(&el.name);
            if el.is_html_element() {
                name.to_ascii_uppercase()
            } else {
                name
            }
        }
        NodeData::Text(_) => "#text".to_owned(),
        NodeData::CdataSection(_) => "#cdata-section".to_owned(),
        NodeData::Comment(_) => "#comment".to_owned(),
        NodeData::Document(_) => "#document".to_owned(),
        NodeData::DocumentFragment { .. } => "#document-fragment".to_owned(),
        NodeData::Doctype { name, .. } => name.to_string(),
        NodeData::ProcessingInstruction { target, .. } => target.to_string(),
    };
    let description = match node.data() {
        NodeData::Element(el) => {
            let mut out = format!("<{}", crate::imp::names::qualified_name(&el.name));
            for attr in el.attrs() {
                let _ = write!(
                    out,
                    " {}=\"{}\"",
                    crate::imp::names::qualified_name(&attr.name),
                    attr.value.replace('"', "&quot;")
                );
            }
            out.push('>');
            out
        }
        _ => match node.character_data() {
            Some(data) => format!("{name} {:?}", elide(data)),
            None => name.clone(),
        },
    };
    Some(ValuePreview::Node { name, description })
}

/// Shortens a character-data snippet for a one-line description.
fn elide(text: &str) -> String {
    const MAX: usize = 40;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

// === Rendering ===

/// Renders a preview the way a console line shows it. A top-level string is
/// *unquoted* (`console.log("a")` prints `a`); a nested one is quoted.
#[must_use]
pub fn render_top(preview: &ValuePreview) -> String {
    match preview {
        ValuePreview::String { value, truncated } => {
            let mut out = value.clone();
            if *truncated {
                out.push('…');
            }
            out
        }
        other => render(other),
    }
}

/// Renders a preview as it appears nested inside another value.
#[must_use]
pub fn render(preview: &ValuePreview) -> String {
    let mut out = String::new();
    write_preview(&mut out, preview);
    out
}

fn write_preview(out: &mut String, preview: &ValuePreview) {
    match preview {
        ValuePreview::Undefined => out.push_str("undefined"),
        ValuePreview::Null => out.push_str("null"),
        ValuePreview::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ValuePreview::Number(n) => out.push_str(&format_number(*n)),
        ValuePreview::BigInt(digits) => {
            let _ = write!(out, "{digits}n");
        }
        ValuePreview::String { value, truncated } => {
            let _ = write!(
                out,
                "\"{}\"",
                value.replace('\\', "\\\\").replace('"', "\\\"")
            );
            if *truncated {
                out.push('…');
            }
        }
        ValuePreview::Symbol(description) => {
            let _ = write!(out, "Symbol({description})");
        }
        ValuePreview::Function { name } => {
            let _ = write!(out, "function {name}()");
        }
        ValuePreview::Array {
            items,
            length,
            truncated,
        } => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_preview(out, item);
            }
            if *truncated {
                let _ = write!(out, ", … {} more", length - items.len());
            }
            out.push(']');
        }
        ValuePreview::Object {
            class,
            entries,
            truncated,
            description,
        } => {
            if !class.is_empty() && class != "Object" {
                let _ = write!(out, "{class} ");
            }
            // A `Date`/`RegExp`/`URL` is its string form; an empty `{}` after
            // it would be noise, since it has no enumerable properties to show.
            if let Some(description) = description {
                out.push_str(description);
                if entries.is_empty() {
                    return;
                }
                out.push(' ');
            }
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{key}: ");
                write_preview(out, value);
            }
            if *truncated {
                out.push_str(", …");
            }
            out.push('}');
        }
        ValuePreview::Error {
            name,
            message,
            stack: _,
        } => {
            if name.is_empty() {
                out.push_str(message);
            } else {
                let _ = write!(out, "{name}: {message}");
            }
        }
        ValuePreview::Promise { state } => {
            let _ = write!(
                out,
                "Promise {{ <{}> }}",
                match state {
                    PromiseState::Pending => "pending",
                    PromiseState::Fulfilled => "fulfilled",
                    PromiseState::Rejected => "rejected",
                }
            );
        }
        ValuePreview::Node { description, .. } => out.push_str(description),
        ValuePreview::Cyclic => out.push_str("[Circular]"),
        ValuePreview::Elided => out.push('…'),
        ValuePreview::Threw { message } => {
            let _ = write!(out, "<threw: {message}>");
        }
    }
}

/// ECMAScript `Number::toString`, near enough for a console line: shortest
/// round-tripping digits, `Infinity`/`NaN` spelled JS's way, and the
/// exponential form outside JS's `[1e-6, 1e21)` fixed-notation window.
#[must_use]
pub fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_owned();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if n == 0.0 {
        // `String(-0)` is `"0"`, but every console shows `-0`: the sign is the
        // whole reason someone is looking.
        return if n.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let abs = n.abs();
    if !(1e-6..1e21).contains(&abs) {
        let rendered = format!("{n:e}");
        return match rendered.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => rendered,
        };
    }
    format!("{n}")
}

// === The console spec's Formatter ===

/// Applies console's format specifiers when `args[0]` is a string containing
/// any, and joins the result with the arguments the specifiers did not
/// consume.
///
/// `%c` consumes its argument and emits nothing: there is no styling in a
/// headless console, and leaving the CSS in the line would be worse than
/// dropping it.
#[must_use]
pub fn format_message(args: &[ValuePreview]) -> String {
    let (format, rest) = match args.split_first() {
        // Only with *more than one* argument, per the console spec's Logger:
        // "if rest is empty, perform Printer(logLevel, « first »)". A lone
        // `console.log("100%% sure")` keeps its two percent signs rather than
        // being silently rewritten.
        Some((ValuePreview::String { value, .. }, rest))
            if !rest.is_empty() && has_specifier(value) =>
        {
            (value, rest)
        }
        _ => {
            return args.iter().map(render_top).collect::<Vec<_>>().join(" ");
        }
    };

    let mut out = String::with_capacity(format.len());
    let mut next = 0;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(&spec) = chars.peek() else {
            out.push('%');
            break;
        };
        if spec == '%' {
            chars.next();
            out.push('%');
            continue;
        }
        if !matches!(spec, 's' | 'd' | 'i' | 'f' | 'o' | 'O' | 'c') {
            out.push('%');
            continue;
        }
        // A specifier with nothing left to substitute stays verbatim, per spec.
        let Some(arg) = rest.get(next) else {
            out.push('%');
            continue;
        };
        chars.next();
        next += 1;
        match spec {
            's' => out.push_str(&render_top(arg)),
            'd' | 'i' => out.push_str(&as_integer(arg)),
            'f' => out.push_str(&as_float(arg)),
            'o' | 'O' => out.push_str(&render(arg)),
            // `%c` is styling; consumed and discarded.
            _ => {}
        }
    }
    for arg in &rest[next.min(rest.len())..] {
        out.push(' ');
        out.push_str(&render_top(arg));
    }
    out
}

fn has_specifier(format: &str) -> bool {
    let bytes = format.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        b == b'%'
            && matches!(
                bytes.get(i + 1),
                Some(b's' | b'd' | b'i' | b'f' | b'o' | b'O' | b'c' | b'%')
            )
    })
}

/// `%d` / `%i`: `ToInteger`, except that a symbol or a BigInt passes through
/// (the spec forbids coercing either).
fn as_integer(arg: &ValuePreview) -> String {
    match arg {
        ValuePreview::Number(n) if n.is_finite() => format_number(n.trunc()),
        ValuePreview::BigInt(digits) => format!("{digits}n"),
        ValuePreview::Symbol(_) => render(arg),
        ValuePreview::Bool(_) | ValuePreview::Null => "0".to_owned(),
        ValuePreview::String { value, .. } => value
            .trim()
            .parse::<f64>()
            .map_or_else(|_| "NaN".to_owned(), |n| format_number(n.trunc())),
        _ => "NaN".to_owned(),
    }
}

/// `%f`: `ToNumber`, with the same symbol/BigInt exception.
fn as_float(arg: &ValuePreview) -> String {
    match arg {
        ValuePreview::Number(n) => format_number(*n),
        ValuePreview::BigInt(digits) => format!("{digits}n"),
        ValuePreview::Symbol(_) => render(arg),
        ValuePreview::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        ValuePreview::Null => "0".to_owned(),
        ValuePreview::String { value, .. } => value
            .trim()
            .parse::<f64>()
            .map_or_else(|_| "NaN".to_owned(), format_number),
        _ => "NaN".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ValuePreview, format_message, format_number, render};

    fn s(value: &str) -> ValuePreview {
        ValuePreview::String {
            value: value.to_owned(),
            truncated: false,
        }
    }

    #[test]
    fn numbers_render_the_way_javascript_writes_them() {
        assert_eq!(format_number(1.0), "1");
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(-0.0), "-0");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(f64::INFINITY), "Infinity");
        assert_eq!(format_number(1e21), "1e+21");
        assert_eq!(format_number(1e-7), "1e-7");
        assert_eq!(format_number(0.000_001), "0.000001");
    }

    #[test]
    fn specifiers_substitute_in_order() {
        let args = [
            s("%s=%d and %f"),
            s("x"),
            ValuePreview::Number(4.7),
            ValuePreview::Number(1.5),
        ];
        assert_eq!(format_message(&args), "x=4 and 1.5");
    }

    #[test]
    fn percent_is_escapable_and_leftovers_are_appended() {
        let args = [s("100%% %s"), s("done"), ValuePreview::Bool(true)];
        assert_eq!(format_message(&args), "100% done true");
    }

    #[test]
    fn a_specifier_with_no_argument_stays_verbatim() {
        assert_eq!(format_message(&[s("%s and %s"), s("one")]), "one and %s");
    }

    #[test]
    fn c_consumes_its_argument_and_emits_nothing() {
        let args = [s("%cstyled"), s("color: red")];
        assert_eq!(format_message(&args), "styled");
    }

    #[test]
    fn o_renders_structurally() {
        let args = [
            s("value: %o"),
            ValuePreview::Array {
                items: vec![ValuePreview::Number(1.0), ValuePreview::Number(2.0)],
                length: 2,
                truncated: false,
            },
        ];
        assert_eq!(format_message(&args), "value: [1, 2]");
    }

    #[test]
    fn a_plain_first_string_is_not_a_format_string() {
        assert_eq!(format_message(&[s("50% off"), s("today")]), "50% off today");
    }

    #[test]
    fn objects_render_as_their_contents() {
        let preview = ValuePreview::Object {
            class: "Object".to_owned(),
            entries: vec![("a".to_owned(), ValuePreview::Number(1.0))],
            truncated: false,
            description: None,
        };
        assert_eq!(render(&preview), "{a: 1}");
    }
}
