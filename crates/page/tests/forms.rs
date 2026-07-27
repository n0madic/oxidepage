//! Form controls end to end: the IDL surface through the JS bindings, the
//! `click()`/`focus()` interaction layer, and — the load-bearing one — that a
//! checkedness change from script actually **re-runs the cascade**, so a
//! `:checked` rule restyles.

use oxidepage_page::{PageOptions, load_html_page};

const FORM: &str = r#"<!DOCTYPE html><html><head><style>
    input:checked + label { color: rgb(255, 0, 0); }
    input:disabled { color: rgb(0, 0, 255); }
    input:focus { outline-color: rgb(0, 255, 0); }
  </style></head><body>
  <form id="f" action="/act">
    <input id="text" name="t" value="v1">
    <input id="cb" name="c" type="checkbox" value="cv"><label id="cblabel">check</label>
    <input id="r1" type="radio" name="g" value="1" checked>
    <input id="r2" type="radio" name="g" value="2">
    <textarea id="ta" name="ta">tav</textarea>
    <select id="sel" name="s">
      <option value="x">x</option>
      <option value="y" selected>y</option>
    </select>
    <button id="btn" name="b" value="bv">go</button>
  </form>
  </body></html>"#;

fn page() -> oxidepage_page::Page {
    load_html_page(FORM, PageOptions::default()).unwrap()
}

fn eval_str(page: &oxidepage_page::Page, source: &str) -> String {
    let value = page
        .eval(source)
        .unwrap_or_else(|e| panic!("eval failed: {e:?}"));
    value
        .as_str()
        .unwrap_or_else(|| panic!("`{source}` did not evaluate to a string"))
        .to_owned()
}

fn eval_bool(page: &oxidepage_page::Page, source: &str) -> bool {
    page.eval(source)
        .unwrap_or_else(|e| panic!("eval failed: {e:?}"))
        .truthy()
}

#[test]
fn control_values_are_readable_from_script() {
    let page = page();
    assert_eq!(
        eval_str(&page, "document.getElementById('text').value"),
        "v1"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('ta').value"),
        "tav"
    );
    assert_eq!(eval_str(&page, "document.getElementById('sel').value"), "y");
    assert_eq!(
        eval_str(&page, "document.getElementById('btn').value"),
        "bv"
    );
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').selectedIndex)"
        ),
        "1"
    );
    assert!(eval_bool(
        &page,
        "document.getElementById('cb').checked === false"
    ));
    assert!(eval_bool(
        &page,
        "document.getElementById('r1').checked === true"
    ));
    assert_eq!(
        eval_str(&page, "document.getElementById('text').type"),
        "text"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('sel').type"),
        "select-one"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('btn').type"),
        "submit",
        "a button with no type defaults to submit"
    );
}

/// The dirty value flag, observed through the IDL: writing `value` must not
/// move the content attribute.
#[test]
fn setting_value_from_script_leaves_the_attribute_alone() {
    let page = page();
    page.eval("document.getElementById('text').value = 'typed'")
        .unwrap();

    assert_eq!(
        eval_str(&page, "document.getElementById('text').value"),
        "typed"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('text').defaultValue"),
        "v1"
    );
    assert_eq!(
        eval_str(
            &page,
            "document.getElementById('text').getAttribute('value')"
        ),
        "v1"
    );
}

/// **The invalidation path.** Setting `checked` from script must snapshot the
/// old element state and hint a restyle, or the `input:checked + label` rule
/// would never re-match and the label would keep its old colour.
#[test]
fn checking_a_box_from_script_restyles_a_checked_rule() {
    let page = page();
    let color = "getComputedStyle(document.getElementById('cblabel')).color";

    assert_eq!(
        eval_str(&page, color),
        "rgb(0, 0, 0)",
        "unchecked: the sibling rule does not apply"
    );

    page.eval("document.getElementById('cb').checked = true")
        .unwrap();

    assert_eq!(
        eval_str(&page, color),
        "rgb(255, 0, 0)",
        "the :checked rule must re-match after a scripted checkedness change"
    );

    page.eval("document.getElementById('cb').checked = false")
        .unwrap();
    assert_eq!(eval_str(&page, color), "rgb(0, 0, 0)");
}

/// The same, for a `disabled` attribute change.
#[test]
fn disabling_from_script_restyles_a_disabled_rule() {
    let page = page();
    let color = "getComputedStyle(document.getElementById('text')).color";
    assert_eq!(eval_str(&page, color), "rgb(0, 0, 0)");

    page.eval("document.getElementById('text').disabled = true")
        .unwrap();

    assert_eq!(eval_str(&page, color), "rgb(0, 0, 255)");
    assert!(eval_bool(
        &page,
        "document.getElementById('text').matches(':disabled')"
    ));
}

