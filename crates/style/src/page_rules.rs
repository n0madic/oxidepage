//! `@page` rules (ADR-0041).
//!
//! stylo has the whole `@page` model — `CssRule::Page`, `PageRule`,
//! `PageSelectors`, the stylist's page-rule map — in every build, but it only
//! *parses* the at-rule in its Gecko build (`"page" if cfg!(feature =
//! "gecko")`), and the `size` descriptor is a Gecko-only longhand. So a Servo
//! build drops every `@page` rule on the floor.
//!
//! This module parses them itself and **splices** the result into the very
//! rule lists stylo built, as ordinary `CssRule::Page` values. That keeps one
//! list per sheet: `cssRules` indices, `insertRule`/`deleteRule`, cascade-order
//! walks and `effective_rules` (media, `@supports`, `@import`, disabled
//! sheets) all see the page rule exactly where the author wrote it, without a
//! second, parallel structure to keep in step.
//!
//! What stylo's `PageRule` has no slot for is `size`, because its declaration
//! block can only hold properties stylo knows. That one descriptor lives in a
//! side table here, keyed by the rule's `Arc` and holding a clone of it, so an
//! address can never be reused while it is a key; entries whose rule nothing
//! else references any more are pruned on every registration. The table is
//! per thread because a page and all its sheets live on one thread (design
//! §7), and because `@import`ed sheets are built inside the import loader,
//! which has no handle on the [`StyleEngine`](crate::StyleEngine).
//!
//! `page-orientation` and the margin at-rules (`@top-left` and friends) are not
//! parsed: nothing here would honour them, and a descriptor that reads back
//! but changes nothing is the fake P6 forbids.

use std::cell::RefCell;

use cssparser::{
    AtRuleParser, CowRcStr, DeclarationParser, ParseError, Parser, ParserInput, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, SourceLocation, StyleSheetParser,
    Token,
};
use servo_arc::Arc as ServoArc;
use style::context::QuirksMode;
use style::parser::{Parse, ParserContext};
use style::properties::{PropertyDeclarationBlock, parse_property_declaration_list};
use style::shared_lock::{Locked, SharedRwLock, SharedRwLockReadGuard};
use style::stylesheets::{
    CssRule, CssRuleType, CssRules, Origin, PageRule, PageSelectors, UrlExtraData,
};
use style_traits::{ParsingMode, ToCss as _};

/// CSS px per inch, and so per millimetre.
const PX_PER_IN: f32 = 96.0;
const PX_PER_MM: f32 = PX_PER_IN / 25.4;
/// `em`/`rem` inside `@page`: the page context's font size is the initial
/// `medium`, 16px.
const PX_PER_EM: f32 = 16.0;

/// A page's orientation keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    Landscape,
}

/// The named page sizes of css-page-3, portrait, in CSS px.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedSize {
    A5,
    A4,
    A3,
    B5,
    B4,
    JisB5,
    JisB4,
    Letter,
    Legal,
    Ledger,
}

impl NamedSize {
    fn from_ident(ident: &str) -> Option<Self> {
        Some(match ident.to_ascii_lowercase().as_str() {
            "a5" => Self::A5,
            "a4" => Self::A4,
            "a3" => Self::A3,
            "b5" => Self::B5,
            "b4" => Self::B4,
            "jis-b5" => Self::JisB5,
            "jis-b4" => Self::JisB4,
            "letter" => Self::Letter,
            "legal" => Self::Legal,
            "ledger" => Self::Ledger,
            _ => return None,
        })
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::A5 => "a5",
            Self::A4 => "a4",
            Self::A3 => "a3",
            Self::B5 => "b5",
            Self::B4 => "b4",
            Self::JisB5 => "jis-b5",
            Self::JisB4 => "jis-b4",
            Self::Letter => "letter",
            Self::Legal => "legal",
            Self::Ledger => "ledger",
        }
    }

    /// `(width, height)` in CSS px, portrait.
    #[must_use]
    pub fn size_px(self) -> (f32, f32) {
        let mm = |w: f32, h: f32| (w * PX_PER_MM, h * PX_PER_MM);
        let inch = |w: f32, h: f32| (w * PX_PER_IN, h * PX_PER_IN);
        match self {
            Self::A5 => mm(148.0, 210.0),
            Self::A4 => mm(210.0, 297.0),
            Self::A3 => mm(297.0, 420.0),
            Self::B5 => mm(176.0, 250.0),
            Self::B4 => mm(250.0, 353.0),
            Self::JisB5 => mm(182.0, 257.0),
            Self::JisB4 => mm(257.0, 364.0),
            Self::Letter => inch(8.5, 11.0),
            Self::Legal => inch(8.5, 14.0),
            Self::Ledger => inch(11.0, 17.0),
        }
    }
}

