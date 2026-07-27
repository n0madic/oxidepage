# OxidePage

**OxidePage** is a headless web engine you can run from the command line or
embed as a Rust library. Point it at a local HTML file or a URL and it loads
the page for real — parses the HTML, applies CSS, runs the page's
JavaScript — then hands you a PNG screenshot, a PDF, the final rendered
HTML, or the answer to a JavaScript expression.

It's built from production-grade components rather than as a from-scratch
browser engine: [html5ever](https://github.com/servo/html5ever) for HTML
parsing, Servo's [`stylo`](https://github.com/servo/stylo) for CSS,
[`taffy`](https://github.com/DioxusLabs/taffy) and
[`parley`](https://github.com/linebender/parley) for layout and text
shaping, [QuickJS-NG](https://github.com/quickjs-ng/quickjs) for
JavaScript, and [`tiny-skia`](https://github.com/linebender/tiny-skia) for
rasterization.

## What it can do

- **Load real pages.** Full HTML5 parsing, a real CSS cascade (media
  queries, `@font-face`, custom properties), block/flex/grid/table layout,
  text shaping and line breaking, and CSS multi-column and list markers.
- **Run page JavaScript.** DOM, events, CSSOM, `getComputedStyle`, element
  geometry, `fetch`/`XMLHttpRequest`, cookies, timers,
  `MutationObserver`/`ResizeObserver`/`IntersectionObserver`,
  `requestAnimationFrame`, custom elements, and Shadow DOM — enough to boot
  real single-page apps (Angular, Lit, Stencil, Vue-based sites).
- **Follow the page where it goes.** Clicking a link, submitting a form, or
  setting `location.href` navigates for real, with session history
  (`back`/`forward`/`pushState`) and `popstate`/`hashchange` — so you can
  script your way through a multi-page flow, not just one document.
- **Watch what the page says.** `alert`/`confirm`/`prompt` work and are
  answered by a handler you install (auto-dismissed by default), and the
  console output and uncaught errors come back as structured data — argument
  values, source locations, error names and parsed stack frames — not as
  flattened strings.
- **Fetch safely.** Networking is SSRF-guarded by default (loopback,
  private, link-local and cloud-metadata addresses are blocked unless you
  opt in), with a real cookie jar, HTTP caching, redirects, and CORS.
- **Render output.** A PNG or JPEG screenshot of the viewport, the whole
  page, or any clipped region of it; a paginated PDF on real paper with
  embedded/subset fonts (selectable text, not outlines); or the final
  serialized HTML after scripts have run — plus a JSON display list for
  debugging.
- **Stay bounded.** Per-page request and byte budgets, a script execution
  timeout, and sandboxed `file://` access, so a hostile page can't run away
  with your process.

OxidePage is early-stage software: it targets headless automation and
offscreen rendering, not a general-purpose browser UI. See
[`docs/status.md`](docs/status.md) for exactly what's implemented and what
isn't yet.

## Installation

There are no published binaries or crates yet — build from source.

**Prerequisites:**
- A recent [Rust toolchain](https://rustup.rs/) (edition 2024).
- **`python3`** on `PATH` — the CSS engine's build script runs a code
  generator that needs it. No other native toolchain is required.

```sh
git clone https://github.com/n0madic/oxidepage
cd oxidepage
cargo build --release -p oxidepage-cli
```

The binary is at `target/release/oxidepage`. It's tested on Linux, macOS,
and Windows.

## Usage

### Render a screenshot, PDF, or HTML

```sh
# Screenshot the viewport (1280x800 by default)
oxidepage render page.html -o screenshot.png

# Screenshot the whole page, at 2x pixel density
oxidepage render https://example.com -o screenshot.png --full-page --dpr 2

# JPEG of one region of the document
oxidepage render page.html -o crop.jpg --clip 0,0,400,300 --quality 80

# Export as PDF, paginated onto A4 (fonts are embedded and subset —
# text stays selectable)
oxidepage render page.html -o page.pdf

# Serialize the DOM after scripts have run
oxidepage render page.html -o page.html
```

The output format is inferred from `-o`'s extension, or set explicitly with
`--format png|jpeg|pdf|html`.

### Evaluate a JavaScript expression

```sh
oxidepage eval page.html                                          # document.title
oxidepage eval https://example.com "document.querySelectorAll('a').length"
```

### Inspect layout and painting (debugging)

```sh
oxidepage dump-layout page.html                    # box tree with computed positions/sizes
oxidepage dump-display-list page.html -o list.json  # paint display list as JSON
```

### Useful options

All commands accept:

| Option | Effect |
| --- | --- |
| `--viewport WxH` | Layout viewport in CSS px (default `1280x800`) |
| `--settle-ms <ms>` | How long to let the event loop settle before capturing output (default `5000`) |
| `--allow-private` | Permit loopback/private hosts, for pointing at a local dev server (off by default) |
| `--max-bytes <sz>` / `--max-requests <n>` | Per-page network budget (e.g. `--max-bytes 1G`) |
| `--lazy-images` / `--no-lazy-images` | Fetch `<img>` only near the viewport, or always eagerly |
| `--quiet` | Suppress page `console.*` output on stderr (script errors and dialogs still print) |

`render` additionally takes:

| Option | Effect |
| --- | --- |
| `--format <png\|jpeg\|pdf\|html>` | Output format (default: inferred from `-o`'s extension) |
| `--dpr <N>` | Device pixel ratio for an image capture; also what the page sees as `window.devicePixelRatio` |
| `--full-page` | Capture the whole document instead of just the viewport |
| `--clip X,Y,W,H` | Capture one region of the document, in CSS px |
| `--quality <N>` | JPEG quality, 1–100 (default `80`) |
| `--paper <spec>` | PDF paper: `a3`, `a4`, `a5`, `letter`, `legal`, `tabloid`, or `WxH` in CSS px (default `a4`) |
| `--margin <spec>` | PDF margins in CSS px: one value, or `top,right,bottom,left` (default `38.4`, i.e. 0.4in) |
| `--scale <N>` | PDF zoom, 0.1–2 (default `1`) |
| `--landscape` | Swap the PDF paper's width and height |
| `--single-page` | One PDF page as tall as the whole document, instead of paginating |
| `--no-fit-to-width` | Don't shrink wide content to the PDF page width |
| `--no-print-background` | Omit element backgrounds from the PDF (on by default, unlike Chrome) |

Run `oxidepage --help` for the full reference.

## Using it as a library

The `page` crate exposes the same engine the CLI drives, as a Rust API. Add
it as a path or git dependency (there's no crates.io release yet):

```toml
[dependencies]
oxidepage-page = { git = "https://github.com/n0madic/oxidepage" }
```

```rust
use oxidepage_page::{Page, PageOptions, WaitUntil};
use std::time::Duration;

let page = Page::new(PageOptions::default())?;
page.navigate("https://example.com", WaitUntil::Load)?;
page.settle(Duration::from_secs(5));

let title = page.eval_to_string("document.title")?;
let png_bytes = page.screenshot(1.0);
```

Dialogs are answered by a handler you install; without one they are
auto-dismissed (`alert` returns, `confirm` is `false`, `prompt` is `null`).
Pass it in `PageOptions` if the page may open one from a script that runs
during the load:

```rust
use oxidepage_page::{DialogResponse, PageOptions};
use std::rc::Rc;

let page = Page::new(PageOptions {
    dialog_handler: Some(Rc::new(|request| {
        println!("{}: {}", request.kind.as_str(), request.message);
        DialogResponse::Accept
    })),
    ..PageOptions::default()
})?;
```

What the page said, and what went wrong, is pulled from three streams:

```rust
for message in page.drain_console() {
    // `message.args` holds a bounded preview of each argument's value;
    // `message.location` is the call site.
    println!("[{}] {}", message.level.as_str(), message.message);
}
for error in page.drain_errors() {
    // `error.kind`, `error.name` and `error.stack` are structured data.
    println!("{error}");
}
for dialog in page.drain_dialog_events() {
    println!("{}: {}", dialog.kind.as_str(), dialog.message);
}
```

## Documentation

- [`docs/development.md`](docs/development.md) — build/test/lint commands
  and the crate layout.
- [`docs/testing.md`](docs/testing.md) — the WPT/html5lib/golden/reftest
  conformance suites.
- [`docs/status.md`](docs/status.md) — implementation status, phase by
  phase.
- [`docs/rust-engine-design.md`](docs/rust-engine-design.md) and
  [`docs/adr/`](docs/adr/) — the architecture and the decisions behind it.

## License

MIT
