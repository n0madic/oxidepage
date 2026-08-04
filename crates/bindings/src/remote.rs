//! The remote object model: CDP's `Runtime.RemoteObject`, and the handle table
//! behind it.
//!
//! # Why this lives here and not in the protocol crate
//!
//! An `objectId` names a *live JavaScript value* that must survive between two
//! commands. [`JsValue`] already is that — `JsValue::Object` wraps an
//! `rquickjs::Persistent` — but it is `!Send`, and, more sharply, it **must be
//! dropped before the realm**. That ordering is what `Page`'s field order
//! encodes (`state, hooks, realm, net`), and a store owned by a protocol
//! session on another thread would break it: the realm would go down first and
//! the store's drop would be a use-after-free.
//!
//! So the table lives on [`WorldState`](crate::WorldState), next to
//! `custom_wrappers`, and the protocol layer holds nothing but `u64`s.
//!
//! # What crosses the thread boundary
//!
//! [`RemoteObject`] is a **snapshot**: plain data, `Send`, no `JsValue`
//! anywhere — the same rule `ConsoleMessage` follows. A by-value result travels
//! as JSON *text*, produced by the realm's own `JSON.stringify`, because that
//! is exactly the serialization CDP specifies: cycles throw, `undefined` and
//! functions vanish, `toJSON` is honored. Re-implementing it in Rust would be a
//! second answer to a question the engine already answers.

use std::collections::HashMap;

use oxidepage_js::{JsThrow, JsValue, ValueKind};

use crate::cx::BindCx;

/// Most live handles one page may hold at once.
///
/// A driver that leaks `objectId`s — never calling `releaseObject`, which is
/// easy to do — would otherwise pin every object it ever touched for the life
/// of the document. Past the cap, minting fails rather than growing without
/// bound; the driver sees an error instead of the process seeing an OOM.
pub const MAX_REMOTE_OBJECTS: usize = 10_000;

/// Longest `description` retained for one object.
///
/// A function's description is its source text, and a minified bundle's
/// top-level function is megabytes of it.
pub const MAX_DESCRIPTION: usize = 1024;

struct Entry {
    value: JsValue,
    /// `Runtime.releaseObjectGroup` releases every handle sharing a name.
    group: Option<String>,
}

/// `objectId` → live value, with object groups and explicit release.
#[derive(Default)]
pub struct ObjectStore {
    objects: HashMap<u64, Entry>,
}

impl ObjectStore {
    /// Retains `value`, returning its id.
    ///
    /// Ids are monotonic and never recycled, like
    /// [`Slab`](crate::WorldState)'s: a stale id must name *nothing*, not
    /// whatever now occupies its slot. That is the same reasoning the arena's
    /// generation counters encode for `NodeId`.
    pub fn insert(&mut self, id: u64, value: JsValue, group: Option<String>) -> Option<u64> {
        if self.objects.len() >= MAX_REMOTE_OBJECTS {
            return None;
        }
        self.objects.insert(id, Entry { value, group });
        Some(id)
    }

    /// The ids this store holds, for the page to drop from its index.
    #[must_use]
    pub fn ids(&self) -> Vec<u64> {
        self.objects.keys().copied().collect()
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<JsValue> {
        self.objects.get(&id).map(|entry| entry.value.clone())
    }

    pub fn release(&mut self, id: u64) -> bool {
        self.objects.remove(&id).is_some()
    }

    /// Releases every handle in `group`, returning how many.
    pub fn release_group(&mut self, group: &str) -> usize {
        let before = self.objects.len();
        self.objects
            .retain(|_, entry| entry.group.as_deref() != Some(group));
        before - self.objects.len()
    }

    /// Drops every handle — a new document invalidates all of them.
    pub fn clear(&mut self) -> usize {
        let dropped = self.objects.len();
        self.objects.clear();
        dropped
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

/// CDP's `RemoteObject.type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteType {
    Object,
    Function,
    Undefined,
    String,
    Number,
    Boolean,
    Symbol,
    Bigint,
}

impl RemoteType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteType::Object => "object",
            RemoteType::Function => "function",
            RemoteType::Undefined => "undefined",
            RemoteType::String => "string",
            RemoteType::Number => "number",
            RemoteType::Boolean => "boolean",
            RemoteType::Symbol => "symbol",
            RemoteType::Bigint => "bigint",
        }
    }
}

