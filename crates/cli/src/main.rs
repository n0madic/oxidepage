//! `oxidepage` CLI (design doc §5.12).
//!
//! Phase 2 surface: `eval` loads a local HTML file (inline scripts execute
//! during the parse), settles the event loop, and evaluates an expression.
//! `render`/`dump-display-list` arrive with their phases.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use oxidepage_cdp::{CdpServer, ServerOptions};
use oxidepage_dom::decode::decode_document_bytes;
use oxidepage_engine::{Browser, BrowserOptions, ContextOptions};
use oxidepage_page::{
    DialogResponse, ImageFormat, Margins, Page, PageOptions, PaintOptions, PaperSize, PdfOptions,
    Rect, ScreenshotOptions, WaitUntil,
};

/// The viewport every command renders at without `--viewport`. A desktop-class
/// size, because 800×600 puts most of the modern web into its mobile layout;
/// the library's own `Viewport::default()` stays 800×600 (the size goldens,
/// reftests, and the WPT harness are written against).
const DEFAULT_VIEWPORT: oxidepage_page::Viewport = oxidepage_page::Viewport {
    width: 1280.0,
    height: 800.0,
    dpr: 1.0,
};

/// The per-page response byte budget every command runs with unless
/// `--max-bytes` says otherwise. Twice the library default: a real content-heavy
/// page fetches every image the moment it is not a viewport screenshot (which
/// is the only command that lazy-loads), and starving the tail of the page of
/// its fonts and logos is a worse failure than the memory it costs. Embedders
/// keep the conservative library default.
const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Upper bound on a single viewport dimension (CSS px), so an absurd
/// `--viewport` cannot drive an enormous raster allocation.
const MAX_VIEWPORT_DIM: f32 = 16384.0;

/// Bounds on the screenshot device-pixel-ratio.
const MIN_DPR: f32 = 0.1;
const MAX_DPR: f32 = 4.0;

/// Bounds on `--scale`, matching Chrome's `Page.printToPDF`.
const MIN_PDF_SCALE: f32 = 0.1;
const MAX_PDF_SCALE: f32 = 2.0;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("eval") => eval_command(&args[1..]),
        Some("dump-layout") => dump_layout_command(&args[1..]),
        Some("dump-display-list") => dump_display_list_command(&args[1..]),
        Some("render") => render_command(&args[1..]),
        Some("serve") => serve_command(&args[1..]),
        Some("--help" | "-h" | "help") | None => {
            usage();
            ExitCode::from(2)
        }
        Some(other) => {
            eprintln!("oxidepage: unknown command `{other}`");
            usage();
            ExitCode::from(2)
        }
    }
}

/// Event-loop settle budget when `--settle-ms` is absent. Real pages chain
/// analytics, ad, and lazy-loaded requests well past the first quiet moment;
/// 5 s is enough for the common ones to finish before output is taken.
const DEFAULT_SETTLE_MS: u64 = 5000;

