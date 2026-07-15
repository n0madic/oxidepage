//! WebIDL sources and the bindings generator (design doc §5.3).
//!
//! `xtask codegen` drives [`generate`], which parses `crates/idl/webidl/*.webidl`
//! with `weedle2` and emits `crates/bindings/src/generated.rs`: prototype
//! registration plus per-member glue functions that perform WebIDL argument
//! conversion and delegate to hand-written `imp::*` functions.
//!
//! The IDL is the checked contract: an interface, member, or type the
//! generator does not understand is a build-time error, not a silent gap.
//! Supported surface (grown deliberately, §11 "codegen scope creep" risk):
//! interfaces with inheritance, partial interfaces, mixins + `includes`,
//! constants, attributes (incl. stringifier), regular/getter operations,
//! variadic `DOMString...` and `(Node or DOMString)...`, `iterable<>`,
//! dictionaries/callbacks/unions as pass-through `any` values.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use weedle::Definition;
use weedle::argument::{Argument, ArgumentList};
use weedle::attribute::{ExtendedAttribute, ExtendedAttributeList};
use weedle::common::Identifier;
use weedle::interface::{
    AttributeInterfaceMember, ConstMember, InterfaceMember, StringifierOrInheritOrStatic,
};
use weedle::literal::{ConstValue, DefaultValue, IntegerLit};
use weedle::mixin::MixinMember;
use weedle::types::{NonAnyType, ReturnType, SingleType, Type};

/// A codegen failure: always a hard error, never skipped silently.
#[derive(Debug, thiserror::Error)]
#[error("webidl codegen: {0}")]
pub struct CodegenError(String);

fn err<T>(message: impl Into<String>) -> Result<T, CodegenError> {
    Err(CodegenError(message.into()))
}

/// Interfaces backed by a DOM node (`this` unwraps to a `NodeId`).
const NODE_INTERFACES: &[&str] = &[
    "Node",
    "Element",
    "HTMLElement",
    "HTMLUnknownElement",
    "HTMLScriptElement",
    // Node-backed and named as an argument/return type: `input.form` returns an
    // `HTMLFormElement?`, `label.control` an `HTMLElement?`.
    "HTMLFormElement",
    "HTMLInputElement",
    "HTMLTextAreaElement",
    "HTMLSelectElement",
    "HTMLOptionElement",
    "HTMLOptGroupElement",
    "HTMLButtonElement",
    "HTMLLabelElement",
    "HTMLFieldSetElement",
    "Document",
    "XMLDocument",
    "DocumentType",
    "DocumentFragment",
    "ShadowRoot",
    "CharacterData",
    "Text",
    "CDATASection",
    "Comment",
    "ProcessingInstruction",
];

/// `this`-unwrap method on `BindCx` per interface.
fn this_unwrap(interface: &str) -> Result<&'static str, CodegenError> {
    Ok(match interface {
        "EventTarget" => "this_event_target",
        "Window" => "this_window",
        "Node" => "this_node",
        "Element" | "HTMLElement" | "HTMLUnknownElement" => "this_element",
        // Per-tag interfaces with no members yet: they are element-backed, so
        // any future member would unwrap as a plain element. `this_unwrap` is
        // required for every registered interface even when currently unused.
        "HTMLHtmlElement"
        | "HTMLHeadElement"
        | "HTMLBodyElement"
        | "HTMLTitleElement"
        | "HTMLMetaElement"
        | "HTMLBaseElement"
        | "HTMLStyleElement"
        | "HTMLTemplateElement"
        | "HTMLDivElement"
        | "HTMLSpanElement"
        | "HTMLParagraphElement"
        | "HTMLHeadingElement"
        | "HTMLPreElement"
        | "HTMLQuoteElement"
        | "HTMLBRElement"
        | "HTMLHRElement"
        | "HTMLUListElement"
        | "HTMLOListElement"
        | "HTMLLIElement"
        | "HTMLDListElement"
        | "HTMLLegendElement"
        | "HTMLTableElement"
        | "HTMLTableSectionElement"
        | "HTMLTableRowElement"
        | "HTMLTableCellElement"
        | "HTMLTableColElement"
        | "HTMLTableCaptionElement"
        | "HTMLIFrameElement"
        | "HTMLCanvasElement"
        | "HTMLPictureElement"
        | "HTMLSourceElement"
        | "HTMLMediaElement"
        | "HTMLVideoElement"
        | "HTMLAudioElement"
        | "HTMLTrackElement"
        | "HTMLObjectElement"
        | "HTMLEmbedElement"
        | "HTMLMapElement"
        | "HTMLDataListElement"
        | "HTMLOutputElement"
        | "HTMLProgressElement"
        | "HTMLMeterElement"
        | "HTMLDetailsElement"
        | "HTMLDialogElement"
        | "HTMLMenuElement"
        | "HTMLTimeElement"
        | "HTMLDataElement"
        | "HTMLModElement"
        | "HTMLSlotElement"
        // Every element in the SVG namespace is an `SVGElement`; the SVG DOM's
        // other per-element interfaces are not implemented, so they stay absent
        // rather than half-faked. `SVGAElement` is element-backed too.
        | "SVGElement"
        | "SVGAElement" => "this_element",
        "SVGAnimatedString" => "this_svg_animated_string",
        // NodeFilter is a const-only namespace; it has no instances, so its
        // unwrap is never emitted (mapped for the unconditional lookup only).
        "NodeFilter" => "this_node",
        "HTMLScriptElement" => "this_html_script_element",
        "HTMLAnchorElement" => "this_html_anchor_element",
        "HTMLAreaElement" => "this_html_area_element",
        "HTMLImageElement" => "this_html_image_element",
        "HTMLLinkElement" => "this_html_link_element",
        // The form controls all carry members whose meaning is tag-specific
        // (`value` on an `<input>` is not `value` on a `<select>`), so each one
        // brand-checks its receiver rather than accepting any element.
        "HTMLFormElement" => "this_html_form_element",
        "HTMLInputElement" => "this_html_input_element",
        "HTMLTextAreaElement" => "this_html_text_area_element",
        "HTMLSelectElement" => "this_html_select_element",
        "HTMLOptionElement" => "this_html_option_element",
        "HTMLOptGroupElement" => "this_html_opt_group_element",
        "HTMLButtonElement" => "this_html_button_element",
        "HTMLLabelElement" => "this_html_label_element",
        "HTMLFieldSetElement" => "this_html_field_set_element",
        "Document" | "XMLDocument" => "this_document",
        // Not node-backed: a slab object carrying the document it was minted
        // for, so `document.implementation` remembers its own document.
        "DOMImplementation" => "this_dom_implementation",
        // A stateless brand: `parseFromString` needs nothing but the realm.
        "DOMParser" => "this_dom_parser",
        "DocumentType" => "this_document_type",
        "DocumentFragment" => "this_document_fragment",
        "ShadowRoot" => "this_shadow_root",
        "CharacterData" => "this_character_data",
        "Text" => "this_text",
        "CDATASection" => "this_cdata_section",
        "Comment" => "this_comment",
        "ProcessingInstruction" => "this_processing_instruction",
        "Event" | "CustomEvent" => "this_event",
        "NodeList" => "this_node_list",
        "HTMLCollection" => "this_html_collection",
        "NamedNodeMap" => "this_named_node_map",
        "Attr" => "this_attr",
        "Navigator" => "this_navigator",
        "Screen" => "this_screen",
        "Performance" => "this_performance",
        "PerformanceTiming" => "this_performance_timing",
        "FontFaceSet" => "this_font_face_set",
        "CustomElementRegistry" => "this_custom_element_registry",
        "MediaQueryList" => "this_media_query_list",
        "AbortController" => "this_abort_controller",
        "AbortSignal" => "this_abort_signal",
        "ResizeObserver" => "this_resize_observer",
        "ResizeObserverEntry" => "this_resize_observer_entry",
        "IntersectionObserver" => "this_intersection_observer",
        "IntersectionObserverEntry" => "this_intersection_observer_entry",
        "PluginArray" => "this_plugin_array",
        "MimeTypeArray" => "this_mime_type_array",
        "DOMTokenList" => "this_token_list",
        "MutationObserver" => "this_observer",
        "MutationRecord" => "this_mutation_record",
        "URL" => "this_url",
        "URLSearchParams" => "this_url_search_params",
        "Headers" => "this_headers",
        "Request" => "this_request",
        "FormData" => "this_form_data",
        "Response" => "this_response",
        "XMLHttpRequest" => "this_xhr",
        "CSSStyleDeclaration" => "this_style_decl",
        "StyleSheet" | "CSSStyleSheet" => "this_style_sheet",
        "StyleSheetList" => "this_style_sheet_list",
        "CSSRule" | "CSSStyleRule" => "this_css_rule",
        "CSSRuleList" => "this_css_rule_list",
        // The read-only base and its mutable subclass share the same backing
        // `RectData`, so they unwrap identically.
        "DOMRectReadOnly" | "DOMRect" => "this_dom_rect",
        "DOMRectList" => "this_dom_rect_list",
        // DOMStringMap (`element.dataset`) has zero members: it is a bootstrap
        // Proxy over the element wrapper, not a host object, so nothing ever
        // unwraps `this` to it. The mapping is required only for the
        // unconditional lookup above; it is never emitted.
        "DOMStringMap" => "this_element",
        other => return err(format!("no this-unwrap known for interface `{other}`")),
    })
}