/// The `size` descriptor's value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageSize {
    /// `auto`: the embedder's paper decides.
    Auto,
    /// `portrait` / `landscape` alone: the embedder's paper, in that
    /// orientation.
    Orientation(Orientation),
    /// One or two lengths, in CSS px. One length makes a square.
    Lengths { width: f32, height: f32 },
    /// A named size, optionally with an orientation.
    Named {
        size: NamedSize,
        orientation: Option<Orientation>,
    },
}

/// A parsed `size` declaration: the value, its canonical serialization, and
/// its priority.
#[derive(Clone, Debug, PartialEq)]
pub struct SizeDeclaration {
    pub value: PageSize,
    pub text: String,
    pub important: bool,
}

/// What a `@page` rule declares that stylo's `PageRule` cannot hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageExtras {
    pub size: Option<SizeDeclaration>,
}

// === The side table ===

thread_local! {
    static EXTRAS: RefCell<Vec<(ServoArc<Locked<PageRule>>, PageExtras)>> =
        const { RefCell::new(Vec::new()) };
}

fn register(rule: &ServoArc<Locked<PageRule>>, extras: PageExtras) {
    EXTRAS.with(|table| {
        let mut table = table.borrow_mut();
        // A rule only this table still references is gone from every sheet.
        table.retain(|(arc, _)| !arc.is_unique());
        if let Some(entry) = table
            .iter_mut()
            .find(|(arc, _)| ServoArc::ptr_eq(arc, rule))
        {
            entry.1 = extras;
        } else if extras != PageExtras::default() {
            table.push((rule.clone(), extras));
        }
    });
}

/// The descriptors stored for `rule` beyond its declaration block.
#[must_use]
pub fn extras_of(rule: &ServoArc<Locked<PageRule>>) -> PageExtras {
    EXTRAS.with(|table| {
        table
            .borrow()
            .iter()
            .find(|(arc, _)| ServoArc::ptr_eq(arc, rule))
            .map(|(_, extras)| extras.clone())
            .unwrap_or_default()
    })
}

/// Replaces the descriptors stored for `rule`.
pub fn set_extras(rule: &ServoArc<Locked<PageRule>>, extras: PageExtras) {
    register(rule, extras);
}

// === Values ===

/// A non-negative absolute or font-relative length, in CSS px.
fn parse_length<'i>(input: &mut Parser<'i, '_>) -> Result<(f32, String), ParseError<'i, ()>> {
    let location = input.current_source_location();
    let (px, text) = match *input.next()? {
        Token::Dimension {
            value, ref unit, ..
        } => {
            let unit = unit.to_ascii_lowercase();
            let factor = match unit.as_str() {
                "px" => 1.0,
                "in" => PX_PER_IN,
                "cm" => PX_PER_MM * 10.0,
                "mm" => PX_PER_MM,
                "q" => PX_PER_MM / 4.0,
                "pt" => PX_PER_IN / 72.0,
                "pc" => PX_PER_IN / 6.0,
                "em" | "rem" => PX_PER_EM,
                _ => return Err(location.new_custom_error(())),
            };
            (value * factor, format!("{value}{unit}"))
        }
        Token::Number { value: 0.0, .. } => (0.0, "0px".to_owned()),
        _ => return Err(location.new_custom_error(())),
    };
    if px < 0.0 {
        return Err(location.new_custom_error(()));
    }
    Ok((px, text))
}