/// CDP's `RemoteObject.subtype`. Only the ones the engine can actually tell
/// apart are listed — `weakmap`, `generator`, `proxy` and friends have no
/// discriminator here, and guessing would be worse than omitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteSubtype {
    Array,
    Null,
    Node,
    Error,
    Promise,
    Date,
    Regexp,
    Map,
    Set,
}

impl RemoteSubtype {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteSubtype::Array => "array",
            RemoteSubtype::Null => "null",
            RemoteSubtype::Node => "node",
            RemoteSubtype::Error => "error",
            RemoteSubtype::Promise => "promise",
            RemoteSubtype::Date => "date",
            RemoteSubtype::Regexp => "regexp",
            RemoteSubtype::Map => "map",
            RemoteSubtype::Set => "set",
        }
    }
}

/// A `Send` description of one JavaScript value.
///
/// Holds no [`JsValue`]: the live value stays in the [`ObjectStore`], named by
/// `object_id`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteObject {
    pub kind: Option<RemoteType>,
    pub subtype: Option<RemoteSubtype>,
    pub class_name: Option<String>,
    pub description: Option<String>,
    /// JSON *text* for a by-value result, as `JSON.stringify` produced it.
    pub value_json: Option<String>,
    /// Set iff the value was retained in the store.
    pub object_id: Option<u64>,
    /// A primitive that JSON cannot express: `NaN`, `Infinity`, `-Infinity`,
    /// `-0`. CDP calls this `unserializableValue` and drivers reconstruct the
    /// primitive from it, so collapsing them all to `null` would be lossy.
    pub unserializable: Option<String>,
}

impl RemoteObject {
    /// The `RemoteObject` for `undefined` — CDP's shape for "no result".
    #[must_use]
    pub fn undefined() -> Self {
        Self {
            kind: Some(RemoteType::Undefined),
            ..Self::default()
        }
    }
}

/// One entry of `Runtime.getProperties`.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyDescriptor {
    pub name: String,
    pub value: Option<RemoteObject>,
    pub enumerable: bool,
    /// Own properties only are reported today (see
    /// [`describe_properties`]), so this is always true; it is present because
    /// the protocol member is not optional and a driver reads it.
    pub is_own: bool,
}

/// CDP's `ExceptionDetails`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExceptionDetails {
    pub text: String,
    pub line: u32,
    pub column: u32,
    pub url: String,
    pub exception: Option<RemoteObject>,
}

/// The outcome of an evaluation: a value, or a thrown exception.
#[derive(Clone, Debug, PartialEq)]
pub struct EvaluationResult {
    pub result: RemoteObject,
    pub exception: Option<ExceptionDetails>,
}

/// How a value should be handed back.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoteOptions<'a> {
    /// Serialize into the result instead of minting an `objectId`.
    pub by_value: bool,
    /// The release group a minted handle joins.
    pub group: Option<&'a str>,
}