/// camelCase / PascalCase → snake_case, treating caps runs as one word
/// (`createElementNS` → `create_element_ns`, `innerHTML` → `inner_html`).
fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_ascii_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if prev_lower || (prev_upper && next_lower) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Escapes Rust keywords with a raw-identifier prefix for generated paths.
fn rust_ident(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "type", "match", "move", "ref", "loop", "impl", "trait", "struct", "enum", "fn", "let",
        "const", "static", "use", "mod", "pub", "crate", "super", "self", "where", "for", "while",
        "if", "else", "return", "break", "continue", "in", "as", "dyn", "box", "unsafe", "extern",
        "true", "false", "async", "await",
    ];
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

/// How an argument converts at the boundary.
#[derive(Clone, Debug, PartialEq)]
enum ArgKind {
    DomString {
        nullable: bool,
    },
    Bool,
    U16,
    U32,
    I32,
    Double,
    Node {
        nullable: bool,
    },
    /// An `Element`-typed argument. Brand-checked to an actual element, so
    /// passing e.g. a `DocumentType` is a `TypeError` as WebIDL requires —
    /// `ArgKind::Node` would accept any node.
    Element,
    EventValue,
    /// any / dictionary / union / callback: passed through raw.
    Raw,
}

#[derive(Clone, Debug)]
enum ArgDefault {
    None,
    Bool(bool),
    Str(String),
    U32(u32),
}

#[derive(Clone, Debug)]
struct Arg {
    kind: ArgKind,
    optional: bool,
    default: ArgDefault,
}

#[derive(Clone, Debug)]
enum Variadic {
    None,
    Strings,
    Raw,
}

/// How a return value converts at the boundary.
#[derive(Clone, Debug)]
enum RetKind {
    Undefined,
    DomString {
        nullable: bool,
    },
    Bool,
    Number,
    Node {
        nullable: bool,
    },
    /// Collections, events, sequences, any: `imp` returns a finished JsValue.
    Raw,
}

/// Everything the generator knows about the parsed IDL universe.
struct Universe {
    /// Identifier → category for type classification.
    passthrough: Vec<String>,
    typedefs: HashMap<String, String>,
}

impl Universe {
    fn classify_arg(&self, ty: &Type<'_>) -> Result<ArgKind, CodegenError> {
        Ok(match ty {
            Type::Single(SingleType::Any(_)) => ArgKind::Raw,
            Type::Single(SingleType::NonAny(non_any)) => match non_any {
                NonAnyType::DOMString(t) => ArgKind::DomString {
                    nullable: t.q_mark.is_some(),
                },
                NonAnyType::USVString(t) => ArgKind::DomString {
                    nullable: t.q_mark.is_some(),
                },
                NonAnyType::Boolean(_) => ArgKind::Bool,
                NonAnyType::Integer(t) => match &t.type_ {
                    weedle::types::IntegerType::Short(s) if s.unsigned.is_some() => ArgKind::U16,
                    weedle::types::IntegerType::Long(l) if l.unsigned.is_some() => ArgKind::U32,
                    weedle::types::IntegerType::Long(_) => ArgKind::I32,
                    other => {
                        return err(format!("unsupported integer argument type: {other:?}"));
                    }
                },
                NonAnyType::FloatingPoint(_) => ArgKind::Double,
                NonAnyType::Identifier(id) => {
                    let name = self.resolve_typedef(id.type_.0);
                    let nullable = id.q_mark.is_some();
                    if name == "Element" && !nullable {
                        ArgKind::Element
                    } else if NODE_INTERFACES.contains(&name.as_str()) {
                        ArgKind::Node { nullable }
                    } else if name == "Event" || name == "CustomEvent" {
                        ArgKind::EventValue
                    } else if self.passthrough.contains(&name) {
                        ArgKind::Raw
                    } else if name == "double" {
                        ArgKind::Double
                    } else {
                        return err(format!("unknown identifier type in argument: `{name}`"));
                    }
                }
                other => return err(format!("unsupported argument type: {other:?}")),
            },
            Type::Union(_) => ArgKind::Raw,
        })
    }

