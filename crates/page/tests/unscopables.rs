//! WebIDL `@@unscopables`: the `[Unscopable]` members of `ChildNode` and
//! `ParentNode` are hidden from `with`, which is observable because an event
//! handler content attribute is compiled inside `with (document) with
//! (element)`.

use oxidepage_page::{Page, PageOptions, load_html_page};

fn page_with(html: &str) -> Page {
    load_html_page(html, PageOptions::default()).expect("page")
}

fn eval(page: &Page, expr: &str) -> String {
    page.eval_to_string(expr).expect("eval")
}

/// Each prototype that includes an `[Unscopable]` member carries its own
/// object: null prototype, the members in declaration order, each `true`, and
/// the property itself read-only, non-enumerable and configurable.
#[test]
fn prototypes_carry_their_own_unscopables() {
    let page = page_with("<!doctype html>");
    assert_eq!(
        eval(
            &page,
            "const keys = (i) => Object.keys(i.prototype[Symbol.unscopables]).join('+');\
             [keys(Element), keys(CharacterData), keys(DocumentType), keys(Document),\
              keys(DocumentFragment)].join(' ')",
        ),
        "prepend+append+replaceChildren+before+after+replaceWith+remove \
         before+after+replaceWith+remove \
         before+after+replaceWith+remove \
         prepend+append+replaceChildren \
         prepend+append+replaceChildren"
    );
    assert_eq!(
        eval(
            &page,
            "const u = Element.prototype[Symbol.unscopables];\
             const d = Object.getOwnPropertyDescriptor(Element.prototype, Symbol.unscopables);\
             [Object.getPrototypeOf(u) === null, u.remove, d.writable, d.enumerable,\
              d.configurable, Object.hasOwn(Node.prototype, Symbol.unscopables),\
              HTMLDivElement.prototype[Symbol.unscopables] === u].join()",
        ),
        "true,true,false,false,true,false,true"
    );
}

/// The observable effect: inside a handler attribute, a bare `remove` names
/// the global, while `this.remove` is still the element's method.
#[test]
fn a_handler_attribute_sees_the_global_not_the_element_member() {
    let page = page_with(
        "<!doctype html><div id=d onclick='window.r1 = typeof remove; window.r2 = typeof this.remove; \
         window.r3 = typeof append; window.r4 = typeof querySelector'></div>",
    );
    assert_eq!(
        eval(
            &page,
            "window.remove = 'global'; window.append = 'global';\
             document.getElementById('d').dispatchEvent(new Event('click'));\
             [r1, r2, r3, r4].join()",
        ),
        // `querySelector` is scopable, so the element's own method wins.
        "string,function,string,function"
    );
}