/// Builds the `RemoteObject` for `value`, retaining it if it needs a handle.
pub fn describe(cx: &BindCx<'_>, value: &JsValue, options: RemoteOptions<'_>) -> RemoteObject {
    let scope = cx.scope;
    let kind = scope.value_kind(value);

    // Primitives never get a handle: there is no identity to preserve, and a
    // driver that had to release a number would leak the ones it forgot.
    if let Some(primitive) = describe_primitive(cx, value, kind) {
        return primitive;
    }

    let mut object = RemoteObject {
        kind: Some(match kind {
            ValueKind::Function => RemoteType::Function,
            _ => RemoteType::Object,
        }),
        subtype: subtype_of(cx, value, kind),
        class_name: class_name_of(cx, value),
        description: Some(describe_object(cx, value, kind)),
        ..RemoteObject::default()
    };

    if options.by_value {
        match stringify(cx, value) {
            Ok(json) => object.value_json = json,
            // Cycles and BigInt make `JSON.stringify` throw. Chrome answers
            // "Object couldn't be returned by value"; leaving `value` absent
            // lets the caller say the same thing without inventing a payload.
            Err(_) => object.value_json = None,
        }
        return object;
    }

    // The id is minted page-wide and the handle is filed in **this** world's
    // store, so `Runtime.callFunctionOn` can route back here (ADR-0033 D10).
    let id = cx.state.page.next_object_id();
    object.object_id = cx.state.remote_objects.borrow_mut().insert(
        id,
        value.clone(),
        options.group.map(str::to_owned),
    );
    if object.object_id.is_some() {
        cx.state.page.note_object_world(id, cx.state.id);
    }
    // `None` here means the table is full. Returning a `RemoteObject` with no
    // `objectId` would hand the caller a handle-shaped answer that names
    // nothing, which is exactly what the cap was added to avoid; the caller
    // turns this into `RemoteError::OutOfHandles`.
    object
}

/// The `RemoteObject` for a primitive, or `None` if `value` needs a handle.
fn describe_primitive(cx: &BindCx<'_>, value: &JsValue, kind: ValueKind) -> Option<RemoteObject> {
    let scope = cx.scope;
    match kind {
        ValueKind::Undefined => Some(RemoteObject::undefined()),
        ValueKind::Null => Some(RemoteObject {
            kind: Some(RemoteType::Object),
            subtype: Some(RemoteSubtype::Null),
            value_json: Some(String::from("null")),
            ..RemoteObject::default()
        }),
        ValueKind::Bool => {
            let truthy = value.truthy();
            Some(RemoteObject {
                kind: Some(RemoteType::Boolean),
                description: Some(truthy.to_string()),
                value_json: Some(truthy.to_string()),
                ..RemoteObject::default()
            })
        }
        ValueKind::Number => {
            let number = value.as_number().unwrap_or(f64::NAN);
            let mut object = RemoteObject {
                kind: Some(RemoteType::Number),
                description: Some(number_description(number)),
                ..RemoteObject::default()
            };
            // JSON has no NaN, no infinities and no signed zero, so these
            // travel in `unserializableValue` and `value` stays absent.
            match unserializable_number(number) {
                Some(text) => object.unserializable = Some(text),
                None => object.value_json = Some(number_description(number)),
            }
            Some(object)
        }
        ValueKind::String => {
            let text = value.as_str().map(str::to_owned).unwrap_or_default();
            Some(RemoteObject {
                kind: Some(RemoteType::String),
                description: Some(truncate(&text)),
                value_json: Some(json_string(&text)),
                ..RemoteObject::default()
            })
        }
        ValueKind::Symbol => Some(RemoteObject {
            kind: Some(RemoteType::Symbol),
            description: Some(format!(
                "Symbol({})",
                scope.symbol_description(value).unwrap_or_default()
            )),
            ..RemoteObject::default()
        }),
        ValueKind::BigInt => {
            // `n`-suffixed, which is how CDP spells a BigInt and how a driver
            // reconstructs one.
            let text = format!("{}n", scope.coerce_string(value).unwrap_or_default());
            Some(RemoteObject {
                kind: Some(RemoteType::Bigint),
                description: Some(text.clone()),
                unserializable: Some(text),
                ..RemoteObject::default()
            })
        }
        _ => None,
    }
}

