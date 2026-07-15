//! DOM name validation and qualified names.
//!
//! Three different productions are in play and it is easy to conflate them, so
//! the tables below are lifted from the WPT files that pin each one:
//! `dom/nodes/Document-createElement.html`, `Document-createElementNS.js`,
//! `productions.js` (attribute names) and
//! `Document-createProcessingInstruction.js`.

use oxidepage_page::{PageOptions, load_html_page};

fn page() -> oxidepage_page::Page {
    load_html_page("<!DOCTYPE html><body>", PageOptions::default()).unwrap()
}

fn s(page: &oxidepage_page::Page, expr: &str) -> String {
    page.eval_to_string(expr).unwrap()
}

/// The `name` of the DOMException `expr` throws, or `"NO THROW"`.
fn throws(page: &oxidepage_page::Page, expr: &str) -> String {
    s(
        page,
        &format!(
            "(() => {{ try {{ {expr}; return 'NO THROW'; }} catch (e) {{ return e.name; }} }})()"
        ),
    )
}

/// A JS string literal for `s`, escaping non-ASCII as `\u{...}` so the test
/// source stays readable.
fn js_str(s: &str) -> String {
    let body: String = s
        .chars()
        .map(|c| match c {
            '\'' | '\\' => format!("\\{c}"),
            c if c.is_ascii_graphic() || c == ' ' => c.to_string(),
            c => format!("\\u{{{:x}}}", c as u32),
        })
        .collect();
    format!("'{body}'")
}

#[test]
fn create_element_validates_the_element_local_name() {
    let page = page();
    // Every name the HTML parser can produce is creatable from script, so `<`,
    // `}` and a leading combining character are all fine; only a name that
    // could not round-trip through a tag is rejected.
    for valid in [
        "foo",
        "f1oo",
        ":",
        ":foo",
        "f:oo",
        "foo:",
        "f:o:o",
        "f::oo",
        "foo:0",
        "f}oo",
        "foo}",
        "f<oo",
        "\u{300}foo",
        "\u{300}",
        "\u{ffff}foo",
    ] {
        assert_eq!(
            throws(&page, &format!("document.createElement({})", js_str(valid))),
            "NO THROW",
            "createElement({valid:?}) should be accepted"
        );
    }
    for invalid in [
        "", "1foo", "1:foo", "fo o", "}foo", "<foo", "foo>", "<foo>", "-foo", ".foo",
    ] {
        assert_eq!(
            throws(
                &page,
                &format!("document.createElement({})", js_str(invalid))
            ),
            "InvalidCharacterError",
            "createElement({invalid:?}) should throw"
        );
    }
}

