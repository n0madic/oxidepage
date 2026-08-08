//! Form-control state (HTML §4.10): the dirty value/checkedness flags, the
//! radio-group and `<select>` invariants, "actually disabled", labels, focus,
//! and the stylo `ElementState` bits those all feed.

use oxidepage_dom::node::{attr_name, html_name};
use oxidepage_dom::{DomTree, NodeId, ParseOptions, parse_document};

fn parse(html: &str) -> DomTree {
    parse_document(html, ParseOptions::default()).tree
}

/// The nth element with the given local name, in tree order.
fn nth(tree: &DomTree, local: &str, n: usize) -> NodeId {
    tree.inclusive_descendants(tree.document())
        .filter(|&id| {
            tree.node(id)
                .as_element()
                .is_some_and(|el| &*el.name.local == local)
        })
        .nth(n)
        .unwrap_or_else(|| panic!("no <{local}> #{n} in document"))
}

fn first(tree: &DomTree, local: &str) -> NodeId {
    nth(tree, local, 0)
}

fn by_id(tree: &DomTree, id: &str) -> NodeId {
    tree.element_by_id(tree.document(), id)
        .unwrap_or_else(|| panic!("no #{id} in document"))
}

fn matches(tree: &DomTree, id: NodeId, selector: &str) -> bool {
    let list = oxidepage_dom::parse_selector_list(selector).expect("valid selector");
    tree.element_matches(id, &list)
}

// ---------------------------------------------------------------- dirty value

