//! Where a printed page may end.
//!
//! Paper is a fragmentation problem, and this engine already solved one: CSS
//! multi-column. [`crate::multicol::break_opportunities`] computes the class-A
//! break points of a block flow — every in-flow block's top border edge and
//! every parley line top — which is exactly the "never cut a line in half" rule
//! ADR-0016 credits it with. Pagination asks it the same question with the flow
//! rooted at the document box and the slice height set by the paper.
//!
//! The offsets are **not** in the display list: `paint::text::paint_ifc` emits
//! glyph runs whose `y` is a baseline, and no `DisplayItem` carries a line top
//! or bottom. So they come from here, and `export-pdf` — which stays a dumb
//! display-list consumer (design P5) — is handed the finished boundaries.
//!
//! **One rule differs from multicol's fill**, deliberately. A column whose
//! content offers no break point simply overflows (`multicol::fill`); a *page*
//! cannot, because a page that overflows is a page of lost paper. A `<body>`
//! that is a flex container, or a single tall block, offers no class-A break
//! point *at all*, and would print as one page as tall as the document — the
//! very bug pagination exists to fix. So a page with no opportunity inside it
//! breaks at the page boundary instead, which is CSS Fragmentation §3.4's
//! last-resort rule (ADR-0026).

use crate::engine::LayoutEngine;

/// Hard cap on the pages one document may produce, mirroring the engine's other
/// budgets: a pathological document must not turn into an unbounded file. Past
/// it the final page simply runs to the end of the document.
pub const MAX_PAGES: usize = 1000;

/// Offsets closer than this are the same break point (multicol's `EPS`).
const EPS: f32 = 0.01;

impl LayoutEngine {
    /// The page boundaries of the current document, in document CSS px: `n + 1`
    /// offsets for `n` pages, starting at `0.0` and ending at the document's
    /// full height. Each page ends at the last break opportunity that fits in
    /// `page_height`, or — when there is none — at the page boundary itself.
    ///
    /// A degenerate `page_height` (zero, negative, non-finite) or an empty
    /// document yields the single pair `[0, height]`: one page, as tall as the
    /// document, which is what `PdfOptions { paginate: false }` asks for anyway.
    #[must_use]
    pub fn page_boundaries(&self, page_height: f32) -> Vec<f32> {
        // The document is at least one viewport tall, exactly as the display
        // list's `content_size` is. Reporting the bare content extent instead
        // made a short document paginate into *two* pages: the exporter's
        // document box is the viewport-floored height, so the extent became an
        // interior boundary and everything below it a second, blank sheet.
        let (_, extent) = self.document_content_extent();
        let doc_height = extent.max(self.viewport().height).max(0.0);
        if !page_height.is_finite() || page_height <= 0.0 || doc_height <= EPS {
            return vec![0.0, doc_height];
        }
        let Some(root) = self.tree().root() else {
            return vec![0.0, doc_height];
        };

        // The document box is the flow: its in-flow blocks and line boxes are
        // the class-A break points, in the same rounded space paint reads. They
        // come out relative to the root box's own border-box top, which is not
        // the document origin when the root is offset (`html { margin }`), so
        // they are shifted into document space here — the space
        // `document_content_extent` and the display list are in.
        let root_y = self.tree().box_(root).final_layout.location.y;
        let mut offsets: Vec<f32> = crate::multicol::break_opportunities(self.tree(), root, true)
            .into_iter()
            .map(|offset| offset + root_y)
            .collect();
        // Only *interior* offsets are candidates. The document's own top and
        // bottom are added below, and the bottom is the scrollable extent rather
        // than the root box's height — content can overflow the root (an
        // absolutely positioned footer, a floated column) and the display list is
        // sized to the extent, so the last page has to reach it.
        offsets.retain(|o| o.is_finite() && *o > EPS && *o < doc_height - EPS);
        offsets.sort_by(f32::total_cmp);
        offsets.dedup_by(|a, b| (*a - *b).abs() < EPS);

        let mut boundaries = vec![0.0f32];
        let mut start = 0.0f32;
        while start < doc_height - EPS && boundaries.len() <= MAX_PAGES {
            crate::budget::checkpoint();
            let bottom = start + page_height;
            if bottom >= doc_height - EPS {
                break;
            }
            // The last opportunity that fits below this page's bottom; the page
            // boundary itself when the page holds no break point (§3.4).
            let next = offsets
                .iter()
                .copied()
                .rfind(|&o| o > start + EPS && o <= bottom + EPS)
                .unwrap_or(bottom);
            boundaries.push(next);
            start = next;
        }
        boundaries.push(doc_height);
        boundaries
    }
}