#[test]
fn radio_group_exclusivity_holds_through_the_idl() {
    let page = page();
    page.eval("document.getElementById('r2').checked = true")
        .unwrap();

    assert!(eval_bool(&page, "document.getElementById('r2').checked"));
    assert!(eval_bool(
        &page,
        "document.getElementById('r1').checked === false"
    ));
    assert_eq!(
        eval_str(
            &page,
            "String(document.querySelectorAll('input:checked').length)"
        ),
        "1",
        "scoped to inputs: a `<option selected>` is `:checked` too"
    );
}

#[test]
fn select_options_and_value_round_trip() {
    let page = page();
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').options.length)"
        ),
        "2"
    );
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').selectedOptions.length)"
        ),
        "1"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('sel').options[1].text"),
        "y"
    );
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').options[1].index)"
        ),
        "1"
    );

    page.eval("document.getElementById('sel').value = 'x'")
        .unwrap();
    assert_eq!(eval_str(&page, "document.getElementById('sel').value"), "x");
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').selectedIndex)"
        ),
        "0"
    );

    // A `long` round-trips through the setter, -1 included.
    page.eval("document.getElementById('sel').selectedIndex = -1")
        .unwrap();
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('sel').selectedIndex)"
        ),
        "-1"
    );
}

#[test]
fn form_elements_is_live_and_ordered() {
    let page = page();
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('f').elements.length)"
        ),
        "7"
    );
    assert_eq!(
        eval_str(&page, "String(document.getElementById('f').length)"),
        "7"
    );
    assert_eq!(
        eval_str(&page, "document.getElementById('f').elements[0].id"),
        "text"
    );
    assert!(eval_bool(
        &page,
        "document.getElementById('text').form === document.getElementById('f')"
    ));

    // Live: a control added later shows up.
    page.eval(
        "const i = document.createElement('input');\
         i.name = 'added';\
         document.getElementById('f').appendChild(i);",
    )
    .unwrap();
    assert_eq!(
        eval_str(
            &page,
            "String(document.getElementById('f').elements.length)"
        ),
        "8"
    );
}

/// `form.reset()` drops the dirty flags.
#[test]
fn form_reset_restores_the_defaults() {
    let page = page();
    page.eval(
        "document.getElementById('text').value = 'typed';\
         document.getElementById('cb').checked = true;\
         document.getElementById('sel').value = 'x';\
         document.getElementById('f').reset();",
    )
    .unwrap();

    assert_eq!(
        eval_str(&page, "document.getElementById('text').value"),
        "v1"
    );
    assert!(eval_bool(
        &page,
        "document.getElementById('cb').checked === false"
    ));
    assert_eq!(eval_str(&page, "document.getElementById('sel').value"), "y");
}

/// `submit()`/`requestSubmit()` exist since navigation does (ADR-0022,
/// reversing ADR-0019's deliberate absence). Behavior lives in
/// `crates/page/tests/navigation.rs`, which has a server to submit to; this
/// only pins the surface feature detection sees.
#[test]
fn form_submission_surface_is_present() {
    let page = page();
    for expr in [
        "typeof document.getElementById('f').submit === 'function'",
        "typeof document.getElementById('f').requestSubmit === 'function'",
        "typeof document.getElementById('f').reset === 'function'",
        "typeof SubmitEvent === 'function'",
        "'submitter' in SubmitEvent.prototype",
    ] {
        assert!(eval_bool(&page, expr), "{expr}");
    }
}

/// `requestSubmit` validates its argument before doing anything: a non-submit
/// button is a `TypeError`, a submit button owned by another form a
/// `NotFoundError`.
#[test]
fn request_submit_validates_the_submitter() {
    let page = page();
    page.eval(
        "window.errs = [];\
         const f = document.getElementById('f');\
         const notAButton = document.createElement('div');\
         try { f.requestSubmit(notAButton); } catch (e) { window.errs.push(e.name); }\
         const other = document.createElement('form');\
         const btn = document.createElement('button');\
         other.appendChild(btn);\
         document.body.appendChild(other);\
         try { f.requestSubmit(btn); } catch (e) { window.errs.push(e.name); }",
    )
    .unwrap();
    assert_eq!(
        eval_str(&page, "window.errs.join(',')"),
        "TypeError,NotFoundError"
    );
}

// --------------------------------------------------------------- interaction