    fn classify_return(&self, ty: &ReturnType<'_>) -> Result<RetKind, CodegenError> {
        let ty = match ty {
            ReturnType::Undefined(_) => return Ok(RetKind::Undefined),
            ReturnType::Type(t) => t,
        };
        Ok(match ty {
            Type::Single(SingleType::Any(_)) => RetKind::Raw,
            Type::Single(SingleType::NonAny(non_any)) => match non_any {
                NonAnyType::DOMString(t) => RetKind::DomString {
                    nullable: t.q_mark.is_some(),
                },
                NonAnyType::USVString(t) => RetKind::DomString {
                    nullable: t.q_mark.is_some(),
                },
                NonAnyType::Boolean(_) => RetKind::Bool,
                NonAnyType::Integer(_) => RetKind::Number,
                NonAnyType::FloatingPoint(_) => RetKind::Number,
                NonAnyType::Sequence(_) => RetKind::Raw,
                NonAnyType::Identifier(id) => {
                    let name = self.resolve_typedef(id.type_.0);
                    let nullable = id.q_mark.is_some();
                    if NODE_INTERFACES.contains(&name.as_str()) {
                        RetKind::Node { nullable }
                    } else if name == "double" {
                        RetKind::Number
                    } else if self.passthrough.contains(&name)
                        || name == "Event"
                        || name == "CustomEvent"
                        || name == "EventTarget"
                    {
                        RetKind::Raw
                    } else {
                        return err(format!("unknown identifier type in return: `{name}`"));
                    }
                }
                other => return err(format!("unsupported return type: {other:?}")),
            },
            Type::Union(_) => RetKind::Raw,
        })
    }

    fn resolve_typedef(&self, name: &str) -> String {
        self.typedefs
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }
}

/// A normalized member ready for emission.
enum Member {
    Const {
        name: String,
        value: f64,
    },
    Attribute {
        name: String,
        readonly: bool,
        stringifier: bool,
        kind: RetKind,
        setter_arg: Option<Arg>,
        /// `[CEReactions]`: the *setter* runs inside a custom element reactions
        /// stack entry. The getter never does — it enqueues nothing.
        ce_reactions: bool,
    },
    /// An `EventHandler` attribute (`onclick`). It needs no `imp` function: the
    /// accessor pair is emitted straight against the handler registry, keyed by
    /// the event type in the member's name.
    EventHandler {
        name: String,
    },
    Operation {
        name: String,
        args: Vec<Arg>,
        variadic: Variadic,
        ret: RetKind,
        /// `[CEReactions]`: the operation runs inside a custom element reactions
        /// stack entry, so the reactions it enqueues are invoked before it
        /// returns to script (ADR-0021).
        ce_reactions: bool,
    },
    Iterable,
}

struct Interface {
    name: String,
    parent: Option<String>,
    members: Vec<(String, Member)>, // (imp module, member)
    constructor: Option<Vec<Arg>>,
    has_constructor: bool,
}

fn integer_lit_i64(lit: &IntegerLit<'_>) -> Result<i64, CodegenError> {
    match lit {
        IntegerLit::Dec(d) => {
            d.0.parse::<i64>()
                .map_err(|e| CodegenError(format!("bad integer literal: {e}")))
        }
        IntegerLit::Hex(h) => {
            i64::from_str_radix(h.0.trim_start_matches("0x").trim_start_matches("0X"), 16)
                .map_err(|e| CodegenError(format!("bad hex literal: {e}")))
        }
        // A bare `0` tokenizes as octal.
        IntegerLit::Oct(o) => {
            i64::from_str_radix(o.0, 8).map_err(|e| CodegenError(format!("bad octal literal: {e}")))
        }
    }
}

fn const_value_f64(value: &ConstValue<'_>) -> Result<f64, CodegenError> {
    match value {
        ConstValue::Integer(lit) => Ok(integer_lit_i64(lit)? as f64),
        other => err(format!("unsupported constant value: {other:?}")),
    }
}

fn convert_args(
    universe: &Universe,
    args: &ArgumentList<'_>,
    context: &str,
) -> Result<(Vec<Arg>, Variadic), CodegenError> {
    let mut out = Vec::new();
    let mut variadic = Variadic::None;
    for (i, arg) in args.list.iter().enumerate() {
        match arg {
            Argument::Single(single) => {
                if !matches!(variadic, Variadic::None) {
                    return err(format!("{context}: argument after variadic"));
                }
                let kind = universe.classify_arg(&single.type_.type_)?;
                let default = match &single.default {
                    None => ArgDefault::None,
                    Some(d) => match &d.value {
                        DefaultValue::Boolean(b) => ArgDefault::Bool(b.0),
                        DefaultValue::String(s) => ArgDefault::Str(s.0.to_owned()),
                        DefaultValue::Integer(int) => {
                            let value = integer_lit_i64(int)?;
                            let value = u32::try_from(value).map_err(|_| {
                                CodegenError(format!("{context}: integer default out of u32 range"))
                            })?;
                            ArgDefault::U32(value)
                        }
                        DefaultValue::EmptyDictionary(_) | DefaultValue::Null(_) => {
                            ArgDefault::None
                        }
                        other => {
                            return err(format!("{context}: unsupported default: {other:?}"));
                        }
                    },
                };
                out.push(Arg {
                    kind,
                    optional: single.optional.is_some(),
                    default,
                });
            }
            Argument::Variadic(var) => {
                if i != args.list.len() - 1 {
                    return err(format!("{context}: variadic must be last"));
                }
                variadic = match universe.classify_arg(&var.type_)? {
                    ArgKind::DomString { nullable: false } => Variadic::Strings,
                    ArgKind::Raw => Variadic::Raw,
                    other => {
                        return err(format!("{context}: unsupported variadic kind {other:?}"));
                    }
                };
            }
        }
    }
    Ok((out, variadic))
}