fn parse_orientation<'i>(input: &mut Parser<'i, '_>) -> Result<Orientation, ParseError<'i, ()>> {
    let location = input.current_source_location();
    let ident = input.expect_ident()?;
    match ident.to_ascii_lowercase().as_str() {
        "portrait" => Ok(Orientation::Portrait),
        "landscape" => Ok(Orientation::Landscape),
        _ => Err(location.new_custom_error(())),
    }
}

fn orientation_keyword(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::Portrait => "portrait",
        Orientation::Landscape => "landscape",
    }
}

/// css-page-3 `size`: `<length [0,∞]>{1,2} | auto | [ <page-size> ||
/// [ portrait | landscape ] ]`, with its canonical serialization.
fn parse_size_value<'i>(
    input: &mut Parser<'i, '_>,
) -> Result<(PageSize, String), ParseError<'i, ()>> {
    if input.try_parse(|i| i.expect_ident_matching("auto")).is_ok() {
        return Ok((PageSize::Auto, "auto".to_owned()));
    }
    if let Ok((width, first)) = input.try_parse(parse_length) {
        return Ok(match input.try_parse(parse_length) {
            Ok((height, second)) => (
                PageSize::Lengths { width, height },
                format!("{first} {second}"),
            ),
            Err(_) => (
                PageSize::Lengths {
                    width,
                    height: width,
                },
                first,
            ),
        });
    }
    let named = |i: &mut Parser<'i, '_>| -> Result<NamedSize, ParseError<'i, ()>> {
        let location = i.current_source_location();
        let ident = i.expect_ident()?;
        NamedSize::from_ident(ident).ok_or_else(|| location.new_custom_error(()))
    };
    let mut size = None;
    let mut orientation = None;
    // `||`: either order, each at most once.
    for _ in 0..2 {
        if size.is_none()
            && let Ok(found) = input.try_parse(named)
        {
            size = Some(found);
            continue;
        }
        if orientation.is_none()
            && let Ok(found) = input.try_parse(parse_orientation)
        {
            orientation = Some(found);
            continue;
        }
        break;
    }
    match (size, orientation) {
        (Some(size), orientation) => {
            let text = match orientation {
                Some(o) => format!("{} {}", size.keyword(), orientation_keyword(o)),
                None => size.keyword().to_owned(),
            };
            Ok((PageSize::Named { size, orientation }, text))
        }
        (None, Some(o)) => Ok((PageSize::Orientation(o), orientation_keyword(o).to_owned())),
        (None, None) => Err(input.new_custom_error(())),
    }
}

/// Parses a `size` value from text (`CSSStyleDeclaration.setProperty`).
#[must_use]
pub fn parse_size(text: &str) -> Option<(PageSize, String)> {
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    parser.parse_entirely(parse_size_value).ok()
}

/// Whether `name` is a page descriptor this module keeps outside stylo's
/// declaration block.
#[must_use]
pub fn is_side_descriptor(name: &str) -> bool {
    name.eq_ignore_ascii_case("size")
}

// === Parsing ===

fn page_context(url_data: &UrlExtraData) -> ParserContext<'_> {
    ParserContext::new(
        Origin::Author,
        url_data,
        Some(CssRuleType::Page),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Default::default(),
        None,
        None,
        Default::default(),
    )
}

/// Parses a page selector list (`CSSPageRule.selectorText`, an `@page`
/// prelude). `None` when it is invalid.
#[must_use]
pub fn parse_selectors(text: &str, url_data: &UrlExtraData) -> Option<PageSelectors> {
    let context = page_context(url_data);
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    parse_prelude(&context, &mut parser).ok()
}

