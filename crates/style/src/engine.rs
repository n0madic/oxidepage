//! The style engine: owns stylo's [`Stylist`], the document's stylesheet set,
//! and drives the (sequential) style traversal that produces computed values
//! (design doc §10, ADR-0005).

use std::collections::BTreeMap;
use std::sync::Mutex;

use encoding_rs::Encoding;
use oxidepage_dom::DomTree;
use oxidepage_dom::NodeId;
use oxidepage_dom::select::{NodeRef, enter_active_tree};
use oxidepage_dom::stylo::node_id_from_opaque;
use servo_arc::Arc as ServoArc;
use style::Atom;
use style::animation::DocumentAnimationSet;
use style::context::{
    QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext,
    StyleContext,
};
use style::dom::{TElement, TNode};
use style::driver;
use style::global_style_data::GLOBAL_STYLE_DATA;
use style::invalidation::stylesheets::RuleChangeKind;
use style::media_queries::{MediaList, MediaType};
use style::properties::ComputedValues;
use style::properties::style_structs::Font;
use style::queries::values::PrefersColorScheme;
use style::servo::media_features::PointerCapabilities;
use style::shared_lock::{Locked, SharedRwLock, StylesheetGuards};
use style::stylesheets::{
    AllowImportRules, CssRule, CssRuleTypes, CustomMediaEvaluator, DocumentStyleSheet, Origin,
    RulesMutateError, Stylesheet, StylesheetInDocument, StylesheetLoader, UrlExtraData,
};
use style::stylist::Stylist;
use style::thread_state::{self, ThreadState};
use style::traversal::{DomTraversal, PerLevelTraversalData, recalc_style_at};
use style::traversal_flags::TraversalFlags;

use style::author_styles::AuthorStyles;
use style::stylesheets::CustomMediaMap;

use crate::fonts::NoopFontMetricsProvider;

/// A comparable key that orders nodes in document (tree) order: the child index
/// of each node on the path from the document root, root-first. Lexicographic
/// comparison of these paths is exactly tree order.
type TreeOrderKey = Vec<u32>;

fn tree_order_key(tree: &DomTree, node: NodeId) -> TreeOrderKey {
    let mut path = Vec::new();
    let mut current = node;
    while let Some(parent) = tree.node(current).parent() {
        let index = tree
            .children(parent)
            .position(|c| c == current)
            .unwrap_or(0);
        path.push(index as u32);
        current = parent;
    }
    path.reverse();
    path
}

/// The rendering viewport (CSS pixels) and device-pixel ratio used to build the
/// stylo [`Device`](style::device::Device) for media-query evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub dpr: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 800.0,
            height: 600.0,
            dpr: 1.0,
        }
    }
}

/// Factory for the device's [`FontMetricsProvider`]: `Device` wants an owned
/// `Box`, and the device is rebuilt on every viewport change, so the engine
/// stores a factory instead of a provider. The layout engine installs a real
/// (parley/skrifa-backed) one; the default reports no metrics.
pub type FontMetricsProviderFactory =
    std::sync::Arc<dyn Fn() -> Box<dyn style::device::servo::FontMetricsProvider>>;

fn make_device(viewport: Viewport, metrics: &FontMetricsProviderFactory) -> style::device::Device {
    // The virtual display fills the viewport exactly, so `device-width` and
    // `device-height` report the viewport scaled into device pixels.
    let device_size = euclid::Size2D::new(
        viewport.width * viewport.dpr,
        viewport.height * viewport.dpr,
    );
    // A headless page has no pointing device and cannot hover, so `pointer`,
    // `any-pointer`, `hover`, and `any-hover` answer `none`.
    let pointer_capabilities = PointerCapabilities::empty();
    style::device::Device::new(
        MediaType::screen(),
        QuirksMode::NoQuirks,
        euclid::Size2D::new(viewport.width, viewport.height),
        device_size,
        euclid::Scale::new(viewport.dpr),
        metrics(),
        ComputedValues::initial_values_with_font_override(Font::initial_values()),
        PrefersColorScheme::Light,
        pointer_capabilities,
        pointer_capabilities,
    )
}