/// `NaN`, `Infinity`, `-Infinity` and `-0` — the primitives JSON cannot carry.
fn unserializable_number(number: f64) -> Option<String> {
    if number.is_nan() {
        return Some(String::from("NaN"));
    }
    if number.is_infinite() {
        return Some(String::from(if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }));
    }
    // `-0.0 == 0.0`, so the sign bit is the only way to tell them apart — and
    // the distinction is observable in JavaScript (`Object.is`, `1/x`).
    (number == 0.0 && number.is_sign_negative()).then(|| String::from("-0"))
}

/// JavaScript's number-to-string, which is not Rust's: an integral `f64` prints
/// without a trailing `.0`, and that is what a driver compares against.
fn number_description(number: f64) -> String {
    if number.is_nan() {
        return String::from("NaN");
    }
    if number.is_infinite() {
        return String::from(if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        });
    }
    if number == number.trunc() && number.abs() < 1e21 {
        // Two integral cases, because neither spelling covers both.
        //
        // Below 2^53 an `i64` is exact and cheap. Above it a float-to-int cast
        // *saturates* in Rust, so `2**64` would print as `i64::MAX`; and Rust's
        // `{}` prints the shortest round-tripping form (`18446744073709552000`)
        // where JavaScript prints the value the double actually holds. `{:.0}`
        // is that exact expansion, which is what a driver compares against.
        if number.abs() < 9_007_199_254_740_992.0 {
            return format!("{}", number as i64);
        }
        return format!("{number:.0}");
    }
    format!("{number}")
}

fn subtype_of(cx: &BindCx<'_>, value: &JsValue, kind: ValueKind) -> Option<RemoteSubtype> {
    // A DOM node is recognized by its host payload, the same way `preview.rs`
    // does it — there is no other reliable discriminator.
    if let Some((tag, _)) = cx.scope.host_payload(value)
        && tag == crate::TAG_NODE
    {
        return Some(RemoteSubtype::Node);
    }
    match kind {
        ValueKind::Array => Some(RemoteSubtype::Array),
        ValueKind::Error => Some(RemoteSubtype::Error),
        ValueKind::Promise => Some(RemoteSubtype::Promise),
        ValueKind::Object => match class_name_of(cx, value).as_deref() {
            Some("Date") => Some(RemoteSubtype::Date),
            Some("RegExp") => Some(RemoteSubtype::Regexp),
            Some("Map") => Some(RemoteSubtype::Map),
            Some("Set") => Some(RemoteSubtype::Set),
            _ => None,
        },
        _ => None,
    }
}

/// `value.constructor.name`, the same two reads `preview.rs` makes.
fn class_name_of(cx: &BindCx<'_>, value: &JsValue) -> Option<String> {
    let object = value.as_object()?;
    let constructor = cx.scope.get(object, "constructor").ok()?;
    let name = cx.scope.get(constructor.as_object()?, "name").ok()?;
    name.as_str().map(str::to_owned).filter(|s| !s.is_empty())
}