#[test]
fn create_element_ns_validates_and_extracts() {
    let page = page();
    let ex = "http://example.com/";
    let xml = "http://www.w3.org/XML/1998/namespace";
    let xmlns = "http://www.w3.org/2000/xmlns/";

    // (namespace, qualifiedName, expected exception name)
    let cases: &[(Option<&str>, &str, &str)] = &[
        // A qualified name is split on its *first* colon; both halves are then
        // checked, and a character error outranks a namespace error.
        (Some(ex), "f:oo", "NO THROW"),
        (Some(ex), "f:o:o", "NO THROW"),
        (Some(ex), "f::oo", "NO THROW"),
        (Some(ex), "0:a", "NO THROW"),
        (Some(ex), "a:_", "NO THROW"),
        (Some(ex), "a:\u{300}", "NO THROW"),
        (Some(ex), "a.b:c", "NO THROW"),
        (Some(ex), "a:0", "InvalidCharacterError"),
        (Some(ex), ":foo", "InvalidCharacterError"),
        (Some(ex), "foo:", "InvalidCharacterError"),
        (Some(ex), "namespaceURI:^", "InvalidCharacterError"),
        (Some(ex), "namespaceURI:a ", "InvalidCharacterError"),
        // Prefix without a namespace.
        (None, "foo", "NO THROW"),
        (None, "f:oo", "NamespaceError"),
        (None, "f:o:o", "NamespaceError"),
        (None, "null:xml", "NamespaceError"),
        (None, ":foo", "InvalidCharacterError"),
        (None, "1foo", "InvalidCharacterError"),
        // `xml` and `xmlns` are bound to their namespaces, in both directions.
        (Some(ex), "xml:foo", "NamespaceError"),
        (Some(ex), "XML:foo", "NO THROW"),
        (Some(xml), "xml:foo", "NO THROW"),
        (
            Some("http://www.w3.org/xml/1998/namespace"),
            "xml:foo",
            "NamespaceError",
        ),
        (Some(ex), "xmlns", "NamespaceError"),
        (Some(ex), "xmlns:foo", "NamespaceError"),
        (Some(ex), "XMLNS", "NO THROW"),
        (Some(ex), "test:xmlns", "NO THROW"),
        (Some(xmlns), "xmlns", "NO THROW"),
        (Some(xmlns), "xmlns:foo", "NO THROW"),
        (Some(xmlns), "foo", "NamespaceError"),
        (Some(xmlns), "foo:xmlns", "NamespaceError"),
        // Character errors are raised before namespace errors.
        (Some(xmlns), "1foo", "InvalidCharacterError"),
        (Some(xmlns), "foo:", "InvalidCharacterError"),
    ];
    for (ns, qualified, expected) in cases {
        let ns = ns.map_or_else(|| "null".to_owned(), js_str);
        assert_eq!(
            throws(
                &page,
                &format!("document.createElementNS({ns}, {})", js_str(qualified))
            ),
            *expected,
            "createElementNS({ns}, {qualified:?})"
        );
    }
}

#[test]
fn tag_name_and_node_name_are_qualified() {
    let page = page();
    let html = "http://www.w3.org/1999/xhtml";
    let svg = "http://www.w3.org/2000/svg";

    // HTML elements in an HTML document report the ASCII-uppercased *qualified*
    // name — the prefix is part of it and gets uppercased too.
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{html}','x:b').tagName")
        ),
        "X:B"
    );
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{html}','x:b').nodeName")
        ),
        "X:B"
    );
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{html}','x:b').localName")
        ),
        "b"
    );
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{html}','x:b').prefix")
        ),
        "x"
    );
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{html}','i').tagName")
        ),
        "I"
    );

    // Non-HTML namespaces keep their case, prefix and all.
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{svg}','s:SVG').tagName")
        ),
        "s:SVG"
    );
    assert_eq!(
        s(
            &page,
            &format!("document.createElementNS('{svg}','textPath').tagName")
        ),
        "textPath"
    );
    assert_eq!(
        s(
            &page,
            "document.createElementNS('http://example.com/','mixedCase').tagName"
        ),
        "mixedCase"
    );

    // createElement never produces a prefix: the whole string is the local name.
    assert_eq!(s(&page, "document.createElement('f:o:o').tagName"), "F:O:O");
    assert_eq!(
        s(&page, "document.createElement('f:o:o').localName"),
        "f:o:o"
    );
    assert_eq!(s(&page, "document.createElement('f:o:o').prefix"), "null");
}

#[test]
fn attribute_names_use_the_lax_production() {
    let page = page();
    // Attribute names have no constraint on their first character.
    for valid in [
        "x",
        "X",
        ":",
        "a:0",
        "invalid^Name",
        "\\",
        "'",
        "\"",
        "0",
        "0:a",
        ":a",
        "x:y:x",
        "~",
    ] {
        let el = "document.createElement('foo')";
        assert_eq!(
            s(
                &page,
                &format!(
                    "(() => {{ const e = {el}; e.setAttribute({n}, 'test'); \
                     return e.getAttribute({n}); }})()",
                    n = js_str(valid)
                )
            ),
            "test",
            "setAttribute({valid:?}) should round-trip"
        );
        assert_eq!(
            s(
                &page,
                &format!(
                    "(() => {{ const e = {el}; return e.toggleAttribute({n}) + '/' + e.hasAttribute({n}); }})()",
                    n = js_str(valid)
                )
            ),
            "true/true",
            "toggleAttribute({valid:?}) should round-trip"
        );
    }
    // Only the empty name is rejected.
    for op in [
        "setAttribute('', 'test')",
        "toggleAttribute('')",
        "setAttributeNS('a', '', 'fail')",
        // `b:` is a valid Name but not a valid QName: the local half is empty.
        "setAttributeNS('a', 'b:', 'fail')",
    ] {
        assert_eq!(
            throws(&page, &format!("document.createElement('foo').{op}")),
            "InvalidCharacterError",
            "{op} should throw"
        );
    }
}

