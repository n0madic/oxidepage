//! [`LayoutEngine`]: owns the box tree + font system and drives reflow
//! (style resolution → box-tree construction → taffy compute → rounding →
//! overflow pass), guarded by a version stamp so synchronous JS geometry
//! reads are cheap when nothing changed.
//!
//! Incremental relayout (WP-K): when the DOM structure is unchanged and the
//! restyle only touched non-structural properties, the box tree is *patched*
//! in place — changed boxes get fresh taffy styles and their ancestor chains'
//! taffy caches are cleared — instead of being rebuilt, and the compute pass
//! reuses the caches of clean subtrees. Detection is by computed-style Arc
//! pointer diffing against a per-build snapshot (a deliberate deviation from
//! blitz-dom's RestyleDamage bits; ADR-0006).

use std::collections::{HashMap, HashSet};

use oxidepage_base::NodeId;
use oxidepage_dom::select::enter_active_tree;
use oxidepage_dom::{DomTree, NodeKind};
use oxidepage_style::{StyleEngine, Viewport};
use servo_arc::Arc as ServoArc;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use taffy::AvailableSpace;

use crate::construct::{build_layout_tree, capture_text_fields, taffy_style_for};
use crate::fonts::FontSystem;
use crate::intrinsic_size::resolve_intrinsic_size_keywords;
use crate::overflow::resolve_scrollable_overflow;
use crate::positioning::{hoist_out_of_flow, restore_static_positions};
use crate::scroll::ScrollState;
use crate::tree::{BoxId, BoxKind, LayoutTree};

/// The state of the world a layout was computed for. Reflow is skipped while
/// the stamp matches.
#[derive(Clone, Copy, PartialEq, Debug)]
struct ReflowStamp {
    dom_version: u64,
    style_version: u64,
    viewport: Viewport,
    images_version: u64,
    fonts_version: u64,
}

/// The state of the world a *paint* (display list) was built for. Adds the
/// element-scroll version to the reflow inputs: an element's overflow scroll is
/// baked into item origins, so it dirties paint but not layout (design doc
/// §5.11). Document (viewport) scroll is deliberately *not* here — it is applied
/// by the rasterizer, not baked into the display list, so the cached list is
/// reused across document scroll positions. Images gain their own version in WP-J.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PaintStamp {
    pub dom_version: u64,
    pub style_version: u64,
    pub viewport: Viewport,
    pub element_scroll_version: u64,
    pub images_version: u64,
    pub fonts_version: u64,
}

/// Per-build record of every styled element's computed values, for the
/// incremental patch's pointer diffing.
struct BuildSnapshot {
    /// [`DomTree::structure_version`] at build time; any structural mutation
    /// forces a full rebuild.
    structure_version: u64,
    /// `(element, primary style)` in document order for every element stylo
    /// styled at build time.
    element_styles: Vec<(NodeId, ServoArc<ComputedValues>)>,
    /// Index into [`Self::element_styles`] by node, so a patch driven by the
    /// restyled set finds an element's captured style without a document walk.
    index_by_node: HashMap<NodeId, usize>,
    /// `::before`/`::after` styles for elements that had them.
    pseudo_styles: HashMap<NodeId, [Option<ServoArc<ComputedValues>>; 2]>,
    /// DOM nodes contributing style spans to some IFC: a style change on one
    /// requires reshaping, i.e. a rebuild.
    ifc_contributors: HashSet<NodeId>,
    /// The image-store version at build time; a change means an image decoded
    /// (new intrinsic size), forcing a full rebuild.
    images_version: u64,
    /// The web-font version at build time; a change means a web font
    /// registered, forcing a full rebuild so text re-shapes against it.
    fonts_version: u64,
}

/// The document's layout engine.
pub struct LayoutEngine {
    tree: LayoutTree,
    fonts: FontSystem,
    viewport: Viewport,
    pub(crate) scroll: ScrollState,
    images: crate::images::ImageStore,
    /// Monotonic counter bumped when a web font is registered; folded into the
    /// reflow/paint stamps so a newly loaded `@font-face` forces a re-shape.
    fonts_version: u64,
    stamp: Option<ReflowStamp>,
    snapshot: Option<BuildSnapshot>,
    /// Counters for tests/diagnostics: how many reflows rebuilt the box tree
    /// vs. patched it in place.
    rebuild_count: u64,
    patch_count: u64,
}