/// Whether an attribute is declared `EventHandler` — the codegen's marker for
/// HTML's event handler IDL attributes, which are emitted without an `imp`.
fn is_event_handler(ty: &Type<'_>) -> bool {
    matches!(
        ty,
        Type::Single(SingleType::NonAny(NonAnyType::Identifier(id)))
            if id.type_.0 == "EventHandler"
    )
}

/// Extended attributes the codegen reads and acts on.
const CE_REACTIONS: &str = "CEReactions";

/// Extended attributes the codegen knowingly ignores, each for a reason:
///
/// - `SameObject` / `NewObject` — identity of the returned object. The `imp`
///   functions already honor these by hand (the `[SameObject]` caches, e.g.
///   [`cx::CSS_RULES_MEMBER`]), so the annotation is documentation here.
/// - `Unscopable` — only observable inside `with (…)`, which we do not support.
/// - `PutForwards` — the three uses are hand-implemented setters.
/// - `LegacyNullToEmptyString` — a *type*-level attribute; the null-to-empty
///   coercion is hand-written in the `imp`.
///
/// Anything else is a hard error: an annotation we neither honor nor recognize
/// is a silent behavior gap, and this is where it must surface (a typo'd
/// `[CEReaction]` used to compile and do nothing).
const IGNORED_EXTENDED_ATTRS: &[&str] = &[
    "SameObject",
    "NewObject",
    "Unscopable",
    "PutForwards",
    "LegacyNullToEmptyString",
];

/// Reads a member's extended attribute list and returns whether it is
/// `[CEReactions]` — the annotation that scopes the member's glue in a custom
/// element reactions stack entry (ADR-0021).
fn ce_reactions(
    attrs: Option<&ExtendedAttributeList<'_>>,
    context: &str,
) -> Result<bool, CodegenError> {
    let Some(attrs) = attrs else {
        return Ok(false);
    };
    let mut found = false;
    for attr in &attrs.body.list {
        let name = match attr {
            ExtendedAttribute::NoArgs(a) => a.0.0,
            ExtendedAttribute::Ident(a) => a.lhs_identifier.0,
            ExtendedAttribute::IdentList(a) => a.identifier.0,
            ExtendedAttribute::ArgList(a) => a.identifier.0,
            ExtendedAttribute::NamedArgList(a) => a.lhs_identifier.0,
        };
        if name == CE_REACTIONS {
            found = true;
        } else if !IGNORED_EXTENDED_ATTRS.contains(&name) {
            return err(format!("{context}: unknown extended attribute `[{name}]`"));
        }
    }
    Ok(found)
}

/// `[CEReactions]` on a member kind whose glue cannot host a reactions scope.
fn reject_ce_reactions(
    attrs: Option<&ExtendedAttributeList<'_>>,
    context: &str,
) -> Result<(), CodegenError> {
    if ce_reactions(attrs, context)? {
        return err(format!(
            "{context}: [CEReactions] unsupported on this member"
        ));
    }
    Ok(())
}

fn attribute_member(
    universe: &Universe,
    attr: &AttributeInterfaceMember<'_>,
    interface: &str,
) -> Result<Member, CodegenError> {
    let context = format!("{interface}.{}", attr.identifier.0);
    if is_event_handler(&attr.type_.type_) {
        // An event handler attribute's setter only stores a callback; it mutates
        // no DOM, so there is nothing to scope.
        reject_ce_reactions(attr.attributes.as_ref(), &context)?;
        return Ok(Member::EventHandler {
            name: attr.identifier.0.to_owned(),
        });
    }
    let ce = ce_reactions(attr.attributes.as_ref(), &context)?;
    let kind = universe.classify_return(&ReturnType::Type(attr.type_.type_.clone()))?;
    let readonly = attr.readonly.is_some();
    let setter_arg = if readonly {
        None
    } else {
        Some(Arg {
            kind: universe.classify_arg(&attr.type_.type_)?,
            optional: false,
            default: ArgDefault::None,
        })
    };
    Ok(Member::Attribute {
        name: attr.identifier.0.to_owned(),
        readonly,
        stringifier: matches!(
            attr.modifier,
            Some(StringifierOrInheritOrStatic::Stringifier(_))
        ),
        kind,
        setter_arg,
        ce_reactions: ce,
    })
}

fn operation_member(
    universe: &Universe,
    identifier: Option<&Identifier<'_>>,
    args: &ArgumentList<'_>,
    return_type: &ReturnType<'_>,
    attrs: Option<&ExtendedAttributeList<'_>>,
    interface: &str,
) -> Result<Member, CodegenError> {
    let Some(name) = identifier else {
        return err(format!(
            "{interface}: unnamed special operations unsupported"
        ));
    };
    let context = format!("{interface}.{}", name.0);
    let (converted, variadic) = convert_args(universe, args, &context)?;
    Ok(Member::Operation {
        name: name.0.to_owned(),
        args: converted,
        variadic,
        ret: universe.classify_return(return_type)?,
        ce_reactions: ce_reactions(attrs, &context)?,
    })
}

fn const_member(konst: &ConstMember<'_>, interface: &str) -> Result<Member, CodegenError> {
    let context = format!("{interface}.{}", konst.identifier.0);
    reject_ce_reactions(konst.attributes.as_ref(), &context)?;
    Ok(Member::Const {
        name: konst.identifier.0.to_owned(),
        value: const_value_f64(&konst.const_value)?,
    })
}