fn parse_prelude<'i>(
    context: &ParserContext<'_>,
    input: &mut Parser<'i, '_>,
) -> Result<PageSelectors, ParseError<'i, ()>> {
    if input.is_exhausted() {
        return Ok(PageSelectors::default());
    }
    let selectors =
        PageSelectors::parse(context, input).map_err(|e| ParseError::from(e.basic()))?;
    input.expect_exhausted()?;
    Ok(selectors)
}

/// One `size` declaration inside a `@page` body.
struct SizeScan {
    found: Option<SizeDeclaration>,
}

impl<'i> DeclarationParser<'i> for SizeScan {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _start: &ParserState,
    ) -> Result<(), ParseError<'i, ()>> {
        if !is_side_descriptor(&name) {
            // Not ours: skip it, stylo has already read it.
            while input.next().is_ok() {}
            return Ok(());
        }
        let (value, text) = parse_size_value(input)?;
        let important = input.try_parse(cssparser::parse_important).is_ok();
        input.expect_exhausted()?;
        // A later declaration wins, unless an earlier one was `!important`
        // and this one is not.
        if self
            .found
            .as_ref()
            .is_none_or(|old| important || !old.important)
        {
            self.found = Some(SizeDeclaration {
                value,
                text,
                important,
            });
        }
        Ok(())
    }
}

impl<'i> AtRuleParser<'i> for SizeScan {
    type Prelude = ();
    type AtRule = ();
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for SizeScan {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();
}

impl<'i> RuleBodyItemParser<'i, (), ()> for SizeScan {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

/// Parses a `@page` body: stylo reads every property it knows into the block,
/// then a second pass over the same tokens picks out `size`.
fn parse_body<'i>(
    context: &ParserContext<'_>,
    input: &mut Parser<'i, '_>,
) -> (PropertyDeclarationBlock, PageExtras) {
    let start = input.state();
    let block = parse_property_declaration_list(context, input, &[]);
    input.reset(&start);
    let mut scan = SizeScan { found: None };
    for item in RuleBodyParser::new(input, &mut scan) {
        // Invalid declarations are dropped, as in any declaration list.
        let _ = item;
    }
    (block, PageExtras { size: scan.found })
}

/// A `@page` rule found while scanning a sheet's source, with where it goes.
struct Found {
    /// The start locations of the group rules it is nested in, outermost
    /// first; empty at the top level.
    path: Vec<SourceLocation>,
    location: SourceLocation,
    selectors: PageSelectors,
    block: PropertyDeclarationBlock,
    extras: PageExtras,
}

/// Walks a rule list, recording every `@page` and descending into the group
/// rules stylo keeps a nested list for.
struct Scanner<'a, 'b> {
    context: &'a ParserContext<'b>,
    path: Vec<SourceLocation>,
    found: Vec<Found>,
}

/// The group at-rules whose nested list can hold a `@page`.
fn is_group_rule(name: &str) -> bool {
    [
        "media",
        "supports",
        "layer",
        "container",
        "document",
        "-moz-document",
    ]
    .iter()
    .any(|group| name.eq_ignore_ascii_case(group))
}

enum Prelude {
    Page(PageSelectors),
    Group,
}

impl<'i> AtRuleParser<'i> for Scanner<'_, '_> {
    type Prelude = Prelude;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Prelude, ParseError<'i, ()>> {
        if name.eq_ignore_ascii_case("page") {
            return parse_prelude(self.context, input).map(Prelude::Page);
        }
        while input.next().is_ok() {}
        if is_group_rule(&name) {
            Ok(Prelude::Group)
        } else {
            Err(input.new_custom_error(()))
        }
    }

    fn rule_without_block(&mut self, _: Prelude, _: &ParserState) -> Result<(), ()> {
        Err(())
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Prelude,
        start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        let location = start.source_location();
        match prelude {
            Prelude::Page(selectors) => {
                let (block, extras) = parse_body(self.context, input);
                self.found.push(Found {
                    path: self.path.clone(),
                    location,
                    selectors,
                    block,
                    extras,
                });
            }
            Prelude::Group => {
                self.path.push(location);
                for item in StyleSheetParser::new(input, self) {
                    let _ = item;
                }
                self.path.pop();
            }
        }
        Ok(())
    }
}