/// `click()` on a checkbox runs the activation behavior: it toggles, and fires
/// `click`, then `input`, then `change`. The toggle lands *before* the `click`
/// event propagates (HTML's legacy-pre-activation behavior), so every one of the
/// three listeners reads the new checkedness.
#[test]
fn clicking_a_label_activates_its_control() {
    let page = page();
    // `for` names the checkbox; clicking the label's text must toggle it, and
    // exactly once.
    page.eval(
        "window.log = [];\
         const cb = document.getElementById('cb');\
         cb.addEventListener('click', () => window.log.push('click:' + cb.checked));\
         document.getElementById('cblabel').htmlFor = 'cb';\
         document.getElementById('cblabel').click();",
    )
    .unwrap();
    assert_eq!(eval_str(&page, "window.log.join(',')"), "click:true");
    assert!(eval_bool(&page, "document.getElementById('cb').checked"));

    // Clicking the control *inside* a label must not toggle twice: the label's
    // activation behavior does nothing for a click on interactive content.
    page.eval(
        "const wrap = document.createElement('label');\
         const inner = document.createElement('input');\
         inner.type = 'checkbox';\
         wrap.appendChild(inner);\
         document.body.appendChild(wrap);\
         window.count = 0;\
         inner.addEventListener('click', () => window.count++);\
         inner.click();\
         window.innerChecked = inner.checked;",
    )
    .unwrap();
    assert_eq!(eval_str(&page, "String(window.count)"), "1");
    assert!(eval_bool(&page, "window.innerChecked"));
}

#[test]
fn click_toggles_a_checkbox_and_fires_input_and_change() {
    let page = page();
    page.eval(
        "window.log = [];\
         const cb = document.getElementById('cb');\
         for (const t of ['click', 'input', 'change']) {\
             cb.addEventListener(t, () => window.log.push(t + ':' + cb.checked));\
         }\
         cb.click();",
    )
    .unwrap();

    assert_eq!(
        eval_str(&page, "window.log.join(',')"),
        "click:true,input:true,change:true",
        "the pre-activation toggle precedes the click event, not the other way round"
    );
    assert!(eval_bool(&page, "document.getElementById('cb').checked"));

    page.eval("document.getElementById('cb').click()").unwrap();
    assert!(
        eval_bool(&page, "document.getElementById('cb').checked === false"),
        "a second click toggles back"
    );
}

/// `preventDefault()` on the click runs the legacy-canceled-activation behavior:
/// the speculative toggle is undone, and neither `input` nor `change` fires.
#[test]
fn preventing_the_click_cancels_the_toggle() {
    let page = page();
    page.eval(
        "window.after = [];\
         const cb = document.getElementById('cb');\
         cb.addEventListener('click', (e) => e.preventDefault());\
         cb.addEventListener('input', () => { window.after.push('input'); });\
         cb.addEventListener('change', () => { window.after.push('change'); });\
         cb.click();",
    )
    .unwrap();

    assert!(eval_bool(
        &page,
        "document.getElementById('cb').checked === false"
    ));
    assert_eq!(eval_str(&page, "window.after.join(',')"), "");
}

/// A disabled control is not activatable: `click()` fires nothing at all.
#[test]
fn clicking_a_disabled_control_does_nothing() {
    let page = page();
    page.eval(
        "window.fired = false;\
         const cb = document.getElementById('cb');\
         cb.disabled = true;\
         cb.addEventListener('click', () => { window.fired = true; });\
         cb.click();",
    )
    .unwrap();

    assert!(eval_bool(&page, "window.fired === false"));
    assert!(eval_bool(
        &page,
        "document.getElementById('cb').checked === false"
    ));
}

/// `focus()` moves `document.activeElement` and fires the four-event sequence.
/// jQuery delegates focus through `focusin`, so the bubbling pair matters.
#[test]
fn focus_fires_focus_and_focusin_and_moves_active_element() {
    let page = page();
    assert_eq!(
        eval_str(&page, "document.activeElement.tagName"),
        "BODY",
        "with nothing focused, activeElement is the body"
    );

    page.eval(
        "window.log = [];\
         const text = document.getElementById('text');\
         text.addEventListener('focus', () => window.log.push('focus'));\
         document.getElementById('f').addEventListener('focusin', () => window.log.push('focusin@form'));\
         text.focus();",
    )
    .unwrap();

    assert_eq!(
        eval_str(&page, "window.log.join(',')"),
        "focus,focusin@form",
        "focus does not bubble; focusin does"
    );
    assert_eq!(eval_str(&page, "document.activeElement.id"), "text");
    assert!(eval_bool(
        &page,
        "document.getElementById('text').matches(':focus')"
    ));
    assert!(eval_bool(
        &page,
        "document.getElementById('f').matches(':focus-within')"
    ));
}