/// Parses all `.webidl` files in `idl_dir` and returns the contents of the
/// generated bindings module.
pub fn generate(idl_dir: &Path) -> Result<String, CodegenError> {
    let mut sources = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(idl_dir)
        .map_err(|e| CodegenError(format!("reading {}: {e}", idl_dir.display())))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "webidl"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        return err(format!("no .webidl files in {}", idl_dir.display()));
    }
    for path in &entries {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CodegenError(format!("reading {}: {e}", path.display())))?;
        sources.push((path.clone(), text));
    }

    // Parse everything first.
    let mut parsed = Vec::new();
    for (path, text) in &sources {
        let defs = weedle::parse(text)
            .map_err(|e| CodegenError(format!("parsing {}: {e}", path.display())))?;
        parsed.push(defs);
    }
    let definitions: Vec<&Definition<'_>> = parsed.iter().flatten().collect();

    // Universe: names of dictionaries, callbacks, and enums → pass-through;
    // typedef resolution table.
    let mut universe = Universe {
        passthrough: vec![
            "EventListener".into(),
            "NodeList".into(),
            "HTMLCollection".into(),
            "DOMTokenList".into(),
            "MutationObserver".into(),
            "MutationRecord".into(),
            "MutationCallback".into(),
            // Abort primitives: `AbortController.signal` returns an
            // `AbortSignal` as a finished JS value.
            "AbortController".into(),
            "AbortSignal".into(),
            // `Performance.timing` returns a `PerformanceTiming` JS value.
            "PerformanceTiming".into(),
            // Phase 3 interfaces passed through as raw JS values (their imp
            // functions return/receive finished `JsValue`s).
            "URL".into(),
            "URLSearchParams".into(),
            "FormData".into(),
            "Headers".into(),
            "Request".into(),
            "Response".into(),
            "XMLHttpRequest".into(),
            // Custom elements: the registry's imp functions receive and return
            // raw JS values (constructors, promises, options objects).
            "CustomElementRegistry".into(),
            // Phase 4 CSSOM interfaces (imp functions return finished JsValues).
            "CSSStyleDeclaration".into(),
            "StyleSheet".into(),
            "CSSStyleSheet".into(),
            "StyleSheetList".into(),
            "CSSRule".into(),
            "CSSStyleRule".into(),
            "CSSRuleList".into(),
            // Phase 5 geometry interfaces (imp functions return finished
            // JsValues; `DOMRect?` returns/args pass through as raw values).
            "DOMRectReadOnly".into(),
            "DOMRect".into(),
            "DOMRectList".into(),
            // `SVGAElement.href` returns a finished `SVGAnimatedString` JsValue.
            "SVGAnimatedString".into(),
            // `Document.implementation` returns a finished JsValue (a slab
            // object, not a node), so the type is passed through.
            "DOMImplementation".into(),
            // `element.dataset` returns a finished `DOMStringMap` JsValue (the
            // `datasetProxy` bootstrap wrapper), not a node or host object.
            "DOMStringMap".into(),
        ],
        typedefs: HashMap::new(),
    };
    for def in &definitions {
        match def {
            Definition::Dictionary(d) => universe.passthrough.push(d.identifier.0.to_owned()),
            Definition::Callback(c) => universe.passthrough.push(c.identifier.0.to_owned()),
            Definition::CallbackInterface(c) => {
                universe.passthrough.push(c.identifier.0.to_owned());
            }
            Definition::Enum(e) => universe.passthrough.push(e.identifier.0.to_owned()),
            Definition::Typedef(t) => {
                let target = match &t.type_.type_ {
                    Type::Single(SingleType::NonAny(NonAnyType::FloatingPoint(_))) => {
                        "double".to_owned()
                    }
                    other => return err(format!("unsupported typedef target: {other:?}")),
                };
                universe.typedefs.insert(t.identifier.0.to_owned(), target);
            }
            _ => {}
        }
    }

    // Collect mixins.
    let mut mixins: HashMap<String, Vec<&MixinMember<'_>>> = HashMap::new();
    for def in &definitions {
        if let Definition::InterfaceMixin(mixin) = def {
            mixins.insert(
                mixin.identifier.0.to_owned(),
                mixin.members.body.iter().collect(),
            );
        }
    }

    // Collect interfaces (+ partials merged in file order).
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for def in &definitions {
        if let Definition::Interface(interface) = def {
            let name = interface.identifier.0.to_owned();
            if index.contains_key(&name) {
                return err(format!("duplicate interface `{name}`"));
            }
            index.insert(name.clone(), interfaces.len());
            interfaces.push(Interface {
                parent: interface.inheritance.map(|i| i.identifier.0.to_owned()),
                name,
                members: Vec::new(),
                constructor: None,
                has_constructor: false,
            });
        }
    }

    let imp_module_for = |interface: &str| snake(interface);

    // Interface members (own + partial).
    let process_members = |interfaces: &mut Vec<Interface>,
                           name: &str,
                           members: &[&InterfaceMember<'_>]|
     -> Result<(), CodegenError> {
        let idx = *index
            .get(name)
            .ok_or_else(|| CodegenError(format!("partial for unknown interface `{name}`")))?;
        let imp_module = imp_module_for(name);
        for member in members {
            match member {
                InterfaceMember::Const(konst) => {
                    interfaces[idx]
                        .members
                        .push((imp_module.clone(), const_member(konst, name)?));
                }
                InterfaceMember::Attribute(attr) => {
                    interfaces[idx]
                        .members
                        .push((imp_module.clone(), attribute_member(&universe, attr, name)?));
                }
                InterfaceMember::Operation(op) => {
                    if op.modifier.is_some() {
                        return err(format!("{name}: static/stringifier ops unsupported"));
                    }
                    let member = operation_member(
                        &universe,
                        op.identifier.as_ref(),
                        &op.args.body,
                        &op.return_type,
                        op.attributes.as_ref(),
                        name,
                    )?;
                    interfaces[idx].members.push((imp_module.clone(), member));
                }
                InterfaceMember::Constructor(ctor) => {
                    let context = format!("{name} constructor");
                    reject_ce_reactions(ctor.attributes.as_ref(), &context)?;
                    let (args, variadic) = convert_args(&universe, &ctor.args.body, &context)?;
                    if !matches!(variadic, Variadic::None) {
                        return err(format!("{context}: variadic constructors unsupported"));
                    }
                    interfaces[idx].constructor = Some(args);
                    interfaces[idx].has_constructor = true;
                }
                InterfaceMember::Iterable(_) => {
                    interfaces[idx]
                        .members
                        .push((imp_module.clone(), Member::Iterable));
                }
                other => {
                    return err(format!("{name}: unsupported member kind: {other:?}"));
                }
            }
        }
        Ok(())
    };

    for def in &definitions {
        match def {
            Definition::Interface(interface) => {
                let members: Vec<&InterfaceMember<'_>> = interface.members.body.iter().collect();
                process_members(&mut interfaces, interface.identifier.0, &members)?;
            }
            Definition::PartialInterface(partial) => {
                let members: Vec<&InterfaceMember<'_>> = partial.members.body.iter().collect();
                process_members(&mut interfaces, partial.identifier.0, &members)?;
            }
            _ => {}
        }
    }

    // `includes` statements: mixin members join the including interface but
    // keep the mixin's imp module so the implementation exists once.
    for def in &definitions {
        let Definition::IncludesStatement(includes) = def else {
            continue;
        };
        let target = includes.lhs_identifier.0;
        let mixin_name = includes.rhs_identifier.0;
        let idx = *index
            .get(target)
            .ok_or_else(|| CodegenError(format!("includes for unknown interface `{target}`")))?;
        let members = mixins
            .get(mixin_name)
            .ok_or_else(|| CodegenError(format!("unknown mixin `{mixin_name}`")))?;
        let imp_module = snake(mixin_name);
        for member in members {
            let converted = match member {
                MixinMember::Const(konst) => const_member(konst, mixin_name)?,
                MixinMember::Attribute(attr) if is_event_handler(&attr.type_.type_) => {
                    let context = format!("{mixin_name}.{}", attr.identifier.0);
                    reject_ce_reactions(attr.attributes.as_ref(), &context)?;
                    Member::EventHandler {
                        name: attr.identifier.0.to_owned(),
                    }
                }
                MixinMember::Attribute(attr) => {
                    let context = format!("{mixin_name}.{}", attr.identifier.0);
                    let ce = ce_reactions(attr.attributes.as_ref(), &context)?;
                    let kind =
                        universe.classify_return(&ReturnType::Type(attr.type_.type_.clone()))?;
                    let readonly = attr.readonly.is_some();
                    let setter_arg = if readonly {
                        None
                    } else {
                        Some(Arg {
                            kind: universe.classify_arg(&attr.type_.type_)?,
                            optional: false,
                            default: ArgDefault::None,
                        })
                    };
                    Member::Attribute {
                        name: attr.identifier.0.to_owned(),
                        readonly,
                        stringifier: attr.stringifier.is_some(),
                        kind,
                        setter_arg,
                        ce_reactions: ce,
                    }
                }
                MixinMember::Operation(op) => operation_member(
                    &universe,
                    op.identifier.as_ref(),
                    &op.args.body,
                    &op.return_type,
                    op.attributes.as_ref(),
                    mixin_name,
                )?,
                MixinMember::Stringifier(_) => {
                    return err(format!("{mixin_name}: bare stringifier unsupported"));
                }
            };
            interfaces[idx]
                .members
                .push((imp_module.clone(), converted));
        }
    }

    // Topological order: parents first (stable within levels).
    let order = topo_order(&interfaces)?;

    // === Emission ===
    let mut glue = String::new();
    let mut registration = String::new();
    let mut handler_types = BTreeSet::new();

    for &i in &order {
        let interface = &interfaces[i];
        emit_interface(interface, &mut glue, &mut registration, &mut handler_types)?;
    }

    let mut out = String::new();
    out.push_str(
        "// @generated by `cargo xtask codegen` from crates/idl/webidl/*.webidl.\n\
         // DO NOT EDIT. Regenerate with `cargo xtask codegen`.\n\n\
         use oxidepage_js::{HostCall, JsThrow, JsValue};\n\n\
         use crate::cx::{BindCx, CtorSpec};\n\
         use crate::imp;\n\n",
    );

    // The event types that have a handler, straight from the `EventHandler`
    // attributes above. `handlers.rs` reads this to decide which `on*` *content*
    // attributes are handlers, so the IDL half and the markup half of HTML's
    // "install an event handler" are the same list by construction.
    out.push_str(
        "/// Every event type with an event handler, from the `EventHandler`\n\
         /// attributes in the IDL. Sorted, so a lookup can binary-search.\n\
         pub(crate) const EVENT_HANDLER_TYPES: &[&str] = &[\n",
    );
    for ty in &handler_types {
        let _ = writeln!(out, "    {ty:?},");
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// Registers every IDL-defined interface: prototypes, methods,\n\
         /// accessors, constants, and constructors, in inheritance order.\n\
         pub(crate) fn register_interfaces(cx: &BindCx<'_>) -> Result<(), JsThrow> {\n",
    );
    out.push_str(&registration);
    out.push_str("    Ok(())\n}\n\n");
    out.push_str(&glue);
    Ok(out)
}

fn topo_order(interfaces: &[Interface]) -> Result<Vec<usize>, CodegenError> {
    let index: HashMap<&str, usize> = interfaces
        .iter()
        .enumerate()
        .map(|(i, iface)| (iface.name.as_str(), i))
        .collect();
    let mut order = Vec::new();
    let mut done = vec![false; interfaces.len()];
    // Quadratic in the number of interfaces; the universe is small.
    while order.len() < interfaces.len() {
        let before = order.len();
        for (i, interface) in interfaces.iter().enumerate() {
            if done[i] {
                continue;
            }
            let ready = match &interface.parent {
                None => true,
                Some(parent) => match index.get(parent.as_str()) {
                    Some(&p) => done[p],
                    None => {
                        return err(format!(
                            "interface `{}` inherits unknown `{parent}`",
                            interface.name
                        ));
                    }
                },
            };
            if ready {
                done[i] = true;
                order.push(i);
            }
        }
        if order.len() == before {
            return err("inheritance cycle in IDL".to_owned());
        }
    }
    Ok(order)
}

/// Emits the argument-conversion statements; returns the imp-call argument list.
fn emit_args(
    body: &mut String,
    args: &[Arg],
    variadic: &Variadic,
    context: &str,
) -> Result<Vec<String>, CodegenError> {
    let mut names = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let var = format!("a{i}");
        let expr = match (&arg.kind, arg.optional, &arg.default) {
            (ArgKind::DomString { nullable: false }, false, _) => {
                format!("cx.arg_dom_string(call, {i})?")
            }
            (ArgKind::DomString { nullable: true }, false, _) => {
                format!("cx.arg_nullable_dom_string(call, {i})?")
            }
            (ArgKind::DomString { nullable: false }, true, ArgDefault::Str(s)) => {
                format!("cx.arg_dom_string_or(call, {i}, {s:?})?")
            }
            (ArgKind::DomString { nullable: false }, true, ArgDefault::None) => {
                format!("cx.arg_opt_dom_string(call, {i})?")
            }
            (ArgKind::Bool, false, _) => format!("cx.arg_bool(call, {i})"),
            (ArgKind::Bool, true, ArgDefault::Bool(b)) => {
                format!("cx.arg_bool_or(call, {i}, {b})")
            }
            (ArgKind::Bool, true, ArgDefault::None) => format!("cx.arg_opt_bool(call, {i})"),
            (ArgKind::U16, false, _) => format!("cx.arg_u16(call, {i})?"),
            (ArgKind::U32, false, _) => format!("cx.arg_u32(call, {i})?"),
            (ArgKind::I32, false, _) => format!("cx.arg_i32(call, {i})?"),
            (ArgKind::U32, true, ArgDefault::U32(n)) => format!("cx.arg_u32_or(call, {i}, {n})?"),
            (ArgKind::Double, false, _) => format!("cx.arg_f64(call, {i})?"),
            (ArgKind::Double, true, ArgDefault::U32(n)) => {
                format!("cx.arg_f64_or(call, {i}, {n}f64)?")
            }
            (ArgKind::Node { nullable: false }, false, _) => format!("cx.arg_node(call, {i})?"),
            (ArgKind::Element, false, _) => format!("cx.arg_element(call, {i})?"),
            // `Node?` and `optional Node? = null` read the same: a missing
            // argument is `undefined`, which `arg_nullable_node` maps to `None`
            // exactly as it maps an explicit `null`.
            (ArgKind::Node { nullable: true }, false | true, ArgDefault::None) => {
                format!("cx.arg_nullable_node(call, {i})?")
            }
            (ArgKind::EventValue, false, _) => format!("cx.arg_event_value(call, {i})?"),
            (ArgKind::Raw, _, _) => format!("call.arg({i})"),
            (kind, optional, default) => {
                return err(format!(
                    "{context}: unsupported arg combination {kind:?} optional={optional} default={default:?}"
                ));
            }
        };
        let _ = writeln!(body, "    let {var} = {expr};");
        names.push(var);
    }
    match variadic {
        Variadic::None => {}
        Variadic::Strings => {
            let start = args.len();
            let _ = writeln!(
                body,
                "    let rest = cx.arg_rest_dom_strings(call, {start})?;"
            );
            names.push("rest".to_owned());
        }
        Variadic::Raw => {
            let start = args.len();
            let _ = writeln!(
                body,
                "    let rest: Vec<JsValue> = call.args.get({start}..).map(<[JsValue]>::to_vec).unwrap_or_default();"
            );
            names.push("rest".to_owned());
        }
    }
    Ok(names)
}