/// A speculative-painter registry with no painters (paint worklets unsupported).
struct NoPainters;
impl RegisteredSpeculativePainters for NoPainters {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

/// The author styles scoped to one shadow root (ADR-0010): its `<style>`/
/// `<link>` sheets in tree order plus its `adoptedStyleSheets`, compiled into
/// a stylo [`AuthorStyles`] whose `CascadeData` the cascade reads through
/// `TShadowRoot::style_data`.
struct ShadowScope {
    node_sheets: BTreeMap<TreeOrderKey, (NodeId, DocumentStyleSheet)>,
    node_keys: std::collections::HashMap<NodeId, TreeOrderKey>,
    adopted: Vec<DocumentStyleSheet>,
    styles: AuthorStyles<DocumentStyleSheet>,
    /// The sheet list changed; `styles` is rebuilt from scratch before the
    /// next cascade (v1 simplicity — the flush recomputes fully anyway).
    dirty: bool,
}

impl ShadowScope {
    fn new() -> Self {
        Self {
            node_sheets: BTreeMap::new(),
            node_keys: std::collections::HashMap::new(),
            adopted: Vec::new(),
            styles: AuthorStyles::new(),
            dirty: false,
        }
    }
}

/// The document's style engine.
pub struct StyleEngine {
    stylist: Stylist,
    /// Clone of the document's shared style lock (must be the same lock the
    /// [`DomTree`] uses so locked stylesheet/attribute data is readable here).
    lock: SharedRwLock,
    animations: DocumentAnimationSet,
    /// Author stylesheets keyed by document-order position; the value carries
    /// the owning node so a sheet can be removed/replaced when its node changes.
    node_sheets: BTreeMap<TreeOrderKey, (NodeId, DocumentStyleSheet)>,
    /// Reverse map node → its current order key, to locate a node's sheet.
    node_keys: std::collections::HashMap<NodeId, TreeOrderKey>,
    /// The document's `adoptedStyleSheets`, appended after all node sheets.
    doc_adopted: Vec<DocumentStyleSheet>,
    /// Shadow-scoped author styles, keyed by shadow root. These sheets never
    /// enter the document `Stylist`; each scope flushes to its own
    /// `CascadeData`, written back into the tree (`set_shadow_cascade`).
    shadow_scopes: std::collections::HashMap<NodeId, ShadowScope>,
    ua_sheet: DocumentStyleSheet,
    viewport: Viewport,
    /// Monotonic counter bumped on every author-stylesheet/rule mutation, so
    /// CSSOM computed-value views can cache and invalidate exactly.
    version: std::cell::Cell<u64>,
    /// Builds the font-metrics provider for each (re)created device.
    metrics_factory: FontMetricsProviderFactory,
    /// Elements visited by the most recent [`Self::resolve_styles`] traversal.
    restyled: Vec<NodeId>,
}

impl StyleEngine {
    /// Builds a style engine for `tree` with the given `viewport`, installing
    /// the built-in user-agent stylesheet.
    #[must_use]
    pub fn new(tree: &DomTree, viewport: Viewport) -> Self {
        // Enable the layout features stylo gates behind static prefs so the
        // corresponding CSS parses and cascades (grid, multicol, …).
        style_config::set_pref!("layout.grid.enabled", true);
        style_config::set_pref!("layout.columns.enabled", true);
        style_config::set_pref!("layout.css.basic-shape-shape.enabled", true);
        style_config::set_pref!("layout.threads", 1);

        let metrics_factory: FontMetricsProviderFactory =
            std::sync::Arc::new(|| Box::new(NoopFontMetricsProvider));
        let device = make_device(viewport, &metrics_factory);
        let mut stylist = Stylist::new(device, QuirksMode::NoQuirks);
        let lock = tree.style_lock().clone();

        let ua_sheet = make_stylesheet_impl(
            &lock,
            tree.url_extra_data(),
            include_str!("../assets/ua.css"),
            Origin::UserAgent,
            None,
            None,
        );
        stylist.append_stylesheet(ua_sheet.clone(), &lock.read());

        Self {
            stylist,
            lock,
            animations: DocumentAnimationSet::default(),
            node_sheets: BTreeMap::new(),
            node_keys: std::collections::HashMap::new(),
            doc_adopted: Vec::new(),
            shadow_scopes: std::collections::HashMap::new(),
            ua_sheet,
            viewport,
            version: std::cell::Cell::new(0),
            metrics_factory,
            restyled: Vec::new(),
        }
    }