#[test]
fn attributes_are_addressed_by_qualified_name() {
    let page = page();
    // `getAttribute`/`setAttribute`/`removeAttribute` match an attribute's
    // *qualified* name, so a namespaced attribute answers to `prefix:local`
    // and is updated in place rather than duplicated.
    assert_eq!(
        s(
            &page,
            "(() => { const e = document.createElement('baz'); \
              e.setAttributeNS('foo', 'foo:bar', '1'); \
              const first = e.getAttribute('foo:bar'); \
              e.setAttribute('foo:bar', '2'); \
              const a = e.attributes[0]; \
              return [first, e.getAttribute('foo:bar'), e.attributes.length, \
                      a.name, a.localName, a.prefix, a.namespaceURI, \
                      e.getAttributeNames().join()].join('/'); })()"
        ),
        "1/2/1/foo:bar/bar/foo/foo/foo:bar"
    );
    // ...and removing by qualified name finds it.
    assert_eq!(
        s(
            &page,
            "(() => { const e = document.createElement('baz'); \
              e.setAttributeNS('foo', 'foo:bar', '1'); \
              e.removeAttribute('foo:bar'); \
              return e.attributes.length + '/' + e.hasAttribute('foo:bar'); })()"
        ),
        "0/false"
    );
    // An unprefixed setAttribute on an HTML element lowercases the name, and
    // getNamedItem lowercases the same way.
    assert_eq!(
        s(
            &page,
            "(() => { const e = document.createElement('div'); \
              e.setAttribute('CHEEseCaKe', 'tasty'); \
              return [e.getAttributeNS('', 'CHEEseCaKe'), e.getAttributeNS('', 'cheesecake'), \
                      e.attributes.getNamedItem('ALIGN'), \
                      e.attributes.getNamedItem('CHEESECAKE').value].map(String).join('/'); })()"
        ),
        "null/tasty/null/tasty"
    );
}

#[test]
fn processing_instruction_target_uses_the_xml_name_production() {
    let page = page();
    // Unlike element names, a PI target is still held to the strict XML `Name`
    // production: U+00B7 is a NameChar but not a NameStartChar, and U+00D7 is
    // neither.
    for valid in ["xml:fail", "A\u{b7}A", "a0"] {
        assert_eq!(
            throws(
                &page,
                &format!(
                    "document.createProcessingInstruction({}, 'x')",
                    js_str(valid)
                )
            ),
            "NO THROW",
            "createProcessingInstruction({valid:?})"
        );
    }
    for invalid in ["\u{b7}A", "\u{d7}A", "A\u{d7}", "\\A", "\u{c}", "0"] {
        assert_eq!(
            throws(
                &page,
                &format!(
                    "document.createProcessingInstruction({}, 'x')",
                    js_str(invalid)
                )
            ),
            "InvalidCharacterError",
            "createProcessingInstruction({invalid:?})"
        );
    }
    // Data containing `?>` is still rejected.
    assert_eq!(
        throws(&page, "document.createProcessingInstruction('A', '?>')"),
        "InvalidCharacterError"
    );
}