fn usage() {
    eprintln!(
        "usage: oxidepage eval <file.html | http(s)://URL> [expression] [--viewport WxH] [--settle-ms <ms>] [--quiet]\n\
         \x20      oxidepage dump-layout <file.html | http(s)://URL> [--viewport WxH] [--settle-ms <ms>] [--quiet]\n\
         \x20      oxidepage dump-display-list <file.html | http(s)://URL> [--viewport WxH] [--settle-ms <ms>] [-o <file>] [--quiet]\n\
         \x20      oxidepage render <file.html | http(s)://URL> -o <out.{{png,jpg,pdf,html}}> [--format png|jpeg|pdf|html] [--viewport WxH] [--dpr N] [--full-page] [--clip X,Y,W,H] [--quality N] [--paper <name|WxH>] [--margin <px|t,r,b,l>] [--scale N] [--landscape] [--single-page] [--no-fit-to-width] [--no-print-background] [--settle-ms <ms>] [--quiet]\n\
         \x20      oxidepage serve [--port N] [--viewport WxH] [--allow-private]\n\
         \x20                      [--download-path DIR]\n\n\
         eval: loads a local HTML file or fetches a document over the network\n\
         (SSRF- and policy-checked), runs its scripts and the event loop until\n\
         it settles, then evaluates `expression` (default: `document.title`)\n\
         and prints the result.\n\n\
         dump-layout: loads the document the same way, lays it out, and prints\n\
         the box tree with computed positions/sizes.\n\n\
         dump-display-list: loads and lays out the document, then prints the\n\
         paint display list as JSON (stable, golden-friendly). Loads every image\n\
         eagerly — unlike a viewport screenshot of the same document, whose list\n\
         is the same one but built after lazy loading skipped the images below\n\
         the fold. Pass --lazy-images to see what the screenshot sees.\n\n\
         render: loads and lays out the document, then writes it as PNG, JPEG,\n\
         PDF, or HTML — chosen by --format, or by -o's extension if --format is\n\
         absent. An image rasterizes the viewport (or, with --full-page, the\n\
         whole document, or with --clip, any region of it); PDF paginates the\n\
         whole document onto real paper; HTML serializes the live DOM after the\n\
         engine has run (doctype included) — the final markup, not the input\n\
         file. A viewport image only fetches images near the viewport;\n\
         --full-page, --clip, pdf, and html fetch everything and are visible to\n\
         IntersectionObserver.\n\n\
         serve: starts a Chrome DevTools Protocol endpoint and prints its\n\
         WebSocket URL, then serves until Browser.close or Ctrl-C. This is what\n\
         puppeteer.connect({{ browserWSEndpoint }}) attaches to. The endpoint\n\
         binds 127.0.0.1 only and its URL carries a random token: anyone who can\n\
         reach it has full control of this process, so do not forward the port.\n\n\
         options:\n\
         \x20 --settle-ms <ms>  event-loop settle budget (default 5000)\n\
         \x20 --viewport WxH    layout viewport in CSS px (default 1280x800)\n\
         \x20 --format <fmt>    output format for render: png, jpeg, pdf, or html\n\
         \x20                   (default: inferred from -o's extension)\n\
         \x20 --dpr N           device pixel ratio; also what the page sees as\n\
         \x20                   window.devicePixelRatio (image output only, default 1)\n\
         \x20 --full-page       render the whole document, not just the viewport\n\
         \x20 --clip X,Y,W,H    render this region of the document, in CSS px\n\
         \x20 --quality N       JPEG quality, 1-100 (default 80)\n\
         \x20 --paper <spec>    PDF paper: a3, a4, a5, letter, legal, tabloid, or\n\
         \x20                   WxH in CSS px (default a4)\n\
         \x20 --margin <spec>   PDF margins in CSS px: one value, or t,r,b,l\n\
         \x20                   (default 38.4 = 0.4in)\n\
         \x20 --scale N         PDF zoom, 0.1-2 (default 1)\n\
         \x20 --landscape       swap the PDF paper's width and height\n\
         \x20 --single-page     one PDF page as tall as the whole document\n\
         \x20 --no-fit-to-width do not shrink wide content to the PDF page width\n\
         \x20 --no-print-background  omit element backgrounds from the PDF\n\
         \x20 --port N          serve: loopback port for the DevTools endpoint\n\
         \x20                   (default 0 = pick a free one and print it)\n\
         \x20 -o, --output <file>  write output to <file> instead of stdout\n\
         \x20 --quiet           suppress page console output\n\
         \x20 --max-bytes <sz>  per-page response byte budget (e.g. 1G, 2G; default 512M)\n\
         \x20 --max-requests <n> per-page request-count budget (default 500)\n\
         \x20 --lazy-images     fetch <img> only near the viewport (default: on\n\
         \x20                   for a viewport screenshot, off everywhere else)\n\
         \x20 --no-lazy-images  fetch every <img> eagerly\n\
         \x20 --allow-private   permit loopback/private hosts (local dev; off by default)\n\
         \x20 --download-path   write `Content-Disposition: attachment` responses here\n\
         \x20                   (without it, a download is refused rather than parsed)"
    );
}

fn dump_layout_command(args: &[String]) -> ExitCode {
    let (args, flags) = match extract_common_flags("dump-layout", args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut file: Option<&str> = None;
    let mut settle_ms: u64 = DEFAULT_SETTLE_MS;
    let mut quiet = false;
    let mut allow_private = false;
    let mut viewport: Option<oxidepage_page::Viewport> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--settle-ms" => {
                let Some(value) = iter.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("oxidepage dump-layout: --settle-ms requires a number");
                    return ExitCode::from(2);
                };
                settle_ms = value;
            }
            "--viewport" => {
                let Some(parsed) = iter.next().and_then(|v| parse_viewport(v)) else {
                    eprintln!("oxidepage dump-layout: --viewport expects WxH (e.g. 1280x720)");
                    return ExitCode::from(2);
                };
                viewport = Some(parsed);
            }
            "--quiet" => quiet = true,
            "--allow-private" => allow_private = true,
            other if file.is_none() => file = Some(other),
            other => {
                eprintln!("oxidepage dump-layout: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("oxidepage dump-layout: missing <file.html | URL>");
        usage();
        return ExitCode::from(2);
    };

    let page = match load_page(file, allow_private, viewport, flags, false, false) {
        Ok(page) => page,
        Err(code) => return code,
    };
    page.settle(Duration::from_millis(settle_ms));

    let dump = page.dump_layout();
    flush_page_output(&page, quiet);
    print!("{dump}");
    ExitCode::SUCCESS
}