fn describe_object(cx: &BindCx<'_>, value: &JsValue, kind: ValueKind) -> String {
    match kind {
        // An array's description carries its length, which is what makes
        // `Array(3)` readable in a driver's log without a round trip.
        ValueKind::Array => format!(
            "Array({})",
            value
                .as_object()
                .and_then(|array| cx.scope.array_length(array).ok())
                .unwrap_or(0)
        ),
        // A function's description is its source; an error's is its stack.
        // Both are `toString`, both can be enormous.
        ValueKind::Function | ValueKind::Error => {
            truncate(&cx.scope.coerce_string(value).unwrap_or_default())
        }
        _ => {
            let rendered = cx.scope.coerce_string(value).unwrap_or_default();
            // `[object Object]` is noise; the class name is the useful answer,
            // exactly as `preview::own_description` decides.
            if rendered.starts_with("[object ") {
                class_name_of(cx, value).unwrap_or_else(|| String::from("Object"))
            } else {
                truncate(&rendered)
            }
        }
    }
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_DESCRIPTION {
        return text.to_owned();
    }
    // Cut on a character boundary: a byte slice through a multi-byte codepoint
    // panics, and `text` is page-controlled.
    let mut end = MAX_DESCRIPTION;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Serializes `value` with the realm's own `JSON.stringify`.
///
/// Deliberately the engine's, not a Rust re-implementation: `toJSON`, property
/// order, cycle detection and `undefined`-elision are all observable behavior
/// that a second implementation would get subtly wrong.
///
/// `Ok(None)` means `stringify` returned `undefined` — a function, a symbol, or
/// plain `undefined` — which CDP reports as a result with no `value`.
fn stringify(cx: &BindCx<'_>, value: &JsValue) -> Result<Option<String>, JsThrow> {
    let scope = cx.scope;
    let global = scope.global();
    let json = scope.get(&global, "JSON")?;
    let json_object = json
        .as_object()
        .ok_or_else(|| JsThrow::Type(String::from("JSON is not an object")))?;
    let stringify = scope.get(json_object, "stringify")?;
    let text = scope.call(&stringify, &json, std::slice::from_ref(value))?;
    Ok(text.as_str().map(str::to_owned))
}

/// A JSON string literal, for the by-value form of a primitive string.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The own enumerable properties of `object_id`'s value.
///
/// Own and enumerable only. The prototype chain is deliberately not walked:
/// both drivers use `getProperties` to read a handle's *data*, the chain of a
/// DOM wrapper is hundreds of accessors deep, and every one of them would have
/// to be retained as a handle of its own.
pub fn describe_properties(
    cx: &BindCx<'_>,
    value: &JsValue,
    limit: usize,
    group: Option<&str>,
) -> Result<Vec<PropertyDescriptor>, JsThrow> {
    let scope = cx.scope;
    let Some(object) = value.as_object() else {
        return Ok(Vec::new());
    };
    let (keys, _truncated) = scope.own_enumerable_keys(object, limit)?;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        // A getter that throws must not fail the whole enumeration: a driver
        // reading an object with one hostile accessor still wants the rest.
        let property = match scope.get(object, &key) {
            Ok(property) => Some(describe(
                cx,
                &property,
                RemoteOptions {
                    by_value: false,
                    group,
                },
            )),
            Err(_) => None,
        };
        out.push(PropertyDescriptor {
            name: key,
            value: property,
            enumerable: true,
            is_own: true,
        });
    }
    Ok(out)
}