/// Moving focus blurs the old element first.
#[test]
fn moving_focus_blurs_the_previous_element() {
    let page = page();
    page.eval(
        "window.log = [];\
         const text = document.getElementById('text');\
         const cb = document.getElementById('cb');\
         text.addEventListener('blur', () => window.log.push('blur:text'));\
         cb.addEventListener('focus', () => window.log.push('focus:cb'));\
         text.focus();\
         cb.focus();",
    )
    .unwrap();

    assert_eq!(
        eval_str(&page, "window.log.join(',')"),
        "blur:text,focus:cb"
    );
    assert_eq!(eval_str(&page, "document.activeElement.id"), "cb");

    page.eval("document.getElementById('cb').blur()").unwrap();
    assert_eq!(
        eval_str(&page, "document.activeElement.tagName"),
        "BODY",
        "blur() returns focus to the body"
    );
}

/// Removing the focused element must not leave `activeElement` pointing at a
/// node that is no longer in the document.
#[test]
fn removing_the_focused_element_resets_active_element() {
    let page = page();
    page.eval(
        "const text = document.getElementById('text');\
         text.focus();\
         text.remove();",
    )
    .unwrap();

    assert_eq!(eval_str(&page, "document.activeElement.tagName"), "BODY");
}

/// A disabled control cannot take focus.
#[test]
fn a_disabled_control_cannot_be_focused() {
    let page = page();
    page.eval(
        "const text = document.getElementById('text');\
         text.disabled = true;\
         text.focus();",
    )
    .unwrap();

    assert_eq!(eval_str(&page, "document.activeElement.tagName"), "BODY");
}

/// Each form interface brand-checks its receiver: `value` on an `<input>` and
/// on a `<select>` are different members, and neither belongs to a `<div>`.
#[test]
fn form_members_brand_check_their_receiver() {
    let page = page();
    assert!(eval_bool(
        &page,
        "(() => { try {\
             HTMLInputElement.prototype.__lookupGetter__('checked')\
                 .call(document.createElement('div'));\
             return false;\
         } catch (e) { return e instanceof TypeError; } })()"
    ));
    assert!(eval_bool(
        &page,
        "document.getElementById('cb') instanceof HTMLInputElement"
    ));
    assert!(eval_bool(
        &page,
        "document.getElementById('sel') instanceof HTMLSelectElement"
    ));
}

/// The regression this whole activation reordering exists for.
///
/// HTML runs the legacy-pre-activation behavior *before* the `click` event
/// propagates, so a `click` listener observes the box already toggled. Firing
/// the event first and toggling afterwards is invisible to a naive test — the
/// box still ends up checked — but it silently breaks React: React synthesises
/// `onChange` for a checkbox from the native `click`, and decides whether
/// anything changed by comparing `node.checked` against the value it recorded at
/// mount. Reading the *pre-toggle* value made that comparison equal, so React's
/// `onChange` never fired for any checkbox or radio.
#[test]
fn a_click_listener_observes_the_already_toggled_checkedness() {
    let page = page();

    let seen = eval_str(
        &page,
        "(() => {
            const cb = document.getElementById('cb');
            let seen = 'no-listener-call';
            cb.addEventListener('click', () => { seen = String(cb.checked); });
            cb.click();
            return seen + '|' + cb.checked;
        })()",
    );

    assert_eq!(
        seen, "true|true",
        "the click listener must see the new checkedness"
    );
}

/// The same for a radio, where undoing means handing the check back to the
/// sibling that held it — not just clearing the clicked one.
#[test]
fn cancelling_a_radio_click_restores_the_previous_member() {
    let page = page();

    let result = eval_str(
        &page,
        "(() => {
            const r1 = document.getElementById('r1'), r2 = document.getElementById('r2');
            let during = '';
            r2.addEventListener('click', (e) => {
                during = r1.checked + '/' + r2.checked;   // already switched
                e.preventDefault();
            });
            r2.click();
            return during + '|' + r1.checked + '/' + r2.checked;
        })()",
    );

    assert_eq!(result, "false/true|true/false");
}

/// A cancelled click must not leave `:checked` styling behind either — the
/// restore goes through the same invalidation path as the toggle.
#[test]
fn a_cancelled_click_leaves_the_checked_rule_unmatched() {
    let page = page();
    let color = "getComputedStyle(document.getElementById('cblabel')).color";

    page.eval(
        "document.getElementById('cb').addEventListener('click', (e) => e.preventDefault());
         document.getElementById('cb').click();",
    )
    .unwrap();

    assert_eq!(eval_str(&page, color), "rgb(0, 0, 0)");
}