fn dump_display_list_command(args: &[String]) -> ExitCode {
    let (args, flags) = match extract_common_flags("dump-display-list", args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut file: Option<&str> = None;
    let mut settle_ms: u64 = DEFAULT_SETTLE_MS;
    let mut quiet = false;
    let mut allow_private = false;
    let mut viewport: Option<oxidepage_page::Viewport> = None;
    let mut output: Option<&str> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--settle-ms" => {
                let Some(value) = iter.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("oxidepage dump-display-list: --settle-ms requires a number");
                    return ExitCode::from(2);
                };
                settle_ms = value;
            }
            "--viewport" => {
                let Some(parsed) = iter.next().and_then(|v| parse_viewport(v)) else {
                    eprintln!(
                        "oxidepage dump-display-list: --viewport expects WxH (e.g. 1280x720)"
                    );
                    return ExitCode::from(2);
                };
                viewport = Some(parsed);
            }
            "-o" | "--output" => {
                let Some(value) = iter.next() else {
                    eprintln!("oxidepage dump-display-list: -o requires a path");
                    return ExitCode::from(2);
                };
                output = Some(value);
            }
            "--quiet" => quiet = true,
            "--allow-private" => allow_private = true,
            other if file.is_none() => file = Some(other),
            other => {
                eprintln!("oxidepage dump-display-list: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("oxidepage dump-display-list: missing <file.html | URL>");
        usage();
        return ExitCode::from(2);
    };

    let page = match load_page(file, allow_private, viewport, flags, false, false) {
        Ok(page) => page,
        Err(code) => return code,
    };
    page.settle(Duration::from_millis(settle_ms));

    let json = page.display_list_json();
    flush_page_output(&page, quiet);
    match output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &json) {
                eprintln!("oxidepage dump-display-list: cannot write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{json}"),
    }
    ExitCode::SUCCESS
}

