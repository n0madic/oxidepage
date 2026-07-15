//! Read-modify-write geometry benchmark (WP-I): a script mutates an inline
//! style and immediately reads `offsetWidth`, forcing a synchronous reflow
//! per iteration on a ~1000-element page. Budget: ≤ 10 ms/iteration with the
//! full-rebuild layout (ADR-0006 records the measured numbers).

use criterion::{Criterion, criterion_group, criterion_main};
use oxidepage_page::{PageOptions, load_html_page};

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
    html.push_str("<div id=probe></div></body></html>");
    html
}

fn bench_geometry_rmw(c: &mut Criterion) {
    let html = build_document();
    let page = load_html_page(&html, PageOptions::default()).expect("load");
    page.eval_to_string("window.__w = 100; 'ok'").expect("init");

    c.bench_function("geometry_read_modify_write", |b| {
        b.iter(|| {
            // Each iteration invalidates layout (style write) and forces a
            // synchronous reflow (offsetWidth read).
            let out = page
                .eval_to_string(
                    "window.__w = (window.__w % 200) + 1;\
                     var probe = document.getElementById('probe');\
                     probe.style.width = window.__w + 'px';\
                     probe.offsetWidth",
                )
                .expect("eval");
            std::hint::black_box(out)
        });
    });
}

criterion_group!(benches, bench_geometry_rmw);
criterion_main!(benches);