impl<'i> QualifiedRuleParser<'i> for Scanner<'_, '_> {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(&mut self, input: &mut Parser<'i, 't>) -> Result<(), ParseError<'i, ()>> {
        while input.next().is_ok() {}
        Ok(())
    }

    fn parse_block<'t>(
        &mut self,
        _: (),
        _: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        // A style rule's body: a `@page` nested in one is invalid.
        while input.next().is_ok() {}
        Ok(())
    }
}

fn new_rule(lock: &SharedRwLock, found: Found) -> (CssRule, ServoArc<Locked<PageRule>>) {
    let rule = ServoArc::new(lock.wrap(PageRule {
        selectors: found.selectors,
        rules: CssRules::new(Vec::new(), lock),
        block: ServoArc::new(lock.wrap(found.block)),
        source_location: found.location,
    }));
    register(&rule, found.extras);
    (CssRule::Page(rule.clone()), rule)
}

/// Where a rule starts in its sheet's source, for the rules that record it.
fn rule_location(rule: &CssRule, guard: &SharedRwLockReadGuard<'_>) -> Option<SourceLocation> {
    Some(match rule {
        CssRule::Style(r) => r.read_with(guard).source_location,
        CssRule::Namespace(r) => r.source_location,
        CssRule::Import(r) => r.read_with(guard).source_location,
        CssRule::Media(r) => r.source_location,
        CssRule::CustomMedia(r) => r.source_location,
        CssRule::Container(r) => r.source_location,
        CssRule::FontFace(r) => r.read_with(guard).source_location,
        CssRule::FontFeatureValues(r) => r.source_location,
        CssRule::FontPaletteValues(r) => r.source_location,
        CssRule::Keyframes(r) => r.read_with(guard).source_location,
        CssRule::Supports(r) => r.source_location,
        CssRule::Page(r) => r.read_with(guard).source_location,
        CssRule::Property(r) => r.source_location,
        CssRule::Document(r) => r.source_location,
        CssRule::LayerBlock(r) => r.source_location,
        CssRule::LayerStatement(r) => r.source_location,
        CssRule::Scope(r) => r.source_location,
        CssRule::StartingStyle(r) => r.source_location,
        _ => return None,
    })
}

/// The nested list of a group rule that can hold a `@page`.
fn nested_rules(rule: &CssRule) -> Option<ServoArc<Locked<CssRules>>> {
    Some(match rule {
        CssRule::Media(r) => r.rules.clone(),
        CssRule::Supports(r) => r.rules.clone(),
        CssRule::LayerBlock(r) => r.rules.clone(),
        CssRule::Container(r) => r.rules.clone(),
        CssRule::Document(r) => r.rules.clone(),
        _ => return None,
    })
}

fn before(a: SourceLocation, b: SourceLocation) -> bool {
    (a.line, a.column) < (b.line, b.column)
}