fn render_command(args: &[String]) -> ExitCode {
    let (args, flags) = match extract_common_flags("render", args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut file: Option<&str> = None;
    let mut settle_ms: u64 = DEFAULT_SETTLE_MS;
    let mut quiet = false;
    let mut allow_private = false;
    let mut viewport: Option<oxidepage_page::Viewport> = None;
    let mut output: Option<&str> = None;
    let mut dpr: f32 = 1.0;
    let mut full_page = false;
    let mut format_flag: Option<&str> = None;
    let mut clip: Option<Rect> = None;
    // The library's default, not a second copy of it.
    let mut quality: u8 = ScreenshotOptions::default().quality;
    let mut pdf = PdfOptions::default();
    let mut paint = PaintOptions::default();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--settle-ms" => {
                let Some(value) = iter.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("oxidepage render: --settle-ms requires a number");
                    return ExitCode::from(2);
                };
                settle_ms = value;
            }
            "--viewport" => {
                let Some(parsed) = iter.next().and_then(|v| parse_viewport(v)) else {
                    eprintln!("oxidepage render: --viewport expects WxH (e.g. 1280x720)");
                    return ExitCode::from(2);
                };
                viewport = Some(parsed);
            }
            "--dpr" => {
                let Some(value) = iter.next().and_then(|v| v.parse::<f32>().ok()) else {
                    eprintln!("oxidepage render: --dpr requires a number");
                    return ExitCode::from(2);
                };
                if !value.is_finite() || value <= 0.0 {
                    eprintln!("oxidepage render: --dpr must be a positive number");
                    return ExitCode::from(2);
                }
                dpr = value.clamp(MIN_DPR, MAX_DPR);
                if dpr != value {
                    eprintln!("oxidepage render: --dpr {value} clamped to {dpr}");
                }
            }
            "--format" => {
                let Some(value) = iter.next() else {
                    eprintln!("oxidepage render: --format requires a value (png, pdf, or html)");
                    return ExitCode::from(2);
                };
                format_flag = Some(value);
            }
            "-o" | "--output" => {
                let Some(value) = iter.next() else {
                    eprintln!("oxidepage render: -o requires a path");
                    return ExitCode::from(2);
                };
                output = Some(value);
            }
            "--clip" => {
                let Some(parsed) = iter.next().and_then(|v| parse_clip(v)) else {
                    eprintln!("oxidepage render: --clip expects X,Y,W,H in CSS px");
                    return ExitCode::from(2);
                };
                clip = Some(parsed);
            }
            "--quality" => {
                let Some(value) = iter.next().and_then(|v| v.parse::<u8>().ok()) else {
                    eprintln!("oxidepage render: --quality requires a number");
                    return ExitCode::from(2);
                };
                if !(1..=100).contains(&value) {
                    eprintln!("oxidepage render: --quality must be 1..=100");
                    return ExitCode::from(2);
                }
                quality = value;
            }
            "--paper" => {
                let Some(parsed) = iter.next().and_then(|v| parse_paper(v)) else {
                    eprintln!(
                        "oxidepage render: --paper expects a name (a3, a4, a5, letter, legal, tabloid) or WxH in CSS px"
                    );
                    return ExitCode::from(2);
                };
                pdf.paper = parsed;
            }
            "--margin" => {
                let Some(parsed) = iter.next().and_then(|v| parse_margins(v)) else {
                    eprintln!("oxidepage render: --margin expects <px> or <top,right,bottom,left>");
                    return ExitCode::from(2);
                };
                pdf.margins = parsed;
            }
            "--scale" => {
                let Some(value) = iter.next().and_then(|v| v.parse::<f32>().ok()) else {
                    eprintln!("oxidepage render: --scale requires a number");
                    return ExitCode::from(2);
                };
                if !value.is_finite() || value <= 0.0 {
                    eprintln!("oxidepage render: --scale must be a positive number");
                    return ExitCode::from(2);
                }
                let clamped = value.clamp(MIN_PDF_SCALE, MAX_PDF_SCALE);
                if clamped != value {
                    eprintln!("oxidepage render: --scale {value} clamped to {clamped}");
                }
                pdf.scale = clamped;
            }
            "--landscape" => pdf.landscape = true,
            "--single-page" => pdf.paginate = false,
            "--no-fit-to-width" => pdf.fit_to_width = false,
            "--no-print-background" => paint.print_background = false,
            "--full-page" => full_page = true,
            "--quiet" => quiet = true,
            "--allow-private" => allow_private = true,
            other if file.is_none() => file = Some(other),
            other => {
                eprintln!("oxidepage render: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("oxidepage render: missing <file.html | URL>");
        usage();
        return ExitCode::from(2);
    };
    let Some(output) = output else {
        eprintln!("oxidepage render: missing -o <out.{{png,pdf,html}}>");
        usage();
        return ExitCode::from(2);
    };
    let format = match detect_format(output, format_flag) {
        Ok(format) => format,
        Err(msg) => {
            eprintln!("oxidepage render: {msg}");
            return ExitCode::from(2);
        }
    };

    // `--dpr` always reaches the page (it is `window.devicePixelRatio`, and a
    // DPR media query picks a different asset), but only an image capture is
    // *rendered* at it. Say precisely that rather than claiming it is ignored.
    if dpr != 1.0 && !format.is_raster() {
        eprintln!(
            "oxidepage render: --dpr {dpr} sets the page's devicePixelRatio; \
             {format} output has no pixel density of its own"
        );
    }

    // Whole document capture: a full-page or clipped image, PDF, and HTML all
    // want every lazy-loaded image fetched and the IntersectionObserver root
    // grown to the document (ADR-0015) — only a plain viewport image stays
    // viewport-scoped. A clip can name any region of the page, including one
    // below the fold, so it is a whole-document capture too.
    let whole_document = full_page || clip.is_some() || !format.is_raster();

    // The device pixel ratio is the page's, not just the encoder's: a page that
    // reads `window.devicePixelRatio` (or matches a `-webkit-min-device-pixel-
    // ratio` query to pick a 2× asset) has to see the value it is rendered at.
    let viewport = Some(oxidepage_page::Viewport {
        dpr,
        ..viewport.unwrap_or(DEFAULT_VIEWPORT)
    });

    let page = match load_page(
        file,
        allow_private,
        viewport,
        flags,
        !whole_document,
        whole_document,
    ) {
        Ok(page) => page,
        Err(code) => return code,
    };
    page.settle(Duration::from_millis(settle_ms));

    // Anything still deferred has to land before a whole-document capture, or
    // it paints as a hole. (Lazy loading is off for these unless `--lazy-images`
    // forced it on.) HTML output paints nothing, so it waits for nothing.
    if whole_document && format != OutputFormat::Html {
        page.load_deferred_images(Duration::from_millis(settle_ms));
    }

    let bytes: Vec<u8> = match format {
        OutputFormat::Png | OutputFormat::Jpeg => page.screenshot_with(&ScreenshotOptions {
            dpr,
            full_page,
            clip,
            format: if format == OutputFormat::Jpeg {
                ImageFormat::Jpeg
            } else {
                ImageFormat::Png
            },
            quality,
            ..ScreenshotOptions::default()
        }),
        OutputFormat::Pdf => page.pdf(&pdf, &paint),
        OutputFormat::Html => page.document_html().into_bytes(),
    };
    flush_page_output(&page, quiet);
    // Empty output only signals failure for the two backends that actually
    // encode a binary format; `document_html()` always returns at least the
    // document element's markup.
    if format != OutputFormat::Html && bytes.is_empty() {
        eprintln!("oxidepage render: failed to produce {format} output");
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(output, &bytes) {
        eprintln!("oxidepage render: cannot write {output}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Flags every command accepts, pulled out before its own parser runs. `None`
/// falls back to the CLI's own defaults ([`DEFAULT_MAX_TOTAL_BYTES`], the
/// command's lazy-loading default) or the library's.
#[derive(Clone, Copy, Default)]
struct CommonFlags {
    max_total_bytes: Option<u64>,
    max_requests: Option<u32>,
    /// `--lazy-images` / `--no-lazy-images`.
    lazy_images: Option<bool>,
}

/// Parses a byte size such as `512M`, `2G`, or a raw byte count. A trailing
/// `K`/`M`/`G` (case-insensitive) multiplies by 1024/1024²/1024³.
fn parse_byte_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mult) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 1024_u64),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// Pulls the flags shared by every command (`--max-bytes <size>`,
/// `--max-requests <n>`, `--lazy-images` / `--no-lazy-images`) out of `args`
/// before the per-command parser runs, returning the remaining args and the
/// overrides.
fn extract_common_flags(
    cmd: &str,
    args: &[String],
) -> Result<(Vec<String>, CommonFlags), ExitCode> {
    let mut limits = CommonFlags::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--max-bytes" => {
                let Some(value) = iter.next().and_then(|v| parse_byte_size(v)) else {
                    eprintln!("oxidepage {cmd}: --max-bytes requires a size (e.g. 512M, 2G)");
                    return Err(ExitCode::from(2));
                };
                limits.max_total_bytes = Some(value);
            }
            "--max-requests" => {
                let Some(value) = iter.next().and_then(|v| v.parse::<u32>().ok()) else {
                    eprintln!("oxidepage {cmd}: --max-requests requires a number");
                    return Err(ExitCode::from(2));
                };
                limits.max_requests = Some(value);
            }
            "--lazy-images" => limits.lazy_images = Some(true),
            "--no-lazy-images" => limits.lazy_images = Some(false),
            _ => rest.push(arg.clone()),
        }
    }
    Ok((rest, limits))
}

