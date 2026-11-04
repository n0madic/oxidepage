# ADR-0041: `@page` rules, parsed beside stylo

- Status: accepted
- Date: 2026-11-04

## Context

`@page` is how print CSS sets the paper and its margins
(`@page { size: A4; margin: 2cm }`), and this engine's main offscreen output
besides PNG is PDF. The rules were silently dropped, so `cssRules` had no
`CSSPageRule`, and the PDF used the caller's paper and margins whatever the
document said.

stylo has the whole `@page` model in every build: `CssRule::Page`,
`PageRule`, `PageSelectors` and the stylist's page-rule map. But its rule
parser only accepts the at-rule in its Gecko build
(`"page" if cfg!(feature = "gecko")`). The `size` descriptor is also a
Gecko-only longhand, so even a `PageRule` built by hand cannot hold it in
its declaration block. Patching stylo would mean a fork of a crate that
moves in lockstep with `selectors`, `cssparser`, `stylo_taffy` and `taffy`.

## Decision

1. **Parse `@page` ourselves, and splice it into stylo's own rule lists.**
   Every sheet goes through one funnel, `loader::stylesheet_from_str`:
   - `<style>`;
   - `<link>`, via `stylesheet_from_bytes`, which runs the same charset steps
     `Stylesheet::from_bytes` does, so the decoded text is available;
   - `@import`ed sheets;
   - `replaceSync`.

   After stylo parses a sheet, `page_rules::splice_into` scans the same text
   with cssparser. It inserts each `@page` as an ordinary `CssRule::Page` into
   the list stylo built, at its source position, which is known because every
   stylo rule records its `source_location`. It descends into `@media`,
   `@supports`, `@layer`, `@container` and `@document` the same way. There is
   one list per sheet, so `cssRules` indices, `insertRule`, `deleteRule` and
   `effective_rules` (media, `@supports`, `@import`, `disabled`) all treat a
   page rule like any other rule. `insertRule("@page …")`, which stylo's Servo
   build rejects as a syntax error, is parsed here too. It obeys the same
   bounds and `@import`/`@namespace` hierarchy checks.

2. **The declarations stylo knows go in stylo's block, and `size` goes in a
   side table.** stylo parses the `@page` body
   (`parse_property_declaration_list` under `CssRuleType::Page`), so margins
   get stylo's own grammar. A second pass over the same tokens picks out
   `size`.
   - The table is keyed by the rule's `Arc`, compared with `ptr_eq`. Each
     entry holds a clone of that `Arc`, so a key's address cannot be reused.
   - It is pruned of rules nothing else references.
   - It is thread-local: a page and its sheets live on one thread (design §7),
     and the `@import` loader builds sheets with no handle on the
     `StyleEngine`.

3. **CSSOM.** `CSSPageRule` (type 6) has `selectorText`, which reparses with
   stylo's `PageSelectors` and ignores an invalid value, and `style`. `style`
   is the ordinary `CSSStyleDeclaration`, which asks the side table about
   `size` and the block about everything else. `cssText` is serialized here,
   because stylo's `PageRule::to_css` does not know `size`. `CSSPageRule`
   inherits `CSSRule`, not `CSSGroupingRule`, because the margin at-rules
   (`@top-left`, …) are not parsed.

4. **PDF.** `StyleEngine::page_style` cascades the effective `@page` rules of
   the document's sheets and `adoptedStyleSheets`:
   - later rules win, and `!important` beats a later normal declaration;
   - only rules without a selector count, since one export has one paper and
     one set of margins.

   `Page::pdf` applies the result the way Chrome's `printToPDF` does. `@page`
   margins always replace the caller's: a caller's margins are the defaults a
   page box starts from. `size` replaces the paper only under
   `PdfOptions::prefer_css_page_size`, which is off by default; the CDP
   `preferCSSPageSize` parameter sets it. The CLI sets it unless `--paper` or
   `--landscape` is given, because for a CLI user "this document's page size"
   is the useful default. A percentage margin resolves against the final paper:
   its width for left and right, its height for top and bottom.

## Consequences

- `@page` rules are visible and editable through the CSSOM, and print CSS
  shapes the PDF. WPT `css/cssom/cssom-pagerule.html` and the page-rule row of
  `cssom-ruleTypeAndOrder.html` pass.
- **`@media print { @page … }` and `<link media=print>` are not seen.** Media
  queries are evaluated against the device the document renders with, and
  there is no print device (no print relayout, ADR-0026).
- **`:first`, `:left`, `:right`, `:blank` and named pages do not affect the
  PDF.** They parse, serialize and reach the CSSOM, but every page of an export
  shares one style.
- **`page-orientation` and the margin at-rules are not parsed.** Nothing would
  honour them, and a descriptor that reads back but changes nothing is the
  fake P6 forbids. `CSSPageDescriptors`' IDL attributes (`style.size`,
  `style.marginTop`) are absent too, so
  `css/cssom/page-descriptors.html` still fails.
- `size` accepts `auto`, one or two non-negative lengths (absolute units, plus
  `em`/`rem` at the initial 16px), a named size with an optional orientation,
  or an orientation alone. Anything else, such as a `calc()`, is dropped like
  any invalid declaration.
- The thread-local table is the one piece of style state outside a
  `StyleEngine`. A future stylo that parses `@page` and `size` outside Gecko
  makes all of this deletable: the rules would arrive in the same lists, of
  the same type.