impl LayoutEngine {
    #[must_use]
    pub fn new(viewport: Viewport) -> Self {
        Self {
            tree: LayoutTree::new(viewport),
            fonts: FontSystem::new(),
            viewport,
            scroll: ScrollState::default(),
            images: crate::images::ImageStore::default(),
            fonts_version: 0,
            stamp: None,
            snapshot: None,
            rebuild_count: 0,
            patch_count: 0,
        }
    }

    /// `(full rebuilds, in-place patches)` performed so far.
    #[must_use]
    pub fn reflow_counts(&self) -> (u64, u64) {
        (self.rebuild_count, self.patch_count)
    }

    /// The current box tree (valid after [`Self::reflow`]).
    #[must_use]
    pub fn tree(&self) -> &LayoutTree {
        &self.tree
    }

    /// The shared font context handle (for the style engine's font-metrics
    /// provider, WP-H).
    #[must_use]
    pub fn font_ctx(&self) -> std::sync::Arc<std::sync::Mutex<parley::FontContext>> {
        self.fonts.font_ctx()
    }

    /// A provider factory for [`StyleEngine::set_font_metrics_provider`]
    /// backed by this engine's font collection, so `ex`/`ch`/`ic` units
    /// resolve against real font metrics.
    #[must_use]
    pub fn font_metrics_factory(&self) -> oxidepage_style::engine::FontMetricsProviderFactory {
        let font_ctx = self.fonts.font_ctx();
        std::sync::Arc::new(move || {
            Box::new(crate::fonts::ParleyFontMetricsProvider {
                font_ctx: std::sync::Arc::clone(&font_ctx),
            })
        })
    }

    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// The paint invalidation key for the current layout state. Callers reflow
    /// first, so [`Self::stamp`]'s dom/style versions are current; the
    /// element-scroll version is read live (scrolling never triggers a reflow).
    #[must_use]
    pub fn paint_stamp(&self) -> PaintStamp {
        let (dom_version, style_version) = self
            .stamp
            .map_or((0, 0), |s| (s.dom_version, s.style_version));
        PaintStamp {
            dom_version,
            style_version,
            viewport: self.viewport,
            element_scroll_version: self.scroll.element_version,
            images_version: self.images.version(),
            fonts_version: self.fonts_version,
        }
    }

    /// Monotonic version of the document (viewport) scroll. Not part of the
    /// [`PaintStamp`] — document scroll is applied at raster time, not baked
    /// into the display list — but consumers whose output *does* depend on the
    /// document scroll position (e.g. `IntersectionObserver`, which intersects
    /// against the scrolled viewport) gate on this.
    #[must_use]
    pub fn document_scroll_version(&self) -> u64 {
        self.scroll.document_version
    }

    /// Decodes and registers a web font (`@font-face` `src`) under `family`,
    /// bumping the fonts version so the next reflow re-shapes text against it
    /// (Phase 7, WP-C).
    pub fn register_web_font(
        &mut self,
        family: &str,
        raw: &[u8],
        attrs: crate::fonts::WebFontAttrs,
    ) -> crate::fonts::WebFontOutcome {
        let outcome = self.fonts.register_web_font(family, raw, attrs);
        if outcome == crate::fonts::WebFontOutcome::Registered {
            self.fonts_version += 1;
        }
        outcome
    }

    /// The decoded-image store (read by paint for `<img>` and background
    /// images).
    #[must_use]
    pub fn images(&self) -> &crate::images::ImageStore {
        &self.images
    }

    /// Inserts a decoded raster image for `url`, bumping the store version so
    /// the next reflow rebuilds intrinsic sizes and the paint cache invalidates.
    pub fn insert_raster_image(
        &mut self,
        url: String,
        width: u32,
        height: u32,
        rgba: std::sync::Arc<Vec<u8>>,
    ) {
        self.images.insert_raster(url, width, height, rgba);
    }

    /// Inserts a vector (SVG) image for `url`: `width`/`height` are its
    /// intrinsic size, and the markup is rasterized by the backend at the size
    /// the element paints at. Bumps the store version like the raster path.
    pub fn insert_vector_image(
        &mut self,
        url: String,
        width: u32,
        height: u32,
        svg: std::sync::Arc<Vec<u8>>,
    ) {
        self.images.insert_vector(url, width, height, svg);
    }