/// Finds the `@page` rules in `css` and splices each into `rules` — the list
/// stylo built from the same text — at its source position, descending into
/// the group rule it is nested in.
///
/// A `@page` whose enclosing group rule stylo rejected has nowhere to go and is
/// dropped with it, as it would be by a parser that knew `@page`.
pub fn splice_into(
    lock: &SharedRwLock,
    rules: &ServoArc<Locked<CssRules>>,
    css: &str,
    url_data: &UrlExtraData,
) {
    if !contains_page_keyword(css) {
        return;
    }
    let context = page_context(url_data);
    let mut scanner = Scanner {
        context: &context,
        path: Vec::new(),
        found: Vec::new(),
    };
    {
        let mut input = ParserInput::new(css);
        let mut parser = Parser::new(&mut input);
        for item in StyleSheetParser::new(&mut parser, &mut scanner) {
            let _ = item;
        }
    }
    for found in scanner.found {
        // Resolve the target list under a read guard, then insert under a
        // write guard: the lock is not reentrant.
        let target = {
            let guard = lock.read();
            let mut list = rules.clone();
            let mut ok = true;
            for group in &found.path {
                let next = list.read_with(&guard).0.iter().find_map(|rule| {
                    (rule_location(rule, &guard) == Some(*group))
                        .then(|| nested_rules(rule))
                        .flatten()
                });
                match next {
                    Some(next) => list = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            ok.then(|| {
                let index = list
                    .read_with(&guard)
                    .0
                    .iter()
                    .take_while(|rule| {
                        rule_location(rule, &guard)
                            .is_none_or(|location| before(location, found.location))
                    })
                    .count();
                (list, index)
            })
        };
        let Some((list, index)) = target else {
            continue;
        };
        let (rule, _) = new_rule(lock, found);
        let mut write = lock.write();
        list.write_with(&mut write).0.insert(index, rule);
    }
}

/// A cheap pre-check, so a sheet without `@page` costs one substring search.
fn contains_page_keyword(css: &str) -> bool {
    css.as_bytes()
        .windows(5)
        .any(|w| w[0] == b'@' && w[1..].eq_ignore_ascii_case(b"page"))
}

/// Parses `text` as a single `@page` rule (`insertRule`). `None` when it is
/// not one, or is invalid.
#[must_use]
pub fn parse_rule(lock: &SharedRwLock, text: &str, url_data: &UrlExtraData) -> Option<CssRule> {
    let trimmed = text.trim_start();
    if !trimmed
        .get(..5)
        .is_some_and(|head| head.eq_ignore_ascii_case("@page"))
    {
        return None;
    }
    let context = page_context(url_data);
    let mut scanner = Scanner {
        context: &context,
        path: Vec::new(),
        found: Vec::new(),
    };
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    let mut count = 0;
    for item in StyleSheetParser::new(&mut parser, &mut scanner) {
        count += 1;
        let _ = item;
    }
    if count != 1 || scanner.found.len() != 1 {
        return None;
    }
    let found = scanner.found.pop()?;
    Some(new_rule(lock, found).0)
}

// === CSSOM ===

/// `CSSPageRule.selectorText`.
#[must_use]
pub fn selector_text(lock: &SharedRwLock, rule: &ServoArc<Locked<PageRule>>) -> String {
    let guard = lock.read();
    rule.read_with(&guard).selectors.to_css_string()
}

/// Sets `CSSPageRule.selectorText`; an invalid selector leaves the rule as it
/// was. Returns whether it changed.
pub fn set_selector_text(
    lock: &SharedRwLock,
    rule: &ServoArc<Locked<PageRule>>,
    text: &str,
    url_data: &UrlExtraData,
) -> bool {
    let Some(selectors) = parse_selectors(text, url_data) else {
        return false;
    };
    let mut write = lock.write();
    rule.write_with(&mut write).selectors = selectors;
    true
}

/// The `cssText` of a page rule's declarations: the side descriptors first,
/// then stylo's block.
#[must_use]
pub fn declarations_css(lock: &SharedRwLock, rule: &ServoArc<Locked<PageRule>>) -> String {
    let guard = lock.read();
    let mut out = String::new();
    if let Some(size) = extras_of(rule).size {
        out.push_str("size: ");
        out.push_str(&size.text);
        if size.important {
            out.push_str(" !important");
        }
        out.push(';');
    }
    let mut block = String::new();
    let _ = rule
        .read_with(&guard)
        .block
        .read_with(&guard)
        .to_css(&mut block);
    if !block.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&block);
    }
    out
}