/// Turns a `JsError` into CDP's `ExceptionDetails`.
pub fn describe_exception(
    cx: &BindCx<'_>,
    error: &oxidepage_js::JsError,
    group: Option<&str>,
) -> ExceptionDetails {
    let frame = error.stack().first();
    // `JsError::rendered` is message-plus-stack and drops the name, which is
    // the single most useful word in an exception: a driver reporting "boom"
    // instead of "TypeError: boom" has thrown away the classification its user
    // needs. `ScriptError` keeps `name` in its own field for the same reason
    // (ADR-0025); here there is only one text member, so it goes in front.
    let text = match error.name() {
        Some(name) if !name.is_empty() => format!("{name}: {}", error.message()),
        _ => error.rendered(),
    };
    ExceptionDetails {
        text,
        // CDP counts from zero and the engine's frames count from one, which is
        // what every stack trace a human reads uses.
        line: frame.map_or(0, |f| f.line.saturating_sub(1)),
        column: frame.map_or(0, |f| f.column.saturating_sub(1)),
        url: frame.map_or_else(String::new, |f| f.url.clone()),
        exception: match error {
            oxidepage_js::JsError::Exception {
                value: Some(value), ..
            } => Some(describe(
                cx,
                value,
                RemoteOptions {
                    by_value: false,
                    group,
                },
            )),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids come from the page now, so every world's handles share one
    /// monotonic sequence (ADR-0033 D10) — two worlds must never both mint
    /// `1`. The store's job is only to honour the id it is handed.
    #[test]
    fn ids_are_never_recycled() {
        // A stale `objectId` must name nothing, not whatever took its slot —
        // the same rule the arena's generation counters encode for `NodeId`.
        let mut store = ObjectStore::default();
        let first = store.insert(1, JsValue::Null, None).unwrap();
        let second = store.insert(2, JsValue::Null, None).unwrap();
        assert!(second > first);
        assert!(store.release(first));
        let third = store.insert(3, JsValue::Null, None).unwrap();
        assert!(third > second, "a released id must not be handed out again");
        assert!(store.get(first).is_none());
        assert!(!store.release(first), "releasing twice is not an error");
    }

    #[test]
    fn groups_release_together_and_leave_others_alone() {
        let mut store = ObjectStore::default();
        let a = store
            .insert(11, JsValue::Null, Some(String::from("g1")))
            .unwrap();
        let b = store
            .insert(12, JsValue::Null, Some(String::from("g1")))
            .unwrap();
        let c = store
            .insert(13, JsValue::Null, Some(String::from("g2")))
            .unwrap();
        let ungrouped = store.insert(14, JsValue::Null, None).unwrap();

        assert_eq!(store.release_group("g1"), 2);
        assert!(store.get(a).is_none());
        assert!(store.get(b).is_none());
        assert!(store.get(c).is_some());
        assert!(store.get(ungrouped).is_some());
        assert_eq!(store.release_group("nope"), 0);
    }

    #[test]
    fn the_table_is_bounded() {
        // A driver that never releases must hit an error, not an OOM.
        let mut store = ObjectStore::default();
        for id in 1..=MAX_REMOTE_OBJECTS as u64 {
            assert!(store.insert(id, JsValue::Null, None).is_some());
        }
        let past_cap = MAX_REMOTE_OBJECTS as u64 + 1;
        assert!(
            store.insert(past_cap, JsValue::Null, None).is_none(),
            "the store must refuse past its cap"
        );
        store.release(1);
        assert!(store.insert(past_cap, JsValue::Null, None).is_some());
    }

    #[test]
    fn clearing_drops_everything() {
        let mut store = ObjectStore::default();
        store.insert(18, JsValue::Null, None);
        store
            .insert(19, JsValue::Null, Some(String::from("g")))
            .unwrap();
        assert_eq!(store.clear(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn numbers_print_the_way_javascript_prints_them() {
        assert_eq!(number_description(1.0), "1");
        assert_eq!(number_description(-7.0), "-7");
        assert_eq!(number_description(1.5), "1.5");
        assert_eq!(number_description(f64::NAN), "NaN");
        assert_eq!(number_description(f64::INFINITY), "Infinity");
    }

    #[test]
    fn json_hostile_numbers_are_reported_as_unserializable() {
        assert_eq!(unserializable_number(f64::NAN).as_deref(), Some("NaN"));
        assert_eq!(
            unserializable_number(f64::INFINITY).as_deref(),
            Some("Infinity")
        );
        assert_eq!(
            unserializable_number(f64::NEG_INFINITY).as_deref(),
            Some("-Infinity")
        );
        // Negative zero is observable in JavaScript, so it must not collapse.
        assert_eq!(unserializable_number(-0.0).as_deref(), Some("-0"));
        assert_eq!(unserializable_number(0.0), None);
        assert_eq!(unserializable_number(42.0), None);
    }

    #[test]
    fn strings_are_escaped_for_json() {
        assert_eq!(json_string("hi"), r#""hi""#);
        assert_eq!(json_string("a\"b"), r#""a\"b""#);
        assert_eq!(json_string("a\\b"), r#""a\\b""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
        // A control character has no literal JSON spelling; it must be escaped.
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn descriptions_are_truncated_on_a_character_boundary() {
        // A minified bundle's top-level function is megabytes of source, and
        // the text is page-controlled — a byte slice through a codepoint panics.
        let text = "é".repeat(MAX_DESCRIPTION);
        let cut = truncate(&text);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= MAX_DESCRIPTION + 4);
        assert_eq!(truncate("short"), "short");
    }
}