/// Loads `file` (local path or http(s) URL) into a fresh page.
///
/// `lazy_default` is the command's own answer to "does this output need the
/// whole document?" — `--lazy-images` / `--no-lazy-images` overrides it.
fn load_page(
    file: &str,
    allow_private: bool,
    viewport: Option<oxidepage_page::Viewport>,
    flags: CommonFlags,
    lazy_default: bool,
    // The output covers the whole document (`--full-page`, `pdf`), so content
    // an IntersectionObserver gates on visibility has to render: none of the
    // document is below a fold when all of it is in the image.
    whole_document_visible: bool,
) -> Result<Page, ExitCode> {
    let is_http = file.starts_with("http://") || file.starts_with("https://");
    let local_html = if is_http {
        None
    } else {
        match std::fs::read(file) {
            Ok(bytes) => Some(decode_document_bytes(&bytes, None).text),
            Err(e) => {
                eprintln!("oxidepage: cannot read {file}: {e}");
                return Err(ExitCode::FAILURE);
            }
        }
    };
    // The CLI always builds its own policy: it renders whole real pages, which
    // routinely fetch past the library's conservative default budget. A
    // viewport screenshot loads only what it shows (`lazy_default`), but every
    // other command still fetches every image on the page. Only the byte budget
    // is raised; the SSRF and request-count defaults stand.
    let policy = {
        let mut p = allow_private
            .then(oxidepage_page::ResourcePolicy::permissive_localhost)
            .unwrap_or_default();
        p.max_total_bytes = flags.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        if let Some(requests) = flags.max_requests {
            p.max_requests = requests;
        }
        Some(p)
    };
    let page = match Page::new(PageOptions {
        url: (!is_http).then(|| file_url(Path::new(file))),
        policy,
        viewport: Some(viewport.unwrap_or(DEFAULT_VIEWPORT)),
        lazy_images: flags.lazy_images.unwrap_or(lazy_default),
        whole_document_visible,
        ..PageOptions::default()
    }) {
        Ok(page) => page,
        Err(e) => {
            eprintln!("oxidepage: failed to create page: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    let load_result = match &local_html {
        None => page.navigate(file, WaitUntil::Load),
        Some(html) => page.load_html(html),
    };
    if let Err(e) = load_result {
        eprintln!("oxidepage: load failed: {e}");
        return Err(ExitCode::FAILURE);
    }
    Ok(page)
}

/// `serve`: a CDP endpoint over a real [`Browser`], not the single `Page` every
/// other subcommand drives.
///
/// This is the one subcommand that goes through `oxidepage-engine`. The others
/// deliberately do not (ADR-0027): they own one page, drive it synchronously
/// through `settle`, and would gain nothing but a thread hop from a browser.
fn serve_command(args: &[String]) -> ExitCode {
    let (args, flags) = match extract_common_flags("serve", args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let mut port: u16 = 0;
    let mut allow_private = false;
    let mut viewport = None;
    let mut download_path: Option<std::path::PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                let Some(value) = iter.next().and_then(|v| v.parse::<u16>().ok()) else {
                    eprintln!("oxidepage serve: --port requires a number (0-65535)");
                    return ExitCode::from(2);
                };
                port = value;
            }
            "--viewport" => {
                let Some(value) = iter.next().and_then(|v| parse_viewport(v)) else {
                    eprintln!("oxidepage serve: --viewport requires WxH (e.g. 1280x800)");
                    return ExitCode::from(2);
                };
                viewport = Some(value);
            }
            "--allow-private" => allow_private = true,
            "--download-path" => {
                let Some(value) = iter.next() else {
                    eprintln!("oxidepage serve: --download-path requires a directory");
                    return ExitCode::from(2);
                };
                // Resolved and created up front, so a bad path is a startup
                // error rather than a download that silently fails an hour in.
                if let Err(e) = std::fs::create_dir_all(value) {
                    eprintln!("oxidepage serve: cannot use `{value}` for downloads: {e}");
                    return ExitCode::from(2);
                }
                match std::path::Path::new(value).canonicalize() {
                    Ok(path) => download_path = Some(path),
                    Err(e) => {
                        eprintln!("oxidepage serve: cannot resolve `{value}`: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("oxidepage serve: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }

    let mut policy = allow_private
        .then(oxidepage_page::ResourcePolicy::permissive_localhost)
        .unwrap_or_default();
    policy.max_total_bytes = flags.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
    if let Some(requests) = flags.max_requests {
        policy.max_requests = requests;
    }

    // These land on the *default* context, not a new one: a driver's
    // `Target.createTarget` without a `browserContextId` creates its page there,
    // so anything configured on a fresh context would be invisible to it.
    let browser = match Browser::new(BrowserOptions {
        policy,
        default_context: ContextOptions {
            viewport: Some(viewport.unwrap_or(DEFAULT_VIEWPORT)),
            lazy_images: flags.lazy_images.unwrap_or(false),
            // A driver expects `alert()` to be reported and answered over the
            // protocol, not auto-dismissed behind its back. `Ask` is what makes
            // `Page.javascriptDialogOpening` and `handleJavaScriptDialog` real.
            dialog_policy: oxidepage_engine::DialogPolicy::Ask {
                timeout: oxidepage_engine::DEFAULT_DIALOG_TIMEOUT,
            },
            // No directory *is* deny: an attachment navigation is refused and
            // recorded rather than parsed as HTML (ADR-0032 D13). A driver can
            // still turn it on per target with `Browser.setDownloadBehavior`.
            download_path,
            ..ContextOptions::default()
        },
        ..BrowserOptions::default()
    }) {
        Ok(browser) => browser,
        Err(e) => {
            eprintln!("oxidepage serve: failed to start the browser: {e}");
            return ExitCode::FAILURE;
        }
    };

    let server = match CdpServer::start(
        browser,
        ServerOptions {
            port,
            ..ServerOptions::default()
        },
    ) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("oxidepage serve: {e}");
            return ExitCode::FAILURE;
        }
    };

    // stdout, not stderr: this line is the whole point of the command, and a
    // caller scripting `oxidepage serve` needs to be able to read it back.
    println!("{}", server.browser_ws_url());
    eprintln!(
        "oxidepage: DevTools endpoint on {} — anyone who can reach it controls this process",
        server.http_url()
    );
    server.wait();
    ExitCode::SUCCESS
}

fn eval_command(args: &[String]) -> ExitCode {
    let (args, flags) = match extract_common_flags("eval", args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut file: Option<&str> = None;
    let mut expression: Option<&str> = None;
    let mut settle_ms: u64 = DEFAULT_SETTLE_MS;
    let mut quiet = false;
    let mut allow_private = false;
    let mut viewport = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--settle-ms" => {
                let Some(value) = iter.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("oxidepage eval: --settle-ms requires a number");
                    return ExitCode::from(2);
                };
                settle_ms = value;
            }
            "--viewport" => {
                let Some(value) = iter.next().and_then(|v| parse_viewport(v)) else {
                    eprintln!("oxidepage eval: --viewport requires WxH");
                    return ExitCode::from(2);
                };
                viewport = Some(value);
            }
            "--quiet" => quiet = true,
            "--allow-private" => allow_private = true,
            other if file.is_none() => file = Some(other),
            other if expression.is_none() => expression = Some(other),
            other => {
                eprintln!("oxidepage eval: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("oxidepage eval: missing <file.html | URL>");
        usage();
        return ExitCode::from(2);
    };
    let expression = expression.unwrap_or("document.title");

    // Secure by default; `--allow-private` opens loopback/RFC1918 for local
    // development against a private server.
    let page = match load_page(file, allow_private, viewport, flags, false, false) {
        Ok(page) => page,
        Err(code) => return code,
    };
    page.settle(Duration::from_millis(settle_ms));

    let result = page.eval_to_string(expression);
    flush_page_output(&page, quiet);
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("oxidepage eval: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Stack frames printed under one error before the rest are summarized.
const MAX_PRINTED_FRAMES: usize = 10;

/// Indentation levels rendered for `console.group` nesting.
///
/// The page's group depth is uncapped — `console.group()` with no label costs
/// it nothing — so the *renderer* clamps. Without this, a page that opens a
/// million groups makes every subsequent line allocate a multi-megabyte indent.
const MAX_GROUP_INDENT: usize = 16;

/// Prints captured console output, dialogs and script errors to stderr.
///
/// `--quiet` suppresses console chatter only. Errors and dialogs always print:
/// a dialog is an interaction the page expected and did not get, which is a
/// diagnosis rather than noise.
fn flush_page_output(page: &Page, quiet: bool) {
    if !quiet {
        for message in page.drain_console() {
            let indent = "  ".repeat((message.group_depth as usize).min(MAX_GROUP_INDENT));
            let level = message.level.as_str();
            match &message.location {
                Some(at) => eprintln!(
                    "[console.{level}] {indent}{} ({}:{}:{})",
                    message.message, at.url, at.line, at.column
                ),
                None => eprintln!("[console.{level}] {indent}{}", message.message),
            }
        }
    }
    for dialog in page.drain_dialog_events() {
        let answer = match &dialog.response {
            DialogResponse::Dismiss => "dismissed".to_owned(),
            DialogResponse::Accept => "accepted".to_owned(),
            DialogResponse::AcceptWith(text) => format!("accepted with {text:?}"),
        };
        eprintln!(
            "[dialog.{}] {} -> {answer}",
            dialog.kind.as_str(),
            dialog.message
        );
    }
    for error in page.drain_errors() {
        eprintln!("[script error] {error}");
        for frame in error.stack.iter().take(MAX_PRINTED_FRAMES) {
            eprintln!("    {frame}");
        }
        if error.stack.len() > MAX_PRINTED_FRAMES {
            eprintln!(
                "    … {} more frames",
                error.stack.len() - MAX_PRINTED_FRAMES
            );
        }
    }
}

/// `render`'s output format — inferred from `-o`'s extension, or set
/// explicitly with `--format`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Png,
    Jpeg,
    Pdf,
    Html,
}

impl OutputFormat {
    /// Whether the output is a rasterized image, i.e. whether `--dpr`,
    /// `--clip` and `--full-page` mean anything for it.
    fn is_raster(self) -> bool {
        matches!(self, OutputFormat::Png | OutputFormat::Jpeg)
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutputFormat::Png => "PNG",
            OutputFormat::Jpeg => "JPEG",
            OutputFormat::Pdf => "PDF",
            OutputFormat::Html => "HTML",
        })
    }
}

/// `--format` always wins; otherwise inferred from `output`'s extension.
/// Both are matched case-insensitively.
fn detect_format(output: &str, format_flag: Option<&str>) -> Result<OutputFormat, String> {
    let by_name = |s: &str| match s {
        "png" => Some(OutputFormat::Png),
        "jpeg" | "jpg" => Some(OutputFormat::Jpeg),
        "pdf" => Some(OutputFormat::Pdf),
        "html" => Some(OutputFormat::Html),
        _ => None,
    };
    if let Some(f) = format_flag {
        return by_name(&f.to_lowercase())
            .ok_or_else(|| format!("unknown --format `{f}` (expected png, jpeg, pdf, or html)"));
    }
    let ext = Path::new(output)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "htm" => Ok(OutputFormat::Html),
        _ => by_name(&ext).ok_or_else(|| {
            format!(
                "cannot infer format from `{output}` (expected .png/.jpg/.pdf/.html) — pass --format <png|jpeg|pdf|html>"
            )
        }),
    }
}

/// Parses a `WxH` viewport spec, clamping each dimension into a sane range so
/// an absurd value cannot drive an enormous raster allocation. A clamped
/// dimension is reported, since the output size would otherwise silently differ
/// from what was asked for.
fn parse_viewport(spec: &str) -> Option<oxidepage_page::Viewport> {
    let (w, h) = spec.split_once(['x', 'X'])?;
    let width = w.parse::<f32>().ok()?;
    let height = h.parse::<f32>().ok()?;
    if !width.is_finite() || !height.is_finite() {
        return None;
    }
    let clamped_width = width.clamp(1.0, MAX_VIEWPORT_DIM);
    let clamped_height = height.clamp(1.0, MAX_VIEWPORT_DIM);
    if clamped_width != width || clamped_height != height {
        eprintln!(
            "oxidepage: viewport {width}x{height} clamped to {clamped_width}x{clamped_height}"
        );
    }
    Some(oxidepage_page::Viewport {
        width: clamped_width,
        height: clamped_height,
        dpr: 1.0,
    })
}

/// Parses `--clip X,Y,W,H` (document CSS px). A non-positive size is rejected:
/// an empty clip would produce a 1×1 image rather than an error.
fn parse_clip(spec: &str) -> Option<Rect> {
    let mut parts = spec.split(',').map(|p| p.trim().parse::<f32>());
    let (x, y, w, h) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if parts.next().is_some() || ![x, y, w, h].iter().all(|v| v.is_finite()) || w <= 0.0 || h <= 0.0
    {
        return None;
    }
    Some(Rect::from_xywh(x, y, w, h))
}

/// Parses `--paper`: a known name, or an explicit `WxH` in CSS px.
fn parse_paper(spec: &str) -> Option<PaperSize> {
    if let Some(named) = PaperSize::by_name(spec) {
        return Some(named);
    }
    let (w, h) = spec.split_once(['x', 'X'])?;
    let width = w.trim().parse::<f32>().ok()?;
    let height = h.trim().parse::<f32>().ok()?;
    (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
        .then_some(PaperSize { width, height })
}

/// Parses `--margin`: one value for all four sides, or `top,right,bottom,left`
/// in CSS px.
fn parse_margins(spec: &str) -> Option<Margins> {
    let values: Option<Vec<f32>> = spec
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
        })
        .collect();
    let values = values?;
    match values.as_slice() {
        [all] => Some(Margins::uniform(*all)),
        [top, right, bottom, left] => Some(Margins {
            top: *top,
            right: *right,
            bottom: *bottom,
            left: *left,
        }),
        _ => None,
    }
}

/// A best-effort `file://` URL for a local path.
fn file_url(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", absolute.display())
}