fn emit_return(body: &mut String, ret: &RetKind, imp_call: &str) {
    match ret {
        RetKind::Undefined => {
            let _ = writeln!(body, "    {imp_call}?;\n    Ok(JsValue::Undefined)");
        }
        RetKind::DomString { nullable: false } => {
            let _ = writeln!(body, "    Ok(JsValue::String({imp_call}?))");
        }
        RetKind::DomString { nullable: true } => {
            let _ = writeln!(
                body,
                "    Ok(match {imp_call}? {{\n        Some(s) => JsValue::String(s),\n        None => JsValue::Null,\n    }})"
            );
        }
        RetKind::Bool => {
            let _ = writeln!(body, "    Ok(JsValue::Bool({imp_call}?))");
        }
        RetKind::Number => {
            let _ = writeln!(body, "    Ok(JsValue::Number({imp_call}?))");
        }
        RetKind::Node { nullable: false } => {
            let _ = writeln!(body, "    let ret = {imp_call}?;\n    cx.node_to_js(ret)");
        }
        RetKind::Node { nullable: true } => {
            let _ = writeln!(
                body,
                "    let ret = {imp_call}?;\n    cx.opt_node_to_js(ret)"
            );
        }
        RetKind::Raw => {
            let _ = writeln!(body, "    {imp_call}");
        }
    }
}