    /// Marks `url` as a broken image (failed load/decode).
    pub fn mark_image_broken(&mut self, url: String) {
        self.images.insert_broken(url);
    }

    /// Replaces the viewport; the next reflow rebuilds the layout.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
    }

    /// Brings the layout up to date with the DOM, styles, and viewport.
    ///
    /// Invariant: this may resolve styles and rebuild the whole box tree; it
    /// must never call back into JS bindings (callers hold the page borrow).
    pub fn reflow(&mut self, dom: &mut DomTree, style: &mut StyleEngine) {
        let stamp = ReflowStamp {
            dom_version: dom.style_version(),
            style_version: style.version(),
            viewport: self.viewport,
            images_version: self.images.version(),
            fonts_version: self.fonts_version,
        };
        if self.stamp == Some(stamp) {
            return;
        }

        style.resolve_styles(dom);

        // Drains the set, which accumulates across every `resolve_styles` since
        // the last reflow — `getComputedStyle` resolves styles too, and would
        // otherwise consume the restyle before layout ever saw it.
        let restyled = style.take_restyled_nodes();

        if self.try_patch(dom, &restyled) {
            self.patch_count += 1;
        } else {
            // Construction reads stylo styles (and may create `NodeRef`s for
            // anonymous-box styling), so the whole build runs inside one
            // active-tree scope. The compute passes below don't need it —
            // they only see captured data.
            let _scope = enter_active_tree(dom);
            self.tree = build_layout_tree(dom, style, &mut self.fonts, self.viewport, &self.images);
            hoist_out_of_flow(&mut self.tree, dom);
            self.snapshot = Some(self.take_snapshot(dom));
            self.rebuild_count += 1;
        }

        if let Some(root) = self.tree.root() {
            // Viewport overflow propagation must run before taffy, so the body's
            // used overflow (and its clipping) is settled for layout, paint and
            // the scrollable-overflow pass alike.
            self.propagate_body_overflow(dom);
            resolve_intrinsic_size_keywords(&mut self.tree);
            let available_space = taffy::Size {
                width: AvailableSpace::Definite(self.viewport.width),
                height: AvailableSpace::Definite(self.viewport.height),
            };
            self.tree.reset_post_layout_offsets();
            taffy::compute_root_layout(&mut self.tree, root.into(), available_space);
            self.tree.apply_relative_float_offsets();
            self.tree.apply_absolute_auto_margins();
            restore_static_positions(&mut self.tree);
            // Outside list markers are placed by hand, after the flow they sit
            // outside of is final and before rounding folds it into
            // `final_layout` (`marker` module docs).
            crate::marker::place_markers(&mut self.tree);
            taffy::round_layout(&mut self.tree, root.into());
            // Column boundaries are placed *after* rounding: paint positions the
            // flow's content from the rounded origins, so a boundary derived from
            // the unrounded ones would sit up to half a pixel off and shave the
            // top line of every column but the first (`multicol`, module docs).
            crate::multicol::resolve_columns(&mut self.tree);
            resolve_scrollable_overflow(&mut self.tree);
        }

        self.stamp = Some(stamp);
    }

    /// Viewport overflow propagation (CSS Overflow §3.3): when the root element's
    /// used overflow is `visible`, the HTML `<body>`'s scrollable overflow is
    /// applied to the viewport instead and the body's own used overflow becomes
    /// `visible`.
    ///
    /// Without it, `body { height: 100vh; overflow: auto }` — a common SPA shell
    /// (angular.dev) — makes the body a scroll container that clips the whole
    /// document to one viewport: its real height never reaches `documentElement`,
    /// so `scrollHeight` and `--full-page` capture a single screen. Applied per
    /// axis and only for `scroll`/`auto` (a `hidden` body keeps clipping, the
    /// conservative pre-existing behaviour). The viewport still scrolls because
    /// the visible root is itself scrollable ([`Self::max_viewport_scroll`]).
    fn propagate_body_overflow(&mut self, dom: &DomTree) {
        let Some(root) = self.tree.root() else {
            return;
        };
        let root_overflow = self.tree.box_(root).style.overflow;
        // CSS `overflow: auto` maps to `taffy::Overflow::Scroll` (stylo_taffy has
        // no `Auto`), so `Scroll` covers both scrollable values.
        let scrollable = |o: taffy::Overflow| matches!(o, taffy::Overflow::Scroll);
        let body = self
            .tree
            .box_(root)
            .children
            .iter()
            .copied()
            .find(|&child| {
                self.tree.box_(child).dom_node.is_some_and(|n| {
                    dom.get(n)
                        .and_then(|node| node.as_element())
                        .is_some_and(|el| el.is_html_element() && &*el.name.local == "body")
                })
            });
        let Some(body) = body else {
            return;
        };
        let body_overflow = self.tree.box_(body).style.overflow;
        let mut propagated = body_overflow;
        if root_overflow.x == taffy::Overflow::Visible && scrollable(body_overflow.x) {
            propagated.x = taffy::Overflow::Visible;
        }
        if root_overflow.y == taffy::Overflow::Visible && scrollable(body_overflow.y) {
            propagated.y = taffy::Overflow::Visible;
        }
        if propagated != body_overflow {
            self.tree.box_mut(body).style.overflow = propagated;
        }
    }

    /// Records the styled elements (and pseudo styles / IFC contributors)
    /// the current box tree was built from.
    fn take_snapshot(&self, dom: &DomTree) -> BuildSnapshot {
        let mut element_styles = Vec::new();
        let mut index_by_node = HashMap::new();
        let mut pseudo_styles = HashMap::new();
        for node in dom.inclusive_descendants(dom.document()) {
            if dom.node(node).data().kind() != NodeKind::Element {
                continue;
            }
            let Some(primary) = dom.primary_style(node) else {
                continue;
            };
            index_by_node.insert(node, element_styles.len());
            element_styles.push((node, primary));
            let before = dom.pseudo_style(node, &PseudoElement::Before);
            let after = dom.pseudo_style(node, &PseudoElement::After);
            if before.is_some() || after.is_some() {
                pseudo_styles.insert(node, [before, after]);
            }
        }

        let mut ifc_contributors = HashSet::new();
        for index in 0..self.tree.box_count() {
            if let Some(ifc) = &self.tree.box_(BoxId(index as u32)).ifc {
                ifc_contributors.extend(ifc.contributors.iter().copied());
            }
        }

        BuildSnapshot {
            structure_version: dom.structure_version(),
            element_styles,
            index_by_node,
            pseudo_styles,
            ifc_contributors,
            images_version: self.images.version(),
            fonts_version: self.fonts_version,
        }
    }

    /// Attempts an in-place patch of the box tree after a restyle: only
    /// non-structural, non-text-affecting style changes on box-generating
    /// elements qualify. Returns `false` when a full rebuild is required.
    ///
    /// `restyled` is the set of elements stylo actually visited
    /// ([`StyleEngine::restyled_nodes`]), so the cost scales with the size of
    /// the restyle, not the size of the document.
    fn try_patch(&mut self, dom: &DomTree, restyled: &[NodeId]) -> bool {
        let Some(snapshot) = &self.snapshot else {
            return false;
        };
        if snapshot.structure_version != dom.structure_version() || self.tree.root().is_none() {
            return false;
        }
        // The snapshot walk (`inclusive_descendants`) covers the light tree
        // only: a style-only mutation inside a shadow tree would be invisible
        // to the diff and "succeed" into a stale layout. Bail to a full
        // rebuild whenever shadow roots exist (incremental shadow patching is
        // a follow-up; ADR-0010).
        if dom.has_shadow_roots() {
            return false;
        }
        // A decoded image changes intrinsic sizes captured at construction.
        if snapshot.images_version != self.images.version() {
            return false;
        }
        // A newly registered web font changes glyph resolution: text must
        // re-shape, so fall back to a full rebuild.
        if snapshot.fonts_version != self.fonts_version {
            return false;
        }

        // Diff only what stylo restyled. The structure-version gate above means
        // the document's element set is unchanged since the build, so every
        // element whose computed style could differ is in `restyled` and the
        // rest still match the snapshot by construction — no document walk.
        let mut patches: Vec<(usize, Option<BoxId>, ServoArc<ComputedValues>)> = Vec::new();
        for &node in restyled {
            let Some(&index) = snapshot.index_by_node.get(&node) else {
                // Restyled an element the box tree was never built from.
                return false;
            };
            let Some(new_style) = dom.primary_style(node) else {
                return false;
            };
            let (_, old_style) = &snapshot.element_styles[index];

            // Stylo also visits elements that merely have dirty descendants, so
            // a restyled node's own style is often unchanged: a pointer-identical
            // primary style Arc means it was not re-cascaded. Its pseudo styles
            // are then unchanged too (stylo recomputes them together), so skip
            // the pseudo fetch and all property checks. A genuine pseudo change
            // re-cascades the element, yielding a fresh primary Arc, so it still
            // reaches the pseudo check below.
            if ServoArc::ptr_eq(&new_style, old_style) {
                continue;
            }

            // Pseudo-element gain/loss/change requires reconstruction.
            let new_before = dom.pseudo_style(node, &PseudoElement::Before);
            let new_after = dom.pseudo_style(node, &PseudoElement::After);
            let [old_before, old_after] = snapshot
                .pseudo_styles
                .get(&node)
                .cloned()
                .unwrap_or([None, None]);
            if !opt_arc_ptr_eq(&new_before, &old_before) || !opt_arc_ptr_eq(&new_after, &old_after)
            {
                return false;
            }

            // Construction-relevant property changes force a rebuild.
            if old_style.clone_display() != new_style.clone_display()
                || old_style.clone_position() != new_style.clone_position()
                || old_style.clone_float() != new_style.clone_float()
            {
                return false;
            }

            // `order` has no `taffy::Style` representation of its own (taffy
            // expects the caller to pre-sort `children`) — only
            // `construct::collect_flex_grid_children` applies it, on a full
            // rebuild. A patch refreshes `LayoutBox::order` but never
            // re-sorts the parent's `children`, so a changed value would
            // silently stay in its old position.
            if old_style.clone_order() != new_style.clone_order() {
                return false;
            }

            // `list-style-*` decides whether a list item generates a marker box
            // at all, what it says, and whether it is inline content of the item
            // or a box outside it — all captured at construction time.
            if old_style.get_list() != new_style.get_list() {
                return false;
            }

            // `column-count`/`column-width` decide whether the element generates
            // a `MulticolRoot` with an anonymous flow child at all, and
            // `column-gap` is captured on the `MulticolContext`, out of reach of
            // a taffy-style patch. The gap check is gated on the element actually
            // being a multicol container, so a flex/grid `column-gap` tweak —
            // which *is* patchable, through `taffy::Style::gap` — stays on the
            // fast path.
            let was_multicol = crate::multicol::is_multicol(old_style);
            if was_multicol != crate::multicol::is_multicol(&new_style)
                || (was_multicol
                    && (old_style.clone_column_count() != new_style.clone_column_count()
                        || old_style.clone_column_width() != new_style.clone_column_width()
                        || old_style.clone_column_gap() != new_style.clone_column_gap()))
            {
                return false;
            }

            // Text-relevant structs are shared with the parent when merely
            // inherited (pointer check); re-specified but identical
            // declarations get fresh structs, so fall back to a value
            // comparison before treating the change as text-affecting.
            let font_same = std::ptr::eq(
                std::ptr::from_ref(old_style.get_font()),
                std::ptr::from_ref(new_style.get_font()),
            ) || old_style.get_font() == new_style.get_font();
            let itext_same = std::ptr::eq(
                std::ptr::from_ref(old_style.get_inherited_text()),
                std::ptr::from_ref(new_style.get_inherited_text()),
            ) || old_style.get_inherited_text() == new_style.get_inherited_text();
            let text_changed = !(font_same && itext_same);

            // A text-relevant change on an IFC contributor means reshaping:
            // rebuild. Other changes on contributors (color, …) don't affect
            // the parley layout — only refresh the snapshot.
            if snapshot.ifc_contributors.contains(&node) {
                if text_changed {
                    return false;
                }
                patches.push((index, None, new_style));
                continue;
            }
            let Some(box_id) = self.tree.box_for_node(node) else {
                // The restyle reached a box-less element. Inline elements
                // are covered by the contributor arm; whitespace-only and
                // `display: none` holders don't affect layout unless their
                // display changed (checked above).
                patches.push((index, None, new_style));
                continue;
            };

            let layout_box = self.tree.box_(box_id);
            // Table grid-item styles are captured in the table context, out
            // of reach of a box patch.
            if layout_box.kind == BoxKind::TableRoot
                || layout_box
                    .parent
                    .is_some_and(|p| self.tree.box_(p).kind == BoxKind::TableRoot)
            {
                return false;
            }

            // Text-affecting changes require reshaping whenever this box owns
            // inline layout, either directly as an IFC root *or* through an
            // anonymous child that owns it on its behalf: the block(s) wrapping
            // inline runs in a mixed / flex / grid container, and the flow box of
            // a multicol container. Anonymous boxes have no `NodeId`, so the patch
            // loop never visits them: their captured font-size/line-height/
            // text-align/text-indent and pre-shaped parley layout would keep
            // stale metrics on an inherited text change (e.g. the parent's
            // `font-size`). Fall back to a full rebuild instead.
            //
            // The predicate is "has no DOM node", not "is an `AnonymousBlock`",
            // because a multicol flow box is promoted to `InlineRoot` when its
            // content is all-inline — the common case.
            let has_anon_child = layout_box
                .children
                .iter()
                .any(|&child| self.tree.box_(child).dom_node.is_none());
            if text_changed && (layout_box.ifc.is_some() || has_anon_child) {
                return false;
            }

            patches.push((index, Some(box_id), new_style));
        }

        // Commit: refresh captured styles and clear the taffy caches of each
        // patched box and its ancestor chain (clean subtrees keep theirs).
        for (snap_index, box_id, new_style) in patches {
            if let Some(box_id) = box_id {
                let new_taffy = taffy_style_for(&new_style);
                let layout_box = self.tree.box_mut(box_id);
                // Paint-only changes (color, …) produce a fresh computed
                // style with an identical taffy translation: keep the caches
                // and skip the relayout entirely.
                let layout_affected = layout_box.style != new_taffy;
                layout_box.style = new_taffy;
                capture_text_fields(layout_box, &new_style);

                if layout_affected {
                    let mut current = Some(box_id);
                    while let Some(id) = current {
                        self.tree.box_mut(id).cache.clear();
                        current = self.tree.box_(id).parent;
                    }
                }
            }

            let snapshot = self.snapshot.as_mut().expect("checked above");
            snapshot.element_styles[snap_index].1 = new_style;
        }
        true
    }

    /// Debug dump of the box tree with computed layouts.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut out = String::new();
        if let Some(root) = self.tree.root() {
            self.dump_box(&mut out, root, 0);
        } else {
            out.push_str("(no layout)\n");
        }
        out
    }

    fn dump_box(&self, out: &mut String, box_id: BoxId, depth: usize) {
        use std::fmt::Write as _;

        let b = self.tree.box_(box_id);
        let layout = &b.final_layout;
        let label = match b.kind {
            BoxKind::Block => match b.style.display {
                taffy::Display::Flex => "FLEX",
                taffy::Display::Grid => "GRID",
                taffy::Display::None => "NONE",
                taffy::Display::Block => "BLOCK",
            },
            BoxKind::InlineRoot => "INLINE",
            BoxKind::AnonymousBlock => "ANON",
            BoxKind::Replaced => "REPLACED",
            BoxKind::TableRoot => "TABLE",
            BoxKind::MulticolRoot => "MULTICOL",
        };
        let node = b
            .dom_node
            .map(|n| format!(" node={}v{}", n.index(), n.generation()))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{:indent$}{label}{node} @ ({x}, {y}) {w}x{h}",
            "",
            indent = depth * 2,
            x = layout.location.x,
            y = layout.location.y,
            w = layout.size.width,
            h = layout.size.height,
        );
        if let Some(mc) = &b.multicol {
            let _ = writeln!(
                out,
                "{:indent$}columns: {n} x {w} gap {gap}",
                "",
                indent = depth * 2 + 2,
                n = mc.columns().len(),
                w = mc.used_width(),
                gap = mc.used_gap(),
            );
        }
        if let Some(ifc) = &b.ifc
            && !ifc.text.is_empty()
        {
            let preview: String = ifc.text.chars().take(40).collect();
            let _ = writeln!(
                out,
                "{:indent$}text: {preview:?}",
                "",
                indent = depth * 2 + 2
            );
        }
        for &child in &b.children {
            self.dump_box(out, child, depth + 1);
        }
    }
}

/// Pointer equality over optional style arcs (`None` == `None`).
fn opt_arc_ptr_eq(
    a: &Option<ServoArc<ComputedValues>>,
    b: &Option<ServoArc<ComputedValues>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => ServoArc::ptr_eq(a, b),
        _ => false,
    }
}
