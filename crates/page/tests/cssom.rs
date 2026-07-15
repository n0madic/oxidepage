//! Phase 4 CSSOM tests (WP-H): `el.style` (inline, writable), `getComputedStyle`
//! (computed, read-only), camelCase/dashed/indexed property access, and author
//! stylesheet cascade observed through `getComputedStyle`.

use oxidepage_page::{PageOptions, load_html_page};

/// Loads a document and evaluates `expr`, returning its string value.
fn eval(html: &str, expr: &str) -> String {
    let page = load_html_page(html, PageOptions::default()).unwrap();
    page.eval_to_string(expr).expect("eval")
}

const DIV: &str = "<!DOCTYPE html><html><body><div id=d>hi</div></body></html>";

#[test]
fn inline_set_property_is_visible_to_computed_style() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.setProperty('color','rgb(0, 128, 0)');\
             getComputedStyle(d).color",
        ),
        "rgb(0, 128, 0)"
    );
}

#[test]
fn camel_case_and_dashed_accessors_round_trip() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.backgroundColor='rgb(1, 2, 3)';\
             [d.style['background-color'], getComputedStyle(d).backgroundColor].join('|')",
        ),
        "rgb(1, 2, 3)|rgb(1, 2, 3)"
    );
}

#[test]
fn important_priority_is_reported() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.setProperty('color','red','important');\
             d.style.getPropertyPriority('color')",
        ),
        "important"
    );
}

#[test]
fn remove_property_returns_old_value_and_clears() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.color='blue';\
             const before=d.style.getPropertyValue('color');\
             const removed=d.style.removeProperty('color');\
             String(before===removed && before!=='' && d.style.getPropertyValue('color')==='')",
        ),
        "true"
    );
}

#[test]
fn css_text_setter_replaces_the_whole_block() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.cssText='color: red; margin-top: 5px';\
             [getComputedStyle(d).color, getComputedStyle(d).marginTop].join('|')",
        ),
        "rgb(255, 0, 0)|5px"
    );
}

#[test]
fn computed_declarations_are_read_only() {
    assert_eq!(
        eval(
            DIV,
            "try {\
               getComputedStyle(document.getElementById('d')).setProperty('color','red');\
               'no-throw'\
             } catch (e) { e.name }",
        ),
        "NoModificationAllowedError"
    );
}

#[test]
fn computed_style_exposes_indexed_longhands() {
    assert_eq!(
        eval(
            DIV,
            "const cs=getComputedStyle(document.getElementById('d'));\
             [cs.length>100, typeof cs[0]==='string' && cs[0].length>0].join('|')",
        ),
        "true|true"
    );
}

#[test]
fn author_stylesheet_cascade_is_visible_to_computed_style() {
    assert_eq!(
        eval(
            "<!DOCTYPE html><html><head><style>#d{color: rgb(9, 9, 9)}</style></head>\
             <body><div id=d>hi</div></body></html>",
            "getComputedStyle(document.getElementById('d')).color",
        ),
        "rgb(9, 9, 9)"
    );
}

// === WP-I: document.styleSheets, CSSStyleSheet, CSSRule(List), CSSStyleRule ===

const SHEET_DOC: &str = "<!DOCTYPE html><html><head><style>#d{color: rgb(9, 9, 9)}</style></head>\
     <body><div id=d>hi</div></body></html>";

#[test]
fn document_style_sheets_lists_author_sheets() {
    assert_eq!(eval(SHEET_DOC, "document.styleSheets.length"), "1");
}

#[test]
fn style_sheet_list_and_sheet_have_stable_identity() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "[document.styleSheets===document.styleSheets,\
              document.styleSheets[0]===document.styleSheets[0]].join('|')",
        ),
        "true|true"
    );
}

#[test]
fn style_rule_exposes_type_selector_and_declarations() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const r=document.styleSheets[0].cssRules[0];\
             [r.type, r.selectorText, r.style.color].join('|')",
        ),
        "1|#d|rgb(9, 9, 9)"
    );
}