    /// Installs a real font-metrics provider factory (from the layout
    /// engine's font system) and rebuilds the device so `ex`/`ch`/`ic`
    /// units resolve against actual font metrics.
    pub fn set_font_metrics_provider(&mut self, factory: FontMetricsProviderFactory) {
        self.metrics_factory = factory;
        // Rebuild the device with the new provider (also re-dirties origins).
        self.set_viewport(self.viewport);
    }

    /// The shared style lock (for CSSOM code that wraps/reads locked data).
    #[must_use]
    pub fn lock(&self) -> &SharedRwLock {
        &self.lock
    }

    /// A counter that increases on every author-stylesheet/rule mutation. CSSOM
    /// computed-value views pair it with [`DomTree::style_version`] to cache.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.get()
    }

    /// The elements restyled since the last [`Self::take_restyled_nodes`]: a
    /// tight superset of those whose computed style changed (stylo only
    /// descends into elements with a restyle hint or dirty descendants).
    /// Layout diffs just these instead of walking every styled element.
    #[must_use]
    pub fn restyled_nodes(&self) -> &[NodeId] {
        &self.restyled
    }

    /// Drains [`Self::restyled_nodes`].
    ///
    /// The set *accumulates* across [`Self::resolve_styles`] calls and is only
    /// emptied here, because a restyle is often resolved before the consumer
    /// runs: `getComputedStyle` (and the page's `background-image` scan) call
    /// `resolve_styles` themselves, which consumes stylo's dirty bits. Clearing
    /// per resolve would leave the following reflow with an empty set, and it
    /// would patch nothing while believing it had patched everything — a stale
    /// box tree. Layout drains this once per reflow instead.
    pub fn take_restyled_nodes(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.restyled)
    }

    fn bump_version(&self) {
        self.version.set(self.version.get() + 1);
    }

    /// The current viewport.
    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Author stylesheets in document order, as `(node, sheet)` pairs.
    pub fn author_sheets(&self) -> impl Iterator<Item = (NodeId, &DocumentStyleSheet)> {
        self.node_sheets.values().map(|(n, s)| (*n, s))
    }

    /// The user-agent stylesheet.
    #[must_use]
    pub fn ua_sheet(&self) -> &DocumentStyleSheet {
        &self.ua_sheet
    }

    /// The document's effective `@font-face` rules (Phase 7, WP-B): walks each
    /// author sheet's *effective* rules — descending into `@media`/`@supports`/
    /// `@layer`/`@import` with media evaluation — and lifts every `@font-face`
    /// into a [`FontFaceInfo`](crate::font_faces::FontFaceInfo). Rules without a
    /// usable family/src are skipped.
    ///
    /// `effective_rules` works on the sheet *contents*, which know nothing about
    /// the sheet wrapper's own `disabled` flag or `<style media="print">` list.
    /// Both are checked here, so a disabled or non-matching sheet contributes no
    /// font loads.
    #[must_use]
    pub fn font_faces(&self) -> Vec<crate::font_faces::FontFaceInfo> {
        use style::stylesheets::StylesheetInDocument;

        let guard = self.lock.read();
        let device = self.stylist.device();
        // `@custom-media` is an experimental feature we don't track globally, so
        // effective-rule evaluation uses an empty map (v1 limitation, ADR-0008).
        let custom_media = style::stylesheets::CustomMediaMap::default();
        let mut out = Vec::new();
        // Fonts are global even when declared inside a shadow tree or an
        // adopted sheet, so every scope's sheets are scanned.
        let shadow_sheets = self.shadow_scopes.values().flat_map(|scope| {
            scope
                .node_sheets
                .values()
                .map(|(_, sheet)| sheet)
                .chain(scope.adopted.iter())
        });
        let sheets = self
            .author_sheets()
            .map(|(_, sheet)| sheet)
            .chain(self.doc_adopted.iter())
            .chain(shadow_sheets);
        for sheet in sheets {
            if !sheet.enabled() || !sheet.is_effective_for_device(device, &custom_media, &guard) {
                continue;
            }
            let contents = sheet.contents(&guard);
            for rule in contents.effective_rules(device, &custom_media, &guard) {
                if let CssRule::FontFace(locked) = rule {
                    let font_face = locked.read_with(&guard);
                    if let Some(info) = crate::font_faces::from_rule(font_face) {
                        out.push(info);
                    }
                }
            }
        }
        out
    }

    /// Replaces the viewport, rebuilding the stylo device (re-evaluates media
    /// queries on the next resolution).
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.bump_version();
        self.viewport = viewport;
        let device = make_device(viewport, &self.metrics_factory);
        let guard = self.lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        // Origins whose media evaluation changed must be reprocessed so the
        // next resolution re-cascades the affected elements.
        let origins = self.stylist.set_device(device, &guards);
        drop(guard);
        self.stylist.force_stylesheet_origins_dirty(origins);
    }

    /// Evaluates a `matchMedia()` query against the same device used by the
    /// stylesheet cascade. Invalid lists parse as `not all` and return false.
    #[must_use]
    pub fn media_query_matches(&self, query: &str) -> bool {
        let url_data =
            UrlExtraData::from(url::Url::parse("about:blank").expect("about:blank is a valid URL"));
        let list = crate::cssom::parse_media_list(query, &url_data);
        list.evaluate(
            self.stylist.device(),
            QuirksMode::NoQuirks,
            &mut CustomMediaEvaluator::none(),
        )
    }

    /// Parses `css` into an author stylesheet owned by the document.
    #[must_use]
    pub fn make_stylesheet(&self, css: &str, url_data: &UrlExtraData) -> DocumentStyleSheet {
        make_stylesheet_impl(&self.lock, url_data, css, Origin::Author, None, None)
    }

    /// Parses `css` into an author stylesheet with an optional sheet-level
    /// `media` query, resolving `@import` via `loader`.
    #[must_use]
    pub fn make_stylesheet_with_loader(
        &self,
        css: &str,
        url_data: &UrlExtraData,
        media: Option<&str>,
        loader: Option<&dyn StylesheetLoader>,
    ) -> DocumentStyleSheet {
        make_stylesheet_impl(&self.lock, url_data, css, Origin::Author, media, loader)
    }

    /// Inserts (or replaces) the author stylesheet for `node`, ordered by the
    /// node's position in the document. A node living inside a shadow tree
    /// routes into that root's [`ShadowScope`] instead of the document
    /// `Stylist` (shadow styles must not leak into the document cascade).
    pub fn add_sheet_for_node(&mut self, tree: &DomTree, node: NodeId, sheet: DocumentStyleSheet) {
        if let Some(root) = tree.containing_shadow_root(node) {
            self.bump_version();
            let scope = self
                .shadow_scopes
                .entry(root)
                .or_insert_with(ShadowScope::new);
            if let Some(key) = scope.node_keys.remove(&node) {
                scope.node_sheets.remove(&key);
            }
            let key = tree_order_key(tree, node);
            scope.node_sheets.insert(key.clone(), (node, sheet));
            scope.node_keys.insert(node, key);
            scope.dirty = true;
            return;
        }
        self.bump_version();
        self.remove_sheet_for_node(node);

        let key = tree_order_key(tree, node);
        // Find the next sheet in document order to insert before, falling back
        // to the first adopted sheet (adopted sheets order after node sheets).
        let insertion_point = self
            .node_sheets
            .range(key.clone()..)
            .next()
            .map(|(_, (_, sheet))| sheet.clone())
            .or_else(|| self.doc_adopted.first().cloned());

        let guard = self.lock.read();
        if let Some(before) = insertion_point {
            self.stylist
                .insert_stylesheet_before(sheet.clone(), before, &guard);
        } else {
            self.stylist.append_stylesheet(sheet.clone(), &guard);
        }
        drop(guard);

        self.node_sheets.insert(key.clone(), (node, sheet));
        self.node_keys.insert(node, key);
    }

    /// Removes the author stylesheet owned by `node`, if any (document or
    /// shadow scope).
    pub fn remove_sheet_for_node(&mut self, node: NodeId) {
        self.bump_version();
        if let Some(key) = self.node_keys.remove(&node)
            && let Some((_, sheet)) = self.node_sheets.remove(&key)
        {
            self.stylist.remove_stylesheet(sheet, &self.lock.read());
            return;
        }
        for scope in self.shadow_scopes.values_mut() {
            if let Some(key) = scope.node_keys.remove(&node) {
                scope.node_sheets.remove(&key);
                scope.dirty = true;
                return;
            }
        }
    }

    /// The document-order sheet (with owner node) for `node`, if present
    /// (document or shadow scope).
    #[must_use]
    pub fn sheet_for_node(&self, node: NodeId) -> Option<&DocumentStyleSheet> {
        if let Some(sheet) = self
            .node_keys
            .get(&node)
            .and_then(|key| self.node_sheets.get(key))
            .map(|(_, sheet)| sheet)
        {
            return Some(sheet);
        }
        self.shadow_scopes.values().find_map(|scope| {
            scope
                .node_keys
                .get(&node)
                .and_then(|key| scope.node_sheets.get(key))
                .map(|(_, sheet)| sheet)
        })
    }

    /// Replaces the `adoptedStyleSheets` of a scope: `None` targets the
    /// document (sheets join the document `Stylist` after all node sheets),
    /// `Some(root)` targets a shadow root's scope.
    ///
    /// The list may name the same sheet more than once — `adoptedStyleSheets`
    /// is a plain observable array, and duplicates are legal. A sheet
    /// participates in the cascade once, at its *last* position (so
    /// `[b, a, b]` cascades as `[a, b]`), which is also the only shape stylo's
    /// sheet set accepts: appending a sheet it already holds trips an assertion
    /// inside `StylesheetSet::append` and takes the process down.
    pub fn set_adopted_sheets(&mut self, scope: Option<NodeId>, sheets: Vec<DocumentStyleSheet>) {
        let sheets = dedupe_keeping_last(sheets);
        self.bump_version();
        match scope {
            None => {
                let guard = self.lock.read();
                for old in std::mem::take(&mut self.doc_adopted) {
                    self.stylist.remove_stylesheet(old, &guard);
                }
                for sheet in &sheets {
                    self.stylist.append_stylesheet(sheet.clone(), &guard);
                }
                drop(guard);
                self.doc_adopted = sheets;
            }
            Some(root) => {
                let scope = self
                    .shadow_scopes
                    .entry(root)
                    .or_insert_with(ShadowScope::new);
                scope.adopted = sheets;
                scope.dirty = true;
            }
        }
    }

    /// A constructed stylesheet's contents changed in place (`replaceSync`).
    /// The sheet may be adopted by any number of scopes, so all author data is
    /// conservatively rebuilt on the next flush.
    pub fn note_constructed_sheet_changed(&mut self) {
        self.bump_version();
        self.stylist
            .force_stylesheet_origins_dirty(Origin::Author.into());
        for scope in self.shadow_scopes.values_mut() {
            scope.styles.stylesheets.force_dirty();
        }
    }

    /// Rebuilds and flushes every dirty shadow scope, writing the resulting
    /// `CascadeData` back into the tree for `TShadowRoot::style_data`. Scopes
    /// whose root has been freed are pruned.
    fn flush_shadow_scopes(&mut self, tree: &mut DomTree) {
        self.shadow_scopes
            .retain(|&root, _| tree.is_shadow_root(root));
        let custom_media = CustomMediaMap::default();
        for (&root, scope) in &mut self.shadow_scopes {
            let guard = self.lock.read();
            if scope.dirty {
                let mut styles = AuthorStyles::new();
                let sheets = scope
                    .node_sheets
                    .values()
                    .map(|(_, sheet)| sheet)
                    .chain(scope.adopted.iter());
                for sheet in sheets {
                    styles.stylesheets.append_stylesheet(
                        None,
                        &custom_media,
                        sheet.clone(),
                        &guard,
                    );
                }
                scope.styles = styles;
                scope.dirty = false;
            }
            if scope.styles.stylesheets.dirty() {
                let _invalidations = scope.styles.flush(&mut self.stylist, &guard);
                drop(guard);
                tree.set_shadow_cascade(root, scope.styles.data.clone());
            }
        }
    }

    /// Marks all stylesheet origins dirty so the next flush reprocesses them
    /// (used after in-place rule mutations or `disabled` toggles).
    pub fn note_sheets_changed(&mut self) {
        self.bump_version();
        self.stylist
            .force_stylesheet_origins_dirty(Origin::Author.into());
    }

    /// The font collection gained a face (an `@font-face` finished loading), so
    /// every computed value that went through the font-metrics provider — `ex`,
    /// `ch`, `ic` units and `font-size-adjust` — may now resolve differently.
    ///
    /// The provider needs no reinstalling (it reads the collection through a
    /// shared `Arc<Mutex<FontContext>>`), but stylo caches `ComputedValues` per
    /// element and reuses them until something dirties the cascade. Forcing the
    /// author origin dirty lands a `restyle_subtree()` hint on the root at the
    /// next flush, so the document re-cascades exactly once.
    pub fn note_fonts_changed(&mut self) {
        self.note_sheets_changed();
    }

    /// Parses author-stylesheet bytes with spec-compliant charset detection,
    /// resolving `@import` through `loader` if provided.
    #[must_use]
    pub fn make_stylesheet_from_bytes(
        &self,
        bytes: &[u8],
        url_data: UrlExtraData,
        protocol_encoding_label: Option<&str>,
        environment_encoding: Option<&'static Encoding>,
        media: Option<&str>,
        loader: Option<&dyn StylesheetLoader>,
    ) -> DocumentStyleSheet {
        let media_list = media_list(&self.lock, &url_data, media);
        let sheet = Stylesheet::from_bytes(
            bytes,
            url_data,
            protocol_encoding_label,
            environment_encoding,
            Origin::Author,
            media_list,
            self.lock.clone(),
            loader,
            None,
            QuirksMode::NoQuirks,
        );
        DocumentStyleSheet(ServoArc::new(sheet))
    }

    /// CSSOM `CSSStyleSheet.insertRule`: parses `text` and inserts it at `index`.
    ///
    /// v1 limitation: an inserted `@namespace` rule does not register its prefix
    /// with the sheet. stylo's `CssRule::parse` builds its parser context over a
    /// `Cow::Borrowed` snapshot of `StylesheetContents::namespaces` and mutates
    /// only the clone, and the contents live behind a shared `Arc` we cannot
    /// write through. Servo has the same gap.
    pub fn insert_rule(
        &mut self,
        sheet: &DocumentStyleSheet,
        text: &str,
        index: usize,
    ) -> Result<CssRule, RulesMutateError> {
        // Parse (and bounds-check `index`) before bumping: a rejected rule must
        // not invalidate every computed-value cache in the document.
        let new_rule = {
            let guard = self.lock.read();
            let contents = sheet.contents(&guard);
            contents.rules.read_with(&guard).parse_rule_for_insert(
                &self.lock,
                text,
                contents,
                index,
                CssRuleTypes::from_bits(0),
                None,
                None,
                AllowImportRules::Yes,
            )?
        };
        self.bump_version();
        let rules = {
            let guard = self.lock.read();
            sheet.contents(&guard).rules.clone()
        };
        {
            let mut write = self.lock.write();
            rules
                .write_with(&mut write)
                .0
                .insert(index, new_rule.clone());
        }
        let guard = self.lock.read();
        self.stylist
            .rule_changed(sheet, &new_rule, &guard, RuleChangeKind::Insertion, &[]);
        Ok(new_rule)
    }

    /// CSSOM `CSSStyleSheet.deleteRule`: removes the rule at `index`.
    pub fn delete_rule(
        &mut self,
        sheet: &DocumentStyleSheet,
        index: usize,
    ) -> Result<(), RulesMutateError> {
        let rules = {
            let guard = self.lock.read();
            sheet.contents(&guard).rules.clone()
        };
        let removed = {
            let guard = self.lock.read();
            rules.read_with(&guard).0.get(index).cloned()
        };
        // An out-of-range index is a no-op, so it must not invalidate caches.
        let Some(removed) = removed else {
            return Err(RulesMutateError::IndexSize);
        };
        {
            let mut write = self.lock.write();
            rules.write_with(&mut write).remove_rule(index)?;
        }
        self.bump_version();
        let guard = self.lock.read();
        self.stylist
            .rule_changed(sheet, &removed, &guard, RuleChangeKind::Removal, &[]);
        Ok(())
    }

    /// Notifies the stylist that a style rule's declarations changed in place.
    pub fn note_style_rule_declarations_changed(
        &mut self,
        sheet: &DocumentStyleSheet,
        rule: &CssRule,
    ) {
        self.bump_version();
        let guard = self.lock.read();
        self.stylist.rule_changed(
            sheet,
            rule,
            &guard,
            RuleChangeKind::StyleRuleDeclarations,
            &[],
        );
    }

    /// Toggles a stylesheet's `disabled` flag and marks author origins dirty.
    pub fn set_sheet_disabled(&mut self, sheet: &DocumentStyleSheet, disabled: bool) {
        sheet.0.set_disabled(disabled);
        self.note_sheets_changed();
    }

    /// Computed style for an anonymous box (layout-generated, no DOM node)
    /// inheriting from `parent`.
    ///
    /// The caller must hold an [`enter_active_tree`] scope: the cascade may
    /// create `NodeRef` handles internally, and those recover the tree from
    /// the active-tree thread-local (ADR-0005).
    #[must_use]
    pub fn anonymous_box_style(
        &self,
        parent: &ServoArc<ComputedValues>,
    ) -> ServoArc<ComputedValues> {
        let guard = self.lock.read();
        let guards = StylesheetGuards::same(&guard);
        self.stylist.style_for_anonymous::<NodeRef<'_>>(
            &guards,
            &style::selector_parser::PseudoElement::ServoAnonymousBox,
            parent,
        )
    }

    /// Resolves computed styles for the whole document, consuming DOM snapshots.
    ///
    /// Takes `&mut DomTree` because the traversal mutates per-element cascade
    /// data through shared references (interior mutability) and then clears the
    /// snapshot map afterwards.
    pub fn resolve_styles(&mut self, tree: &mut DomTree) {
        let Some(root_id) = tree.document_element() else {
            return;
        };

        // Shadow-scoped author styles flush before the traversal so
        // `TShadowRoot::style_data` reads current cascade data.
        self.flush_shadow_scopes(tree);

        thread_state::enter(ThreadState::LAYOUT);
        {
            // Install the tree so `NodeRef` handles created during the
            // traversal can recover it (pointer-sized handle constraint).
            let active = enter_active_tree(&*tree);
            let guard = self.lock.read();
            let guards = StylesheetGuards {
                author: &guard,
                ua_or_user: &guard,
            };
            let root = NodeRef::new(&active, root_id);

            let invalidations = self.stylist.flush(&guards);
            invalidations.process_style::<NodeRef<'_>>(root, Some(tree.snapshots()));

            let context = SharedStyleContext {
                traversal_flags: TraversalFlags::empty(),
                stylist: &self.stylist,
                options: GLOBAL_STYLE_DATA.options.clone(),
                guards,
                visited_styles_enabled: false,
                animations: self.animations.clone(),
                current_time_for_animations: 0.0,
                snapshot_map: tree.snapshots(),
                registered_speculative_painters: &NoPainters,
            };

            let token = RecalcStyle::pre_traverse(root, &context);
            if token.should_traverse() {
                let traverser = RecalcStyle::new(context);
                driver::traverse_dom(&traverser, token, None);
                self.restyled.append(&mut traverser.into_restyled());
            }
        }

        self.stylist.rule_tree().maybe_gc();
        tree.clear_snapshots();
        thread_state::exit(ThreadState::LAYOUT);
    }
}

