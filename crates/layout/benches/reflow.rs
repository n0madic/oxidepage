//! Full-rebuild reflow benchmark (WP-I): style resolution + box-tree
//! construction + taffy/parley compute over a ~1000-element document.
//! WP-K compares its incremental relayout against these numbers (ADR-0006).

use criterion::{Criterion, criterion_group, criterion_main};
use oxidepage_dom::{DomTree, ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_style::{StyleEngine, Viewport};

/// ~1000 elements: nested rows of blocks, inline text, and flex items.
fn build_document() -> String {
    let mut html = String::from(
        "<!DOCTYPE html><html><head><style>\
         .row { display: flex; margin: 2px; }\
         .cell { flex: 1; padding: 2px; border: 1px solid black; }\
         .txt { font-family: Ahem; font-size: 10px; line-height: 10px; }\
         </style></head><body>",
    );
    for row in 0..100 {
        html.push_str("<div class=row>");
        for cell in 0..4 {
            html.push_str(&format!(
                "<div class=cell><span class=txt>item {row}-{cell}</span></div>"
            ));
        }
        html.push_str("</div>");
        html.push_str(&format!("<p class=txt>row {row} description text</p>"));
    }
    html.push_str("</body></html>");
    html
}

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

fn bench_reflow(c: &mut Criterion) {
    let html = build_document();

    c.bench_function("full_reflow_1000_elements", |b| {
        let mut dom = parse(&html);
        let mut style = StyleEngine::new(&dom, Viewport::default());
        let mut layout = LayoutEngine::new(Viewport::default());
        let body = dom
            .inclusive_descendants(dom.document())
            .find(|&id| {
                dom.node(id)
                    .as_element()
                    .is_some_and(|el| &*el.name.local == "body")
            })
            .expect("body");
        // A non-style attribute toggle bumps the DOM structure version,
        // forcing the full pipeline (box-tree rebuild + compute) while
        // styles stay warm.
        let mut flip = false;
        b.iter(|| {
            flip = !flip;
            dom.set_attribute(
                body,
                oxidepage_dom::node::attr_name(html5ever::LocalName::from("data-flip")),
                if flip { "a".into() } else { "b".into() },
            );
            layout.reflow(&mut dom, &mut style);
            std::hint::black_box(layout.tree().box_count())
        });
    });

    c.bench_function("incremental_relayout_1000_elements", |b| {
        // WP-K: an inline-style width change on one leaf takes the patch
        // path — no box-tree rebuild, taffy caches reused outside the
        // changed ancestor chain.
        let mut dom = parse(&html);
        let mut style = StyleEngine::new(&dom, Viewport::default());
        let mut layout = LayoutEngine::new(Viewport::default());
        layout.reflow(&mut dom, &mut style);
        let probe = dom
            .inclusive_descendants(dom.document())
            .find(|&id| {
                dom.node(id)
                    .as_element()
                    .is_some_and(|el| el.classes().iter().any(|c| &**c == "cell"))
            })
            .expect("a cell");
        let mut width = 100;
        b.iter(|| {
            width = (width % 200) + 1;
            dom.set_attribute(
                probe,
                oxidepage_dom::node::attr_name(html5ever::LocalName::from("style")),
                format!("width: {width}px").into(),
            );
            layout.reflow(&mut dom, &mut style);
            std::hint::black_box(layout.tree().box_count())
        });
    });

    c.bench_function("styles_and_reflow_1000_elements", |b| {
        // Cold path: fresh style engine + layout each iteration (includes
        // the first full cascade).
        b.iter_batched(
            || {
                let dom = parse(&html);
                let style = StyleEngine::new(&dom, Viewport::default());
                (dom, style, LayoutEngine::new(Viewport::default()))
            },
            |(mut dom, mut style, mut layout)| {
                layout.reflow(&mut dom, &mut style);
                std::hint::black_box(layout.tree().box_count())
            },
            criterion::BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, bench_reflow);
criterion_main!(benches);