/// `Node.lookupNamespaceURI`/`lookupPrefix`/`isDefaultNamespace`, lifted from
/// `dom/nodes/Node-lookupNamespaceURI.html`: the DOM §4.4 "locate a
/// namespace"/"locate a namespace prefix" algorithms, including the
/// `xmlns`/`xmlns:*` attribute walk, empty-string normalization, and the
/// per-node-kind delegation (Document → document element, Doctype/Fragment →
/// null, everything else → nearest *element* parent, not just any ancestor).
#[test]
fn node_lookup_namespace_methods_follow_the_locate_a_namespace_algorithm() {
    let page = page();
    let xml_ns = "http://www.w3.org/XML/1998/namespace";
    let xmlns_ns = "http://www.w3.org/2000/xmlns/";

    // DocumentFragment and DocumentType: always null / always-default.
    assert_eq!(
        s(
            &page,
            "document.createDocumentFragment().lookupNamespaceURI(null)"
        ),
        "null"
    );
    assert_eq!(
        s(
            &page,
            "document.createDocumentFragment().isDefaultNamespace(null)"
        ),
        "true"
    );
    assert_eq!(
        s(&page, "document.doctype.lookupNamespaceURI('foo')"),
        "null"
    );

    // An element with an explicit default and prefixed xmlns declarations.
    // Each assertion re-runs this setup inside its own IIFE — top-level
    // `const` bindings persist in the realm's global lexical environment, so
    // reusing an un-wrapped `const el = ...` across separate `eval()` calls
    // would throw a redeclaration `SyntaxError` on the second call.
    let setup = "const el = document.createElementNS('fooNamespace', 'prefix:elem'); \
                 el.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:bar', 'barURI'); \
                 el.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns', 'bazURI');";
    let lookup = |expr: &str| format!("(() => {{ {setup} return {expr}; }})()");
    assert_eq!(s(&page, &lookup("el.lookupNamespaceURI(null)")), "bazURI");
    // Empty-string prefix normalizes to null, same result.
    assert_eq!(s(&page, &lookup("el.lookupNamespaceURI('')")), "bazURI");
    assert_eq!(s(&page, &lookup("el.lookupNamespaceURI('bar')")), "barURI");
    assert_eq!(
        s(&page, &lookup("el.lookupNamespaceURI('xmlns')")),
        xmlns_ns
    );
    assert_eq!(s(&page, &lookup("el.lookupNamespaceURI('xml')")), xml_ns);
    // `prefix` maps to the element's own namespace only via its *own* prefix
    // matching, not via an attribute — `fooNamespace` is not itself an xmlns
    // declaration target.
    assert_eq!(
        s(&page, &lookup("el.lookupNamespaceURI('prefix')")),
        "fooNamespace"
    );
    assert_eq!(s(&page, &lookup("el.isDefaultNamespace('bazURI')")), "true");
    assert_eq!(
        s(&page, &lookup("el.isDefaultNamespace('barURI')")),
        "false"
    );

    // A child text/comment node inherits through its parent *element* — but a
    // node whose direct parent is not an element (e.g. a comment appended
    // straight to `document`) does not climb any further.
    assert_eq!(
        s(
            &page,
            &lookup(
                "(() => { const c = document.createComment('x'); el.appendChild(c); return c.lookupNamespaceURI(null); })()"
            )
        ),
        "bazURI"
    );
    assert_eq!(
        s(
            &page,
            "document.appendChild(document.createComment('x')).lookupNamespaceURI('bar')"
        ),
        "null"
    );

    // An `xmlns:foo=""` (empty value) declaration is present but returns null,
    // not the empty string.
    assert_eq!(
        s(
            &page,
            "(() => { const el = document.createElementNS('ns', 'e'); \
              el.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:foo', ''); \
              return el.lookupNamespaceURI('foo') === null; })()"
        ),
        "true"
    );

    // Document delegates to its document element.
    assert_eq!(
        s(&page, "document.lookupNamespaceURI(null)"),
        "http://www.w3.org/1999/xhtml"
    );
    assert_eq!(
        s(
            &page,
            "document.isDefaultNamespace('http://www.w3.org/1999/xhtml')"
        ),
        "true"
    );

    // `lookupPrefix`: the reverse walk, matching namespace to a declared
    // prefix, recursing through ancestor elements.
    assert_eq!(
        s(
            &page,
            "(() => { const el = document.createElementNS('testNS', 'r'); \
              el.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:t', 'testNS'); \
              const child = document.createElementNS('testNS', 'child'); \
              el.appendChild(child); \
              return child.lookupPrefix('testNS'); })()"
        ),
        "t"
    );
    // Null/empty namespace always answers null, without even looking at the tree.
    assert_eq!(s(&page, "document.lookupPrefix(null)"), "null");
    assert_eq!(s(&page, "document.lookupPrefix('')"), "null");
}