/// Drops repeats from an `adoptedStyleSheets` list, keeping each sheet at its
/// last position — the cascade order the CSSOM gives duplicates, and the only
/// one stylo's sheet set will accept (see [`StyleEngine::set_adopted_sheets`]).
///
/// Identity is `Arc::ptr_eq` (`DocumentStyleSheet`'s `PartialEq`): two sheets
/// with the same text are two sheets, and both cascade.
fn dedupe_keeping_last(sheets: Vec<DocumentStyleSheet>) -> Vec<DocumentStyleSheet> {
    let mut kept: Vec<DocumentStyleSheet> = Vec::with_capacity(sheets.len());
    for sheet in sheets.into_iter().rev() {
        if !kept.contains(&sheet) {
            kept.push(sheet);
        }
    }
    kept.reverse();
    kept
}

fn make_stylesheet_impl(
    lock: &SharedRwLock,
    url_data: &UrlExtraData,
    css: &str,
    origin: Origin,
    media: Option<&str>,
    loader: Option<&dyn StylesheetLoader>,
) -> DocumentStyleSheet {
    let media_list = media_list(lock, url_data, media);
    let sheet = Stylesheet::from_str(
        css,
        url_data.clone(),
        origin,
        media_list,
        lock.clone(),
        loader,
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
    );
    DocumentStyleSheet(ServoArc::new(sheet))
}