/// `CSSPageRule.cssText`: `@page`, the selectors if any, and the
/// declarations in braces (`@page { }` when there are none).
#[must_use]
pub fn rule_css_text(lock: &SharedRwLock, rule: &ServoArc<Locked<PageRule>>) -> String {
    let selectors = selector_text(lock, rule);
    let declarations = declarations_css(lock, rule);
    let mut out = String::from("@page ");
    if !selectors.is_empty() {
        out.push_str(&selectors);
        out.push(' ');
    }
    out.push('{');
    out.push(' ');
    if !declarations.is_empty() {
        out.push_str(&declarations);
        out.push(' ');
    }
    // Not stylo's `PageRule::to_css`, which knows nothing of `size`.
    out.push('}');
    out
}

// === Resolution ===

/// A page margin as declared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarginValue {
    Px(f32),
    /// Of the page width for left and right, of its height for top and bottom.
    Percent(f32),
}

impl MarginValue {
    /// The margin in CSS px for a page `extent` px along its axis.
    #[must_use]
    pub fn resolve(self, extent: f32) -> f32 {
        match self {
            Self::Px(px) => px,
            Self::Percent(p) => p / 100.0 * extent,
        }
    }
}

/// The page style every page shares: what the selector-less `@page` rules
/// declare, cascaded.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageStyle {
    pub size: Option<PageSize>,
    /// `[top, right, bottom, left]`.
    pub margins: [Option<MarginValue>; 4],
}

fn parse_margin(text: &str) -> Option<MarginValue> {
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    parser
        .parse_entirely(|input| {
            if let Ok(p) = input.try_parse(|i| i.expect_percentage()) {
                return Ok(MarginValue::Percent(p * 100.0));
            }
            parse_length(input).map(|(px, _)| MarginValue::Px(px))
        })
        .ok()
}

/// Cascades `rules` — page rules in cascade order — into the page style.
///
/// Only rules without selectors apply: every page of an export shares one
/// paper size and one set of margins, so `:first`, `:left`, `:right`, `:blank`
/// and named pages have nothing to select. `auto` and anything that is not a
/// plain length or percentage (a `calc()`, say) leaves the side to the
/// embedder.
#[must_use]
pub fn resolve(lock: &SharedRwLock, rules: &[ServoArc<Locked<PageRule>>]) -> PageStyle {
    let guard = lock.read();
    let mut style = PageStyle::default();
    let mut size_important = false;
    let mut margin_important = [false; 4];
    const SIDES: [&str; 4] = ["margin-top", "margin-right", "margin-bottom", "margin-left"];
    for rule in rules {
        let page = rule.read_with(&guard);
        if !page.selectors.is_empty() {
            continue;
        }
        if let Some(size) = extras_of(rule).size
            && (size.important || !size_important)
        {
            size_important = size.important;
            style.size = Some(size.value);
        }
        let block = page.block.read_with(&guard);
        for (i, side) in SIDES.iter().enumerate() {
            let value = crate::cssom::block_get(block, side);
            if value.is_empty() {
                continue;
            }
            let important = crate::cssom::block_is_important(block, side);
            if !important && margin_important[i] {
                continue;
            }
            margin_important[i] = important;
            style.margins[i] = parse_margin(&value);
        }
    }
    style
}

// === CSSOM declarations (`CSSPageRule.style`) ===
//
// A page rule's `style` is stylo's declaration block plus the side table. The
// bindings ask these first; `None` means "not a page rule, or not a side
// descriptor", and the ordinary block path answers.

fn page_of(rule: &CssRule) -> Option<&ServoArc<Locked<PageRule>>> {
    match rule {
        CssRule::Page(page) => Some(page),
        _ => None,
    }
}

/// `cssText` of a page rule's declarations.
#[must_use]
pub fn decl_css_text(lock: &SharedRwLock, rule: &CssRule) -> Option<String> {
    page_of(rule).map(|page| declarations_css(lock, page))
}

/// Replaces a page rule's declarations from `text` (the `cssText` setter),
/// read the way an `@page` body is. Returns whether `rule` is a page rule.
pub fn decl_set_css_text(
    lock: &SharedRwLock,
    rule: &CssRule,
    text: &str,
    url_data: &UrlExtraData,
) -> bool {
    let Some(page) = page_of(rule) else {
        return false;
    };
    let context = page_context(url_data);
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    let (block, extras) = parse_body(&context, &mut parser);
    {
        let mut write = lock.write();
        let block_lock = page.write_with(&mut write).block.clone();
        *block_lock.write_with(&mut write) = block;
    }
    register(page, extras);
    true
}