#[test]
fn insert_rule_is_visible_to_computed_style() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             s.insertRule('#d { background-color: rgb(1, 2, 3) }', s.cssRules.length);\
             getComputedStyle(document.getElementById('d')).backgroundColor",
        ),
        "rgb(1, 2, 3)"
    );
}

#[test]
fn delete_rule_reindexes_the_list() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             s.insertRule('#d{margin-top: 7px}', s.cssRules.length);\
             const grew=s.cssRules.length;\
             s.deleteRule(grew-1);\
             [grew, s.cssRules.length].join('|')",
        ),
        "2|1"
    );
}

#[test]
fn rule_style_mutation_affects_computed_style() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const r=document.styleSheets[0].cssRules[0];\
             r.style.color='rgb(4, 5, 6)';\
             getComputedStyle(document.getElementById('d')).color",
        ),
        "rgb(4, 5, 6)"
    );
}

#[test]
fn disabling_a_sheet_removes_its_cascade() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             s.disabled=true;\
             const off=getComputedStyle(document.getElementById('d')).color;\
             s.disabled=false;\
             const on=getComputedStyle(document.getElementById('d')).color;\
             [off!=='rgb(9, 9, 9)', on==='rgb(9, 9, 9)'].join('|')",
        ),
        "true|true"
    );
}

#[test]
fn insert_rule_out_of_range_throws_index_size_error() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "try { document.styleSheets[0].insertRule('#d{}', 99); 'no-throw' }\
             catch (e) { e.name }",
        ),
        "IndexSizeError"
    );
}

#[test]
fn rule_css_text_serializes_the_rule() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const t=document.styleSheets[0].cssRules[0].cssText;\
             String(t.includes('#d') && t.toLowerCase().includes('color'))",
        ),
        "true"
    );
}

// === Regression tests for the Phase 4 code review ===

#[test]
fn set_property_empty_value_removes_before_priority_check() {
    // CSSOM setProperty step 3 (empty value → remove) precedes step 4 (priority
    // validation): a garbage priority must not prevent the removal.
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.color='blue';\
             d.style.setProperty('color','','garbage');\
             d.style.getPropertyValue('color')",
        ),
        ""
    );
}

#[test]
fn rule_style_and_cssrules_honor_same_object() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             const same_rule=s.cssRules[0]===s.cssRules[0];\
             const r=s.cssRules[0];\
             const same_style=r.style===r.style;\
             [same_rule, same_style].join('|')",
        ),
        "true|true"
    );
}

/// Regression: `CSSRuleList.item` caches a wrapper per index, and the list is
/// `[SameObject]`. `insertRule`/`deleteRule` shift the underlying rules, so the
/// cache must be dropped — otherwise `cssRules[0]` keeps answering with the rule
/// that used to sit there while `cssRules.length` already reports the new count.
#[test]
fn rule_list_cache_is_invalidated_by_rule_mutations() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             const first=s.cssRules[0].cssText;\
             s.insertRule('#inserted { color: rgb(9, 9, 9) }', 0);\
             const after_insert=s.cssRules[0].cssText;\
             const shifted=s.cssRules[1].cssText;\
             s.deleteRule(0);\
             const after_delete=s.cssRules[0].cssText;\
             [after_insert.includes('#inserted'), shifted===first, after_delete===first].join('|')",
        ),
        "true|true|true"
    );
}

#[test]
fn rule_style_parent_rule_chain_has_identity_and_owner() {
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             const r=s.cssRules[0];\
             const back=r.style.parentRule.parentStyleSheet;\
             [back===s, r.style.parentRule.parentStyleSheet.ownerNode===s.ownerNode].join('|')",
        ),
        "true|true"
    );
}