/// `getElementsByTagName`/`getElementsByTagNameNS`, lifted from
/// `dom/nodes/Document-Element-getElementsByTagName.js` and
/// `...getElementsByTagNameNS.js`: HTML elements match case-insensitively by
/// lowering the *query* (never the stored name, so a mixed-case HTML element
/// built via `createElementNS` can never match any query), matching is
/// against the *qualified* name (prefix included), and the NS-qualified
/// variant never case-folds.
#[test]
fn get_elements_by_tag_name_matches_the_qualified_name_and_folds_case_only_for_html() {
    let page = page();

    // Case-insensitive matching for a plain (parser-lowercased) HTML element.
    assert_eq!(
        s(
            &page,
            "(() => { const d = document.createElement('div'); document.body.appendChild(d); \
              return document.getElementsByTagName('DIV').length; })()"
        ),
        "1"
    );

    // An HTML-namespace element with a non-lowercase local name (only reachable
    // via `createElementNS`) never matches, in either case of the query.
    assert_eq!(
        s(
            &page,
            "(() => { const t = document.createElementNS('http://www.w3.org/1999/xhtml', 'I'); \
              document.body.appendChild(t); \
              return document.getElementsByTagName('I').length + '/' + \
                     document.getElementsByTagName('i').length; })()"
        ),
        "0/0"
    );

    // A prefixed, non-HTML-namespace element matches only its full qualified
    // name, case-sensitively.
    assert_eq!(
        s(
            &page,
            "(() => { const t = document.createElementNS('test', 'te:st'); \
              document.body.appendChild(t); \
              return [document.getElementsByTagName('st').length, \
                      document.getElementsByTagName('te:st').length, \
                      document.getElementsByTagName('te:ST').length].join('/'); })()"
        ),
        "0/1/0"
    );

    // `getElementsByTagNameNS`: namespace `"*"` and localName `"*"` are
    // wildcards; `null`/`""` both mean the null namespace; no case-folding
    // even for HTML elements.
    assert_eq!(
        s(
            &page,
            "(() => { const t = document.createElementNS('test', 'body'); \
              document.body.appendChild(t); \
              return document.getElementsByTagNameNS('test', 'body').length; })()"
        ),
        "1"
    );
    assert_eq!(
        s(
            &page,
            "(() => { const t = document.createElementNS('http://www.w3.org/1999/xhtml', 'ABC'); \
              document.body.appendChild(t); \
              return document.getElementsByTagNameNS('http://www.w3.org/1999/xhtml', 'abc').length + \
                     '/' + \
                     document.getElementsByTagNameNS('http://www.w3.org/1999/xhtml', 'ABC').length; })()"
        ),
        "0/1"
    );
    assert_eq!(
        s(
            &page,
            "(() => { const t = document.createElementNS('', 'body'); \
              document.body.appendChild(t); \
              return document.getElementsByTagNameNS('', '*').length + '/' + \
                     document.getElementsByTagNameNS(null, '*').length; })()"
        ),
        "1/1"
    );

    // Live: a collection reflects later insertions/removals without re-querying.
    assert_eq!(
        s(
            &page,
            "(() => { const list = document.getElementsByTagNameNS('test', 'abc'); \
              const before = list.length; \
              const t = document.createElementNS('test', 'abc'); \
              document.body.appendChild(t); \
              const after = list.length; \
              document.body.removeChild(t); \
              return before + '/' + after + '/' + list.length; })()"
        ),
        "0/1/0"
    );

    // Same live behavior and case-fold rule via `Element.prototype.getElementsByTagName`.
    assert_eq!(
        s(
            &page,
            "(() => { const parent = document.createElement('section'); \
              document.body.appendChild(parent); \
              const child = document.createElement('span'); \
              parent.appendChild(child); \
              return parent.getElementsByTagName('SPAN').length; })()"
        ),
        "1"
    );
}