/// Writing `value` sets the dirty value flag: the IDL attribute stops tracking
/// the content attribute, which keeps reading back as the *default* value.
#[test]
fn setting_value_does_not_touch_the_content_attribute() {
    let mut tree = parse(r#"<input id="i" value="default">"#);
    let input = by_id(&tree, "i");

    assert_eq!(tree.form_value(input), "default");
    assert_eq!(tree.form_default_value(input), "default");

    tree.set_form_value(input, "typed".to_owned());

    assert_eq!(tree.form_value(input), "typed");
    assert_eq!(tree.form_default_value(input), "default");
    assert_eq!(
        tree.node(input)
            .as_element()
            .and_then(|el| el.attr(&attr_name("value".into())))
            .map(ToString::to_string)
            .as_deref(),
        Some("default"),
        "the content attribute must be untouched"
    );
}

/// Once dirty, the value no longer follows the content attribute.
#[test]
fn dirty_value_shadows_later_attribute_writes() {
    let mut tree = parse(r#"<input id="i" value="a">"#);
    let input = by_id(&tree, "i");
    tree.set_form_value(input, "dirty".to_owned());

    tree.set_attribute(input, attr_name("value".into()), "b".into());

    assert_eq!(
        tree.form_value(input),
        "dirty",
        "a dirty value wins over the content attribute"
    );
    assert_eq!(tree.form_default_value(input), "b");
}

/// A `<textarea>` has no `value` attribute: its default value is its child text.
#[test]
fn textarea_default_value_is_its_text() {
    let mut tree = parse("<textarea id=\"t\">hello</textarea>");
    let ta = by_id(&tree, "t");

    assert_eq!(tree.form_value(ta), "hello");
    assert_eq!(tree.form_default_value(ta), "hello");

    tree.set_form_value(ta, "typed".to_owned());
    assert_eq!(tree.form_value(ta), "typed");
    assert_eq!(tree.form_default_value(ta), "hello");
}

/// `<option>` with no `value` attribute falls back to its (collapsed) text.
#[test]
fn option_value_falls_back_to_its_text() {
    let tree = parse("<select><option>  one  two  </option></select>");
    let option = first(&tree, "option");
    assert_eq!(tree.form_value(option), "one two");
    assert_eq!(tree.option_text(option), "one two");
}

// ------------------------------------------------------------- checkedness

#[test]
fn checked_attribute_is_the_default_checkedness() {
    let mut tree = parse(r#"<input id="c" type="checkbox" checked>"#);
    let cb = by_id(&tree, "c");

    assert!(tree.checkedness(cb));
    assert!(tree.default_checkedness(cb));
    assert!(matches(&tree, cb, ":checked"));

    tree.set_checkedness(cb, false);

    assert!(!tree.checkedness(cb));
    assert!(
        tree.default_checkedness(cb),
        "defaultChecked still reflects the attribute"
    );
    assert!(!matches(&tree, cb, ":checked"));
    assert!(
        matches(&tree, cb, ":default"),
        ":default is the initial checkedness, not the current one"
    );
}

/// Checking a radio unchecks the rest of its group, so `:checked` can never
/// match two radios with the same name at once.
#[test]
fn checking_a_radio_unchecks_its_group() {
    let mut tree = parse(
        r#"<form>
             <input id="a" type="radio" name="g" checked>
             <input id="b" type="radio" name="g">
             <input id="c" type="radio" name="other">
           </form>"#,
    );
    let (a, b, c) = (by_id(&tree, "a"), by_id(&tree, "b"), by_id(&tree, "c"));
    tree.set_checkedness(c, true);

    assert!(tree.checkedness(a));

    tree.set_checkedness(b, true);

    assert!(tree.checkedness(b));
    assert!(!tree.checkedness(a), "the same-name radio is unchecked");
    assert!(
        tree.checkedness(c),
        "a radio in a different group is untouched"
    );
}

/// Radios in *different forms* are different groups even with the same name.
#[test]
fn radio_groups_are_scoped_to_their_form() {
    let mut tree = parse(
        r#"<form id="f1"><input id="a" type="radio" name="g" checked></form>
           <form id="f2"><input id="b" type="radio" name="g"></form>"#,
    );
    let (a, b) = (by_id(&tree, "a"), by_id(&tree, "b"));

    tree.set_checkedness(b, true);

    assert!(tree.checkedness(b));
    assert!(
        tree.checkedness(a),
        "a radio in another form's group keeps its checkedness"
    );
}

// ------------------------------------------------------------------ select

/// A single-selection `<select>` with no `selected` option selects its first —
/// browsers report `selectedIndex === 0`, never -1.
#[test]
fn select_with_no_selected_option_selects_the_first() {
    let tree = parse("<select><option value=a>a</option><option value=b>b</option></select>");
    let select = first(&tree, "select");

    assert_eq!(tree.select_selected_index(select), 0);
    assert_eq!(tree.select_value(select), "a");
}

/// Options are inserted one at a time, so the first is auto-selected before the
/// one carrying `selected` even arrives. Only the last selected may survive.
#[test]
fn parsing_a_selected_option_deselects_the_auto_selected_first() {
    let tree =
        parse("<select><option value=a>a</option><option value=b selected>b</option></select>");
    let select = first(&tree, "select");
    let selected: Vec<_> = tree
        .select_options(select)
        .into_iter()
        .filter(|&o| tree.checkedness(o))
        .collect();

    assert_eq!(selected.len(), 1, "exactly one option may be selected");
    assert_eq!(tree.select_value(select), "b");
    assert_eq!(tree.select_selected_index(select), 1);
}

/// A `multiple` select imposes no such invariant.
#[test]
fn multiple_select_keeps_every_selected_option() {
    let tree = parse(
        "<select multiple>\
           <option value=a selected>a</option>\
           <option value=b selected>b</option>\
           <option value=c>c</option>\
         </select>",
    );
    let select = first(&tree, "select");
    let selected = tree
        .select_options(select)
        .into_iter()
        .filter(|&o| tree.checkedness(o))
        .count();

    assert_eq!(selected, 2);
}

/// An empty `multiple` select selects nothing at all.
#[test]
fn multiple_select_may_select_nothing() {
    let tree = parse("<select multiple><option>a</option></select>");
    let select = first(&tree, "select");
    assert_eq!(tree.select_selected_index(select), -1);
    assert_eq!(tree.select_value(select), "");
}

#[test]
fn setting_option_selected_deselects_siblings_in_a_single_select() {
    let mut tree = parse("<select><option value=a>a</option><option value=b>b</option></select>");
    let select = first(&tree, "select");
    let b = nth(&tree, "option", 1);

    tree.set_option_selected(b, true);

    assert_eq!(tree.select_value(select), "b");
    assert!(!tree.checkedness(nth(&tree, "option", 0)));
}

/// Setting a value that no option carries deselects everything, as browsers do.
#[test]
fn setting_an_unknown_select_value_deselects_everything() {
    let mut tree = parse("<select><option value=a>a</option></select>");
    let select = first(&tree, "select");

    tree.set_select_value(select, "nope");

    assert_eq!(tree.select_selected_index(select), -1);
    assert_eq!(tree.select_value(select), "");
}

/// Removing the selected option makes the select fall back to another one.
#[test]
fn removing_the_selected_option_re_runs_the_reset() {
    let mut tree =
        parse("<select><option id=\"a\" value=a>a</option><option value=b>b</option></select>");
    let select = first(&tree, "select");
    let a = by_id(&tree, "a");
    assert_eq!(tree.select_value(select), "a");

    tree.remove_child(select, a).expect("removable");

    assert_eq!(
        tree.select_value(select),
        "b",
        "the remaining option is selected"
    );
}

// ---------------------------------------------------------------- disabled

/// "Actually disabled" inherits from an ancestor `<fieldset disabled>` — but
/// the fieldset's first `<legend>` stays interactive.
#[test]
fn fieldset_disabled_inherits_except_in_the_first_legend() {
    let tree = parse(
        r#"<fieldset disabled>
             <legend><input id="in-legend"></legend>
             <legend><input id="in-second-legend"></legend>
             <input id="outside-legend">
           </fieldset>
           <input id="outside">"#,
    );

    let in_legend = by_id(&tree, "in-legend");
    let in_second = by_id(&tree, "in-second-legend");
    let inside = by_id(&tree, "outside-legend");
    let outside = by_id(&tree, "outside");

    assert!(
        !tree.is_actually_disabled(in_legend),
        "the first legend escapes the disabled fieldset"
    );
    assert!(
        tree.is_actually_disabled(in_second),
        "only the *first* legend escapes"
    );
    assert!(tree.is_actually_disabled(inside));
    assert!(!tree.is_actually_disabled(outside));

    assert!(matches(&tree, inside, ":disabled"));
    assert!(matches(&tree, in_legend, ":enabled"));
    assert!(matches(&tree, outside, ":enabled"));
}

/// A disabled `<optgroup>` disables its options.
#[test]
fn optgroup_disabled_disables_its_options() {
    let tree = parse(r#"<select><optgroup disabled><option id="o">a</option></optgroup></select>"#);
    let option = by_id(&tree, "o");
    assert!(tree.is_actually_disabled(option));
    assert!(matches(&tree, option, ":disabled"));
}

/// Disabling a fieldset after the fact re-states its whole subtree — the
/// attribute change must invalidate the descendants, not just the fieldset.
#[test]
fn disabling_a_fieldset_restates_its_subtree() {
    let mut tree = parse(r#"<fieldset id="f"><input id="i"></fieldset>"#);
    let (fieldset, input) = (by_id(&tree, "f"), by_id(&tree, "i"));
    assert!(matches(&tree, input, ":enabled"));

    tree.set_attribute(fieldset, attr_name("disabled".into()), "".into());
    assert!(matches(&tree, input, ":disabled"));

    tree.remove_attribute(input, &attr_name("nonexistent".into()));
    tree.remove_attribute(fieldset, &attr_name("disabled".into()));
    assert!(matches(&tree, input, ":enabled"));
}

/// Moving a control *into* a disabled fieldset disables it.
#[test]
fn inserting_into_a_disabled_fieldset_disables_the_control() {
    let mut tree = parse(r#"<fieldset id="f" disabled></fieldset><input id="i">"#);
    let (fieldset, input) = (by_id(&tree, "f"), by_id(&tree, "i"));
    assert!(matches(&tree, input, ":enabled"));

    tree.append_child(fieldset, input).expect("appendable");

    assert!(
        matches(&tree, input, ":disabled"),
        "insertion under a disabled fieldset must re-derive the state"
    );
}

// ----------------------------------------------------- required / readonly

#[test]
fn required_and_optional_are_complementary() {
    let tree = parse(r#"<input id="r" required><input id="o"><div id="d"></div>"#);

    assert!(matches(&tree, by_id(&tree, "r"), ":required"));
    assert!(!matches(&tree, by_id(&tree, "r"), ":optional"));
    assert!(matches(&tree, by_id(&tree, "o"), ":optional"));
    assert!(
        !matches(&tree, by_id(&tree, "d"), ":optional"),
        "a <div> is neither: it is not a requireable control"
    );
}

#[test]
fn readonly_and_placeholder_shown_track_the_control() {
    let mut tree = parse(r#"<input id="i" placeholder="hint"><input id="ro" readonly>"#);
    let (input, readonly) = (by_id(&tree, "i"), by_id(&tree, "ro"));

    assert!(matches(&tree, input, ":read-write"));
    assert!(matches(&tree, readonly, ":read-only"));
    assert!(
        matches(&tree, input, ":placeholder-shown"),
        "an empty value shows the placeholder"
    );

    tree.set_form_value(input, "typed".to_owned());
    assert!(
        !matches(&tree, input, ":placeholder-shown"),
        "a non-empty value hides it"
    );
}

// ------------------------------------------------------------------ labels

#[test]
fn labels_come_from_for_and_from_containment() {
    let tree = parse(
        r#"<label id="l1" for="i">by for</label>
           <label id="l2">wrapping <input id="i"></label>
           <label id="l3" for="other">unrelated</label>"#,
    );
    let input = by_id(&tree, "i");
    let labels = tree.labels_for(input);

    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&by_id(&tree, "l1")));
    assert!(labels.contains(&by_id(&tree, "l2")));

    assert_eq!(tree.label_control(by_id(&tree, "l1")), Some(input));
    assert_eq!(tree.label_control(by_id(&tree, "l2")), Some(input));
    assert_eq!(tree.label_control(by_id(&tree, "l3")), None);
}

// ---------------------------------------------------------- form ownership

/// A `form` content attribute associates a control with a form it is not inside.
#[test]
fn form_attribute_associates_a_control_outside_the_form() {
    let tree = parse(
        r#"<form id="f"><input id="inside" name="a"></form>
           <input id="outside" name="b" form="f">
           <input id="unowned" name="c">"#,
    );
    let form = by_id(&tree, "f");

    assert_eq!(tree.form_owner(by_id(&tree, "inside")), Some(form));
    assert_eq!(tree.form_owner(by_id(&tree, "outside")), Some(form));
    assert_eq!(tree.form_owner(by_id(&tree, "unowned")), None);

    let controls = tree.form_controls(form);
    assert_eq!(controls.len(), 2);
    assert!(controls.contains(&by_id(&tree, "outside")));
}

/// `<input type=image>` is excluded from the listed controls.
#[test]
fn form_elements_excludes_image_inputs() {
    let tree = parse(
        r#"<form id="f"><input name="a"><input type="image" name="i"><button></button></form>"#,
    );
    let controls = tree.form_controls(by_id(&tree, "f"));
    assert_eq!(controls.len(), 2, "the image input is not listed");
}

/// `form.reset()` drops every dirty flag, so the controls track their content
/// attributes again — and the selects re-run their reset.
#[test]
fn reset_form_clears_the_dirty_flags() {
    let mut tree = parse(
        r#"<form id="f">
             <input id="t" value="default">
             <input id="c" type="checkbox" checked>
             <select id="s"><option value=a>a</option><option value=b selected>b</option></select>
           </form>"#,
    );
    let form = by_id(&tree, "f");
    let (text, checkbox, select) = (by_id(&tree, "t"), by_id(&tree, "c"), by_id(&tree, "s"));

    tree.set_form_value(text, "typed".to_owned());
    tree.set_checkedness(checkbox, false);
    tree.set_select_value(select, "a");
    assert_eq!(tree.select_value(select), "a");

    tree.reset_form(form);

    assert_eq!(tree.form_value(text), "default");
    assert!(tree.checkedness(checkbox));
    assert_eq!(
        tree.select_value(select),
        "b",
        "the select falls back to its `selected` option"
    );
}

// ------------------------------------------------------------------- focus

#[test]
fn focus_moves_and_updates_the_focus_states() {
    let mut tree = parse(r#"<div id="wrap"><input id="a"></div><input id="b">"#);
    let (wrap, a, b) = (by_id(&tree, "wrap"), by_id(&tree, "a"), by_id(&tree, "b"));

    let (blurred, focused) = tree.set_focused(Some(a));
    assert_eq!((blurred, focused), (None, Some(a)));
    assert_eq!(tree.focused(), Some(a));
    assert!(matches(&tree, a, ":focus"));
    assert!(
        matches(&tree, wrap, ":focus-within"),
        "the ancestor chain gains :focus-within"
    );

    let (blurred, focused) = tree.set_focused(Some(b));
    assert_eq!((blurred, focused), (Some(a), Some(b)));
    assert!(!matches(&tree, a, ":focus"));
    assert!(
        !matches(&tree, wrap, ":focus-within"),
        "the old chain loses it"
    );
    assert!(matches(&tree, b, ":focus"));
}

/// Removing the focused element must not leave `activeElement` naming a
/// detached node — focus falls back to nothing (the body, one layer up).
#[test]
fn removing_the_focused_element_drops_focus() {
    let mut tree = parse(r#"<div id="wrap"><input id="a"></div>"#);
    let (wrap, a) = (by_id(&tree, "wrap"), by_id(&tree, "a"));
    tree.set_focused(Some(a));
    assert_eq!(tree.focused(), Some(a));

    tree.remove_child(wrap, a).expect("removable");

    assert_eq!(tree.focused(), None);
    assert!(!matches(&tree, wrap, ":focus-within"));
}

/// A disconnected element cannot hold focus.
#[test]
fn focusing_a_detached_element_is_a_no_op() {
    let mut tree = parse("<body></body>");
    let detached = tree.create_element(html_name("input".into()), Vec::new());

    let (blurred, focused) = tree.set_focused(Some(detached));

    assert_eq!((blurred, focused), (None, None));
    assert_eq!(tree.focused(), None);
}

// ------------------------------------------------- click activation (DOM §2.9)

/// The pre-activation behavior runs *before* the click event propagates, so a
/// `click` listener already sees the toggle. It also clears `indeterminate`.
#[test]
fn pre_activation_toggles_a_checkbox_and_clears_indeterminate() {
    let mut tree = parse(r#"<body><input type="checkbox"></body>"#);
    let cb = nth(&tree, "input", 0);
    tree.set_indeterminate(cb, true);

    let activation = tree.legacy_pre_activation(cb).expect("checkbox activates");

    assert!(
        tree.checkedness(cb),
        "checkedness must flip before dispatch"
    );
    assert!(
        !tree.indeterminate(cb),
        "a clicked checkbox is never indeterminate"
    );
    // And a cancelled click puts both back.
    tree.legacy_canceled_activation(activation);
    assert!(!tree.checkedness(cb));
    assert!(tree.indeterminate(cb));
}

/// A radio's pre-activation takes the check from its group sibling; cancelling
/// must hand it back, not merely uncheck the clicked one — an empty group is a
/// state the click never asked for.
#[test]
fn cancelled_radio_activation_restores_the_previous_member() {
    let mut tree =
        parse(r#"<body><input type="radio" name="g" checked><input type="radio" name="g"></body>"#);
    let (r1, r2) = (nth(&tree, "input", 0), nth(&tree, "input", 1));

    let activation = tree.legacy_pre_activation(r2).expect("radio activates");
    assert!(
        tree.checkedness(r2),
        "clicked radio is checked before dispatch"
    );
    assert!(!tree.checkedness(r1), "the group stays exclusive");

    tree.legacy_canceled_activation(activation);

    assert!(
        tree.checkedness(r1),
        "the previous member gets the check back"
    );
    assert!(!tree.checkedness(r2));
}

/// Clicking an already-checked radio changes nothing, so it activates nothing —
/// which is what keeps it from firing `input`/`change`.
#[test]
fn an_already_checked_radio_does_not_activate() {
    let mut tree = parse(r#"<body><input type="radio" name="g" checked></body>"#);
    let r = nth(&tree, "input", 0);

    assert!(tree.legacy_pre_activation(r).is_none());
    assert!(tree.checkedness(r));
}