fn required_len(args: &[Arg]) -> usize {
    args.iter().take_while(|a| !a.optional).count()
}

fn emit_interface(
    interface: &Interface,
    glue: &mut String,
    registration: &mut String,
    handler_types: &mut BTreeSet<String>,
) -> Result<(), CodegenError> {
    let name = &interface.name;
    let iface_snake = snake(name);
    let unwrap = this_unwrap(name)?;
    let proto_var = format!("proto_{iface_snake}");
    let parent = match &interface.parent {
        Some(p) => format!("Some({p:?})"),
        None => "None".to_owned(),
    };
    let _ = writeln!(
        registration,
        "    let {proto_var} = cx.begin_interface({name:?}, {parent})?;"
    );

    let mut has_iterable = false;
    for (imp_module, member) in &interface.members {
        match member {
            Member::Const { name: cname, value } => {
                let _ = writeln!(
                    registration,
                    "    cx.define_constant(&{proto_var}, {cname:?}, {value}f64)?;"
                );
            }
            Member::Iterable => has_iterable = true,
            Member::EventHandler { name: attr_name } => {
                let Some(event_type) = attr_name.strip_prefix("on") else {
                    return err(format!(
                        "event handler attribute `{attr_name}` must be named `on<type>`"
                    ));
                };
                handler_types.insert(event_type.to_owned());

                let attr_snake = snake(attr_name);
                let getter_fn = format!("gen_{iface_snake}_get_{attr_snake}");
                let setter_fn = format!("gen_{iface_snake}_set_{attr_snake}");
                // `this` is whatever the interface's unwrap yields — a `NodeId`
                // for element/document, an `EventTargetKey` for the Window — and
                // both convert into the registry's key, so one shape serves all.
                let _ = writeln!(
                    glue,
                    "fn {getter_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{\n\
                    \x20   let this = cx.{unwrap}(&call.this)?;\n\
                    \x20   Ok(imp::event_handler(cx, this, {event_type:?}))\n\
                     }}\n"
                );
                let _ = writeln!(
                    glue,
                    "fn {setter_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{\n\
                    \x20   let this = cx.{unwrap}(&call.this)?;\n\
                    \x20   let value = call.arg(0);\n\
                    \x20   imp::set_event_handler(cx, this, {event_type:?}, value);\n\
                    \x20   Ok(JsValue::Undefined)\n\
                     }}\n"
                );
                let _ = writeln!(
                    registration,
                    "    cx.define_accessor(&{proto_var}, {attr_name:?}, {getter_fn}, {setter_fn})?;"
                );
            }
            Member::Attribute {
                name: attr_name,
                readonly,
                stringifier,
                kind,
                setter_arg,
                ce_reactions,
            } => {
                let attr_snake = snake(attr_name);
                let getter_fn = format!("gen_{iface_snake}_get_{attr_snake}");
                let mut body = String::new();
                let _ = writeln!(
                    body,
                    "fn {getter_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{"
                );
                let _ = writeln!(body, "    let this = cx.{unwrap}(&call.this)?;");
                let imp_call = format!("imp::{imp_module}::{}(cx, this)", rust_ident(&attr_snake));
                emit_return(&mut body, kind, &imp_call);
                let _ = writeln!(body, "}}\n");
                glue.push_str(&body);

                if *readonly {
                    let _ = writeln!(
                        registration,
                        "    cx.define_getter(&{proto_var}, {attr_name:?}, {getter_fn})?;"
                    );
                } else {
                    let setter_fn = format!("gen_{iface_snake}_set_{attr_snake}");
                    let arg = setter_arg
                        .as_ref()
                        .expect("non-readonly attribute has a setter arg");
                    let mut body = String::new();
                    let _ = writeln!(
                        body,
                        "fn {setter_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{"
                    );
                    let _ = writeln!(body, "    let this = cx.{unwrap}(&call.this)?;");
                    let names = emit_args(
                        &mut body,
                        std::slice::from_ref(arg),
                        &Variadic::None,
                        &format!("{name}.{attr_name} setter"),
                    )?;
                    let imp_call = format!(
                        "imp::{imp_module}::set_{attr_snake}(cx, this, {})",
                        names.join(", ")
                    );
                    emit_return(&mut body, &RetKind::Undefined, &imp_call);
                    let _ = writeln!(body, "}}\n");
                    glue.push_str(&body);
                    // Only the setter is scoped: a getter enqueues no reactions.
                    let define = if *ce_reactions {
                        "define_accessor_ce"
                    } else {
                        "define_accessor"
                    };
                    let _ = writeln!(
                        registration,
                        "    cx.{define}(&{proto_var}, {attr_name:?}, {getter_fn}, {setter_fn})?;"
                    );
                }
                if *stringifier {
                    let _ = writeln!(
                        registration,
                        "    cx.define_method(&{proto_var}, \"toString\", 0, {getter_fn})?;"
                    );
                }
            }
            Member::Operation {
                name: op_name,
                args,
                variadic,
                ret,
                ce_reactions,
            } => {
                let op_snake = snake(op_name);
                let glue_fn = format!("gen_{iface_snake}_{op_snake}");
                let mut body = String::new();
                let _ = writeln!(
                    body,
                    "fn {glue_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{"
                );
                let _ = writeln!(body, "    let this = cx.{unwrap}(&call.this)?;");
                let names = emit_args(&mut body, args, variadic, &format!("{name}.{op_name}"))?;
                let mut call_args = vec!["cx".to_owned(), "this".to_owned()];
                call_args.extend(names);
                let imp_call = format!(
                    "imp::{imp_module}::{}({})",
                    rust_ident(&op_snake),
                    call_args.join(", ")
                );
                emit_return(&mut body, ret, &imp_call);
                let _ = writeln!(body, "}}\n");
                glue.push_str(&body);
                let define = if *ce_reactions {
                    "define_method_ce"
                } else {
                    "define_method"
                };
                let _ = writeln!(
                    registration,
                    "    cx.{define}(&{proto_var}, {op_name:?}, {}, {glue_fn})?;",
                    required_len(args)
                );
            }
        }
    }

    if has_iterable {
        let _ = writeln!(registration, "    cx.install_iterable(&{proto_var})?;");
    }

    // Constructor.
    if interface.has_constructor {
        let args = interface.constructor.as_ref().expect("checked");
        let ctor_fn = format!("gen_{iface_snake}_constructor");
        let mut body = String::new();
        let _ = writeln!(
            body,
            "fn {ctor_fn}(cx: &BindCx<'_>, call: &HostCall) -> Result<JsValue, JsThrow> {{"
        );
        let names = emit_args(
            &mut body,
            args,
            &Variadic::None,
            &format!("{name} constructor"),
        )?;
        let mut call_args = vec!["cx".to_owned(), "call".to_owned()];
        call_args.extend(names);
        let _ = writeln!(
            body,
            "    imp::{iface_snake}::constructor({})",
            call_args.join(", ")
        );
        let _ = writeln!(body, "}}\n");
        glue.push_str(&body);
        let _ = writeln!(
            registration,
            "    cx.finish_interface({name:?}, &{proto_var}, CtorSpec::Native {{ length: {}, construct: {ctor_fn} }})?;",
            required_len(args)
        );
    } else {
        let _ = writeln!(
            registration,
            "    cx.finish_interface({name:?}, &{proto_var}, CtorSpec::Illegal)?;"
        );
    }
    registration.push('\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_handles_caps_runs() {
        assert_eq!(snake("appendChild"), "append_child");
        assert_eq!(snake("createElementNS"), "create_element_ns");
        assert_eq!(snake("innerHTML"), "inner_html");
        assert_eq!(snake("URL"), "url");
        assert_eq!(snake("baseURI"), "base_uri");
        assert_eq!(snake("DOMTokenList"), "dom_token_list");
        assert_eq!(snake("HTMLCollection"), "html_collection");
        assert_eq!(snake("nodeType"), "node_type");
    }

    #[test]
    fn generates_from_the_checked_in_idl() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("webidl");
        let generated = generate(&dir).expect("codegen must succeed on checked-in IDL");
        assert!(generated.contains("fn register_interfaces"));
        assert!(generated.contains("gen_node_append_child"));
        assert!(generated.contains("gen_element_get_inner_html"));
        assert!(generated.contains("gen_event_constructor"));
        assert!(generated.contains("cx.define_constant(&proto_node, \"ELEMENT_NODE\", 1f64)"));
        // Mixin members register on the including interface but delegate to
        // the mixin's imp module.
        assert!(generated.contains(
            "cx.define_method(&proto_element, \"querySelector\", 1, gen_element_query_selector)"
        ));
        assert!(generated.contains("imp::parent_node::query_selector"));
        // Geometry: optional double constructor args default to 0, the mutable
        // subclass gets accessors while the base gets getters, and both unwrap
        // through the shared `RectData`.
        assert!(generated.contains("cx.arg_f64_or(call, 0, 0f64)?"));
        assert!(generated.contains("cx.define_getter(&proto_dom_rect_read_only, \"top\""));
        assert!(generated.contains("cx.define_accessor(&proto_dom_rect, \"x\""));
        // `toJSON` is declared on the read-only base and delegates to its imp
        // module (which re-exports the shared implementation).
        assert!(generated.contains("imp::dom_rect_read_only::to_json"));
    }
}
