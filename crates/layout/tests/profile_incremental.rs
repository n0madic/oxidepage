//! Ad-hoc phase profile for the incremental path (run with --nocapture).
use oxidepage_dom::{ParseOptions, parse_document};
use oxidepage_layout::LayoutEngine;
use oxidepage_style::{StyleEngine, Viewport};

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

#[test]
#[ignore]
fn profile_phases() {
    let html = build_document();
    let mut dom = parse_document(&html, ParseOptions::default()).tree;
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
        .unwrap();

    let n = 200;
    let mut restyle = std::time::Duration::ZERO;
    let mut rest = std::time::Duration::ZERO;
    for i in 0..n {
        dom.set_attribute(
            probe,
            oxidepage_dom::node::attr_name(html5ever::local_name!("style")),
            format!("width: {}px", 50 + (i % 100)).into(),
        );
        let t0 = std::time::Instant::now();
        style.resolve_styles(&mut dom);
        restyle += t0.elapsed();
        let t1 = std::time::Instant::now();
        layout.reflow(&mut dom, &mut style);
        rest += t1.elapsed();
    }
    eprintln!(
        "restyle: {:?}/iter, reflow-after-restyle: {:?}/iter, counts: {:?}",
        restyle / n,
        rest / n,
        layout.reflow_counts()
    );
}