#[test]
fn cached_sheet_follows_style_element_reparse() {
    // Editing the <style> text replaces the underlying stylesheet (new Arc); a
    // previously-obtained CSSStyleSheet must reflect the new rules and mutate
    // the live cascade, not a detached snapshot.
    assert_eq!(
        eval(
            SHEET_DOC,
            "const s=document.styleSheets[0];\
             const d=document.getElementById('d');\
             document.querySelector('style').firstChild.data='#d{color: rgb(3, 3, 3)}';\
             getComputedStyle(d).color;\
             s.insertRule('#d{background-color: rgb(4, 4, 4)}', s.cssRules.length);\
             [s.cssRules.length, getComputedStyle(d).color, getComputedStyle(d).backgroundColor].join('|')",
        ),
        "2|rgb(3, 3, 3)|rgb(4, 4, 4)"
    );
}

#[test]
fn get_computed_style_sees_synchronously_added_style_element() {
    assert_eq!(
        eval(
            DIV,
            "const s=document.createElement('style');\
             s.textContent='#d{color: rgb(2, 4, 6)}';\
             document.head.appendChild(s);\
             getComputedStyle(document.getElementById('d')).color",
        ),
        "rgb(2, 4, 6)"
    );
}

#[test]
fn media_query_style_element_applies_conditionally() {
    // A non-matching media query keeps the sheet inert; a matching one applies.
    // Parsing the attribute as a real media list (not a textual @media wrapper)
    // is what makes both directions correct.
    let inert = eval(
        "<!DOCTYPE html><html><head><style media=\"print\">#d{color: rgb(5, 5, 5)}</style></head>\
         <body><div id=d>hi</div></body></html>",
        "getComputedStyle(document.getElementById('d')).color",
    );
    let active = eval(
        "<!DOCTYPE html><html><head><style media=\"screen\">#d{color: rgb(5, 5, 5)}</style></head>\
         <body><div id=d>hi</div></body></html>",
        "getComputedStyle(document.getElementById('d')).color",
    );
    assert_ne!(inert, "rgb(5, 5, 5)", "print media inert on screen");
    assert_eq!(active, "rgb(5, 5, 5)", "screen media active");
}

#[test]
fn computed_style_object_stays_live_across_mutations() {
    // The per-view computed cache must invalidate when the DOM changes: a held
    // getComputedStyle object reflects a later mutation.
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             const cs=getComputedStyle(d);\
             const before=cs.color;\
             d.style.color='rgb(1, 1, 1)';\
             const after=cs.color;\
             [before!==after, after].join('|')",
        ),
        "true|rgb(1, 1, 1)"
    );
}

#[test]
fn inline_style_reflects_attribute_replacement() {
    // The inline block cache is keyed by the attribute text: replacing the
    // whole `style` attribute must be seen on the next read.
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style.color='blue';\
             const s=d.style;\
             const first=s.getPropertyValue('color');\
             d.setAttribute('style','color: lime');\
             [first, s.getPropertyValue('color')].join('|')",
        ),
        "blue|lime"
    );
}

#[test]
fn element_style_string_assignment_forwards_to_css_text() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style='color: rgb(7, 7, 7)';\
             getComputedStyle(d).color",
        ),
        "rgb(7, 7, 7)"
    );
}

/// `getComputedStyle(el, null)` must behave like the one-argument form: the
/// nullable `pseudoElt` selects the element's own style rather than parsing as
/// an unsupported pseudo-element.
#[test]
fn get_computed_style_accepts_a_null_pseudo_element() {
    assert_eq!(
        eval(
            DIV,
            "const d=document.getElementById('d');\
             d.style='color: rgb(7, 7, 7)';\
             [getComputedStyle(d, null).color,\
              getComputedStyle(d, undefined).color,\
              getComputedStyle(d, '').color].join('|')",
        ),
        "rgb(7, 7, 7)|rgb(7, 7, 7)|rgb(7, 7, 7)"
    );
    assert_eq!(
        eval(
            DIV,
            "String(getComputedStyle(document.getElementById('d'), '::marker'))",
        ),
        "null"
    );
}