/// Builds a locked [`MediaList`] from an optional `media` query string.
fn media_list(
    lock: &SharedRwLock,
    url_data: &UrlExtraData,
    media: Option<&str>,
) -> ServoArc<Locked<MediaList>> {
    let list = match media {
        Some(media) => crate::cssom::parse_media_list(media, url_data),
        None => MediaList::empty(),
    };
    ServoArc::new(lock.wrap(list))
}

/// The sequential restyle traversal, mirroring `blitz-dom`'s `RecalcStyle`.
struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
    /// Every element the traversal visited, i.e. every element whose computed
    /// style may have changed. Stylo only descends into elements carrying a
    /// restyle hint or dirty descendants, so this is a tight superset of the
    /// changed set and lets layout patch just those nodes ([`StyleEngine::restyled_nodes`]).
    /// `DomTraversal` is a `Sync` trait taking `&self`, hence the mutex.
    restyled: Mutex<Vec<NodeId>>,
}

impl<'a> RecalcStyle<'a> {
    fn new(context: SharedStyleContext<'a>) -> Self {
        RecalcStyle {
            context,
            restyled: Mutex::new(Vec::new()),
        }
    }

    fn into_restyled(self) -> Vec<NodeId> {
        self.restyled
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[allow(unsafe_code)]
impl<E> DomTraversal<E> for RecalcStyle<'_>
where
    E: TElement,
{
    fn process_preorder<F: FnMut(E::ConcreteNode)>(
        &self,
        traversal_data: &PerLevelTraversalData,
        context: &mut StyleContext<'_, E>,
        node: E::ConcreteNode,
        note_child: F,
    ) {
        if let Some(el) = node.as_element() {
            // SAFETY: stylo's traversal has exclusive access to this element.
            let mut data = unsafe { el.ensure_data() };
            recalc_style_at(self, traversal_data, context, el, &mut data, note_child);
            // SAFETY: same exclusive-access contract.
            unsafe { el.unset_dirty_descendants() }
            self.restyled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(node_id_from_opaque(el.as_node().opaque()));
        }
    }

    #[inline]
    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(&self, _context: &mut StyleContext<'_, E>, _node: E::ConcreteNode) {
        unreachable!("post-order traversal is disabled")
    }

    #[inline]
    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.context
    }
}