/// The declared names of a page rule: the side descriptors, then the block's.
#[must_use]
pub fn decl_names(lock: &SharedRwLock, rule: &CssRule) -> Option<Vec<String>> {
    let page = page_of(rule)?;
    let mut names = Vec::new();
    if extras_of(page).size.is_some() {
        names.push("size".to_owned());
    }
    let guard = lock.read();
    let block = page.read_with(&guard).block.clone();
    names.extend(crate::cssom::block_names(block.read_with(&guard)));
    Some(names)
}

/// `getPropertyValue` for a side descriptor of a page rule.
#[must_use]
pub fn decl_get(rule: &CssRule, name: &str) -> Option<String> {
    let page = page_of(rule).filter(|_| is_side_descriptor(name))?;
    Some(
        extras_of(page)
            .size
            .map(|size| size.text)
            .unwrap_or_default(),
    )
}

/// `getPropertyPriority` for a side descriptor of a page rule.
#[must_use]
pub fn decl_is_important(rule: &CssRule, name: &str) -> Option<bool> {
    let page = page_of(rule).filter(|_| is_side_descriptor(name))?;
    Some(extras_of(page).size.is_some_and(|size| size.important))
}

/// `setProperty` for a side descriptor of a page rule. An invalid value is
/// ignored, as `setProperty` ignores any; an empty one removes it.
#[must_use]
pub fn decl_set(rule: &CssRule, name: &str, value: &str, important: bool) -> Option<bool> {
    let page = page_of(rule).filter(|_| is_side_descriptor(name))?;
    if value.trim().is_empty() {
        return decl_remove(rule, name).map(|old| !old.is_empty());
    }
    let Some((value, text)) = parse_size(value) else {
        return Some(false);
    };
    let mut extras = extras_of(page);
    extras.size = Some(SizeDeclaration {
        value,
        text,
        important,
    });
    register(page, extras);
    Some(true)
}

/// `removeProperty` for a side descriptor of a page rule: the old value.
#[must_use]
pub fn decl_remove(rule: &CssRule, name: &str) -> Option<String> {
    let page = page_of(rule).filter(|_| is_side_descriptor(name))?;
    let mut extras = extras_of(page);
    let old = extras.size.take().map(|size| size.text).unwrap_or_default();
    register(page, extras);
    Some(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Option<String> {
        parse_size(value).map(|(_, text)| text)
    }

    #[test]
    fn size_accepts_its_grammar_and_serializes_canonically() {
        assert_eq!(text("auto").as_deref(), Some("auto"));
        assert_eq!(text("A4").as_deref(), Some("a4"));
        assert_eq!(
            text("landscape JIS-B5").as_deref(),
            Some("jis-b5 landscape")
        );
        assert_eq!(text("portrait").as_deref(), Some("portrait"));
        assert_eq!(text("216mm").as_deref(), Some("216mm"));
        assert_eq!(text("5in 7in").as_deref(), Some("5in 7in"));
        assert_eq!(
            parse_size("2in").map(|(value, _)| value),
            Some(PageSize::Lengths {
                width: 192.0,
                height: 192.0
            })
        );
    }

    #[test]
    fn size_rejects_what_is_outside_it() {
        for bad in [
            "a4 a4",
            "landscape portrait",
            "-1in",
            "10%",
            "calc(1in)",
            "1in 2in 3in",
            "",
        ] {
            assert_eq!(text(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_margin_is_a_length_or_a_percentage() {
        assert_eq!(parse_margin("1in"), Some(MarginValue::Px(96.0)));
        assert_eq!(parse_margin("10%"), Some(MarginValue::Percent(10.0)));
        assert_eq!(parse_margin("auto"), None);
    }
}
