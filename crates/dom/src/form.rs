//! HTML form controls (HTML §4.10) and the element-state pseudo-classes.
//!
//! A form control keeps state that its content attributes do not describe. The
//! `value` content attribute of an `<input>` is only its *default* value: once
//! script (or a user) writes `input.value`, the element's **dirty value flag**
//! is set and the IDL attribute stops tracking the content attribute. The same
//! split exists for `checked`/`defaultChecked` on `<input>` and
//! `selected`/`defaultSelected` on `<option>`.
//!
//! [`FormState`] holds exactly that extra state, and models each dirty flag as
//! an `Option`: `None` means "not dirty, fall back to the content attribute".
//! It is boxed behind an `Option` on [`ElementData`], so the elements that
//! cannot have it (the overwhelming majority) pay one null pointer.
//!
//! The second half of this module is [`DomTree::update_element_state`], which
//! derives stylo's [`ElementState`] bits — `:checked`, `:disabled`, `:enabled`,
//! `:required`, `:read-only`, `:default`, `:indeterminate`, `:focus` — from the
//! attributes plus that state. Stylo already reads `element_state` (it is the
//! return of `TElement::state`) and `build_snapshot` already records it, so the
//! invalidation machinery works the moment the bits are populated: every mutator
//! here snapshots the element first, then re-derives, then hints a restyle.

use std::rc::Rc;

use html5ever::{local_name, ns};
use oxidepage_base::NodeId;
use style_dom::ElementState;

use crate::node::{ElementData, attr_name};
use crate::tree::DomTree;

/// State a form control keeps beyond its content attributes.
///
/// Each `Option` field *is* the corresponding spec dirty flag: `None` = unset,
/// so the IDL attribute mirrors the content attribute.
#[derive(Debug, Clone, Default)]
pub struct FormState {
    /// The "value" IDL attribute once the **dirty value flag** is set.
    pub(crate) value: Option<String>,
    /// `<input>` checkedness / `<option>` selectedness once the **dirty
    /// checkedness (selectedness) flag** is set.
    pub(crate) checkedness: Option<bool>,
    /// `input.indeterminate`. Purely an IDL/`:indeterminate` concern — it has
    /// no content attribute and no dirty flag.
    pub(crate) indeterminate: bool,
    /// The text entry cursor, as UTF-16 offsets into the value — the units
    /// `selectionStart`/`selectionEnd` are defined in, and the units script
    /// will compare against `value.length`. Equal start and end is a collapsed
    /// caret, which is the overwhelmingly common case.
    pub(crate) selection_start: usize,
    pub(crate) selection_end: usize,
    /// `"forward"`, `"backward"` or `"none"`.
    pub(crate) selection_direction: SelectionDirection,
    /// The value at the moment the control took focus, kept so that blur can
    /// decide whether to fire `change`. `None` when the control is not focused.
    ///
    /// A text control fires `input` on every mutation but `change` **only on
    /// blur, and only if the value actually differs from what it was when
    /// focus arrived**. Recomputing that from the current value is impossible;
    /// it has to be snapshotted at focus time.
    pub(crate) value_at_focus: Option<String>,
    /// The **selected files** of an `<input type=file>` (ADR-0032 D11).
    ///
    /// Plain data, and a `dom` type on purpose: `dom` cannot see `bindings`,
    /// and files only ever enter from the *embedder* —
    /// `DOM.setFileInputFiles` and the file chooser. There is no
    /// `DataTransfer`, so page script can never write this, which is what
    /// makes `input.files` read-only in practice. `bindings` wraps each entry
    /// into a `File` on read.
    pub(crate) files: Vec<SelectedFile>,
}

/// One file an embedder selected into an `<input type=file>`.
#[derive(Debug, Clone)]
pub struct SelectedFile {
    pub name: String,
    /// `Rc`, not `Vec`: `input.files` mints a fresh `FileList` on **every**
    /// property read (there is no `[SameObject]` cache, because an embedder can
    /// replace the selection at any time), and building the entry list for a
    /// form post reads them again. With an owned `Vec` a page that touched
    /// `input.files[0]` twice copied a 200 MB upload twice.
    pub bytes: Rc<Vec<u8>>,
    /// The MIME type, guessed from the extension when the embedder gave none.
    pub content_type: String,
    /// Unix milliseconds, for `File.lastModified`.
    pub last_modified: i64,
}

/// `HTMLInputElement.selectionDirection`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionDirection {
    #[default]
    None,
    Forward,
    Backward,
}

impl SelectionDirection {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "forward" => Self::Forward,
            "backward" => Self::Backward,
            _ => Self::None,
        }
    }
}

/// What a click's legacy-pre-activation behavior changed, so that a cancelled
/// click can put it back.
///
/// Produced by [`DomTree::legacy_pre_activation`] and consumed by
/// [`DomTree::legacy_canceled_activation`]. The fields describe the state
/// *before* the click, not after.
#[derive(Debug, Clone, Copy)]
pub struct ClickActivation {
    node: NodeId,
    was_checked: bool,
    was_indeterminate: bool,
    /// The radio in the group that held the check beforehand, if any.
    checked_before: Option<NodeId>,
}

/// The `type` keywords an `<input>` recognises. Anything else (including a
/// missing attribute) is `text` — HTML's "invalid value default".
const INPUT_TYPES: &[&str] = &[
    "hidden",
    "text",
    "search",
    "tel",
    "url",
    "email",
    "password",
    "date",
    "month",
    "week",
    "time",
    "datetime-local",
    "number",
    "range",
    "color",
    "checkbox",
    "radio",
    "file",
    "submit",
    "image",
    "reset",
    "button",
];

/// The canonical `type` keyword of an `<input>`, ASCII-lowercased, defaulting
/// to `"text"`. Also what `input.type` returns.
#[must_use]
pub fn input_type(el: &ElementData) -> &'static str {
    let Some(raw) = el.attr(&attr_name(local_name!("type"))) else {
        return "text";
    };
    let lower = raw.to_ascii_lowercase();
    INPUT_TYPES
        .iter()
        .copied()
        .find(|&t| t == lower)
        .unwrap_or("text")
}

/// An `<input>` whose checkedness — not its value — is what it contributes:
/// the two types for which `:checked` and `input.checked` are meaningful.
fn is_checkable_input(el: &ElementData) -> bool {
    el.local_name() == &local_name!("input") && matches!(input_type(el), "checkbox" | "radio")
}

/// Types that ignore the `value` content attribute for `:enabled`/dirty-value
/// purposes because they are buttons, not value carriers.
fn is_button_input(el: &ElementData) -> bool {
    matches!(input_type(el), "submit" | "image" | "reset" | "button")
}

/// The elements that can be `:disabled` — HTML's "actually disabled" is only
/// defined for these. (`<fieldset>` and `<optgroup>` are disable*able* too, and
/// `<option>` takes a `disabled` attribute of its own.)
fn is_disableable(el: &ElementData) -> bool {
    if !el.is_html_element() {
        return false;
    }
    matches!(
        el.local_name().as_ref(),
        "button" | "input" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
    )
}

/// Controls that take user input, i.e. can be `:read-only` / `:read-write`.
fn is_text_entry(el: &ElementData) -> bool {
    match el.local_name().as_ref() {
        "textarea" => true,
        "input" => matches!(
            input_type(el),
            "text"
                | "search"
                | "tel"
                | "url"
                | "email"
                | "password"
                | "date"
                | "month"
                | "week"
                | "time"
                | "datetime-local"
                | "number"
        ),
        _ => false,
    }
}

/// Controls that can be `:required` / `:optional`.
fn is_requireable(el: &ElementData) -> bool {
    match el.local_name().as_ref() {
        "select" | "textarea" => true,
        "input" => !is_button_input(el) && !matches!(input_type(el), "hidden" | "range" | "color"),
        _ => false,
    }
}

impl ElementData {
    /// The element's local name. Form code branches on it constantly.
    #[must_use]
    pub fn local_name(&self) -> &html5ever::LocalName {
        &self.name.local
    }

    fn has_attr(&self, local: html5ever::LocalName) -> bool {
        self.attr(&attr_name(local)).is_some()
    }

    fn attr_str(&self, local: html5ever::LocalName) -> Option<String> {
        self.attr(&attr_name(local))
            .map(std::string::ToString::to_string)
    }

    /// Whether this element is an HTML element with the given local name.
    #[must_use]
    pub fn is_html(&self, local: &html5ever::LocalName) -> bool {
        self.name.ns == ns!(html) && self.name.local == *local
    }

    fn form_state(&self) -> Option<&FormState> {
        self.form.as_deref()
    }

    fn form_state_mut(&mut self) -> &mut FormState {
        self.form
            .get_or_insert_with(|| Box::new(FormState::default()))
    }
}

impl DomTree {
    // ---------------------------------------------------------------- values

    /// `input.value` / `textarea.value` / `button.value` / `option.value`.
    ///
    /// The dirty value wins; otherwise the element falls back to its content
    /// attribute (and, for `<textarea>` and `<option>`, to its text content).
    #[must_use]
    pub fn form_value(&self, id: NodeId) -> String {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return String::new();
        };
        if let Some(v) = el.form_state().and_then(|f| f.value.as_ref()) {
            return v.clone();
        }
        match el.local_name().as_ref() {
            // A textarea's default value *is* its child text — there is no
            // `value` content attribute to fall back to.
            "textarea" => self.text_content(id),
            // `<option value>` falls back to the option's text (HTML §4.10.10).
            "option" => el
                .attr_str(local_name!("value"))
                .unwrap_or_else(|| self.option_text(id)),
            _ => el.attr_str(local_name!("value")).unwrap_or_default(),
        }
    }

    /// Sets the dirty value flag and the value. Does **not** touch the content
    /// attribute — that is the whole point of the flag.
    pub fn set_form_value(&mut self, id: NodeId, value: String) {
        let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) else {
            return;
        };
        el.form_state_mut().value = Some(value);
        // `:placeholder-shown` and (later) validity depend on the value.
        self.update_element_state(id);
    }

    /// The files selected into an `<input type=file>`.
    #[must_use]
    pub fn selected_files(&self, id: NodeId) -> &[SelectedFile] {
        self.get(id)
            .and_then(|n| n.as_element())
            .and_then(ElementData::form_state)
            .map_or(&[], |state| state.files.as_slice())
    }

    /// Replaces the files selected into an `<input type=file>`.
    ///
    /// The one mutator, so the single invalidation code path is preserved:
    /// `:valid`/`:invalid` for a `required` file input turn on whether the list
    /// is empty, and `update_element_state` is what tells stylo. A caller that
    /// wrote `FormState::files` directly would leave the selector state stale.
    ///
    /// Non-`<input type=file>` elements are ignored rather than given a file
    /// list they could not have: an embedder command naming the wrong element
    /// is refused above this, and silently storing files on a `<div>` would
    /// make that refusal untestable.
    pub fn set_selected_files(&mut self, id: NodeId, files: Vec<SelectedFile>) -> bool {
        if !self.is_file_input(id) {
            return false;
        }
        let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) else {
            return false;
        };
        el.form_state_mut().files = files;
        self.update_element_state(id);
        true
    }

    /// Whether `id` is an `<input type=file>`.
    #[must_use]
    pub fn is_file_input(&self, id: NodeId) -> bool {
        self.get(id)
            .and_then(|n| n.as_element())
            .is_some_and(|el| el.is_html(&local_name!("input")) && crate::input_type(el) == "file")
    }

    /// Drops any selected files an element is no longer entitled to.
    ///
    /// Called when `type` changes: HTML clears the selected files when an
    /// `<input>` stops being a file input, and keeping them would let
    /// `type="file"` → `type="text"` → `type="file"` resurrect a list the page
    /// was never allowed to build.
    pub(crate) fn clear_files_if_not_a_file_input(&mut self, id: NodeId) {
        if self.is_file_input(id) {
            return;
        }
        if let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut())
            && let Some(state) = el.form.as_deref_mut()
            && !state.files.is_empty()
        {
            state.files.clear();
            self.update_element_state(id);
        }
    }

    /// Whether the element is a text entry control — one that has a caret, a
    /// selection and an editable value. `<input type=checkbox>` has a value but
    /// no text entry; `<textarea>` has both.
    #[must_use]
    pub fn is_text_entry(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if !el.is_html_element() {
            return false;
        }
        match &**el.local_name() {
            "textarea" => true,
            "input" => matches!(
                crate::input_type(el),
                "text" | "search" | "url" | "tel" | "password" | "email" | "number"
            ),
            _ => false,
        }
    }

    /// Whether pressing Enter in this control implicitly submits its form
    /// (HTML's **implicit submission**).
    ///
    /// Narrower than [`Self::is_text_entry`] on purpose: a `<textarea>` is a
    /// text entry control, but Enter inserts a newline there rather than
    /// submitting. HTML scopes implicit submission to the *input* text states.
    #[must_use]
    pub fn allows_implicit_submission(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        el.is_html_element() && &**el.local_name() == "input" && self.is_text_entry(id)
    }

    /// Whether the control refuses edits: `readonly` or disabled.
    #[must_use]
    pub fn is_edit_blocked(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return true;
        };
        el.attr(&crate::node::attr_name("readonly".into()))
            .is_some()
            || self.is_actually_disabled(id)
    }

    /// `maxlength`, when present and valid. Enforced only against *user* edits,
    /// per HTML: setting `value` from script bypasses it.
    #[must_use]
    pub fn max_length(&self, id: NodeId) -> Option<usize> {
        let el = self.get(id).and_then(|n| n.as_element())?;
        el.attr(&crate::node::attr_name("maxlength".into()))?
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|&n| n >= 0)
            .map(|n| n as usize)
    }

    /// The selection as `(start, end, direction)` in UTF-16 offsets, clamped to
    /// the current value — the value can change under a stale selection (script
    /// assigning `value`, or a `maxlength` truncation).
    #[must_use]
    pub fn selection(&self, id: NodeId) -> (usize, usize, SelectionDirection) {
        let len = self.form_value(id).encode_utf16().count();
        let Some(state) = self
            .get(id)
            .and_then(|n| n.as_element())
            .and_then(ElementData::form_state)
        else {
            return (len, len, SelectionDirection::None);
        };
        let start = state.selection_start.min(len);
        let end = state.selection_end.min(len).max(start);
        (start, end, state.selection_direction)
    }

    /// `setSelectionRange()`. `end` is clamped to be at least `start`, which is
    /// what the spec's "if end is less than start then set end to start" says.
    pub fn set_selection(
        &mut self,
        id: NodeId,
        start: usize,
        end: usize,
        direction: SelectionDirection,
    ) {
        let len = self.form_value(id).encode_utf16().count();
        let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) else {
            return;
        };
        let state = el.form_state_mut();
        state.selection_start = start.min(len);
        state.selection_end = end.min(len).max(state.selection_start);
        state.selection_direction = direction;
    }

    /// Places a collapsed caret at the end of the value — what a control gets
    /// when focus arrives and what an insertion leaves behind.
    pub fn collapse_selection_to(&mut self, id: NodeId, offset: usize) {
        self.set_selection(id, offset, offset, SelectionDirection::None);
    }

    /// Snapshots the value as focus arrives, so blur can decide whether the
    /// `change` event is owed. Clearing it (`None`) is what a blur does.
    pub fn set_value_at_focus(&mut self, id: NodeId, value: Option<String>) {
        if let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) {
            el.form_state_mut().value_at_focus = value;
        }
    }

    /// The value this control had when it took focus, if it is focused.
    #[must_use]
    pub fn value_at_focus(&self, id: NodeId) -> Option<String> {
        self.get(id)
            .and_then(|n| n.as_element())
            .and_then(ElementData::form_state)
            .and_then(|f| f.value_at_focus.clone())
    }

    /// The default value: the `value` content attribute (or child text for a
    /// `<textarea>`). Backs `defaultValue`.
    #[must_use]
    pub fn form_default_value(&self, id: NodeId) -> String {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return String::new();
        };
        if el.is_html(&local_name!("textarea")) {
            return self.text_content(id);
        }
        el.attr_str(local_name!("value")).unwrap_or_default()
    }

    /// `option.text`: the child text, with `<script>`/`<style>` descendants
    /// skipped and whitespace stripped-and-collapsed (HTML §4.10.10).
    #[must_use]
    pub fn option_text(&self, id: NodeId) -> String {
        let raw = self.text_content(id);
        raw.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
    }

    // ------------------------------------------------------ click activation

    /// HTML's **legacy-pre-activation behavior** for `input` (DOM §2.9 step 5):
    /// it runs *before* the `click` event propagates, so a `click` listener
    /// already observes the new checkedness. Returns `None` for an element with
    /// no checkedness activation — including an already-checked radio, whose
    /// click changes nothing and therefore fires nothing.
    ///
    /// The caller must pair this with [`DomTree::legacy_canceled_activation`] if
    /// a listener cancels the event: the toggle is speculative until the
    /// dispatch comes back un-cancelled.
    pub fn legacy_pre_activation(&mut self, id: NodeId) -> Option<ClickActivation> {
        let el = self.get(id)?.as_element()?;
        if !el.is_html(&local_name!("input")) {
            return None;
        }
        let kind = input_type(el);
        let was_checked = self.checkedness(id);
        let was_indeterminate = self.indeterminate(id);
        match kind {
            "checkbox" => {
                self.set_indeterminate(id, false);
                self.set_checkedness(id, !was_checked);
                Some(ClickActivation {
                    node: id,
                    was_checked,
                    was_indeterminate,
                    checked_before: None,
                })
            }
            "radio" if !was_checked => {
                // Which radio held the check before, so a cancelled click can hand
                // it back — unchecking `id` alone would leave the group empty, a
                // state the user's click never asked for.
                let checked_before = self
                    .radio_group(id)
                    .into_iter()
                    .find(|&r| self.checkedness(r));
                self.set_checkedness(id, true);
                Some(ClickActivation {
                    node: id,
                    was_checked,
                    was_indeterminate,
                    checked_before,
                })
            }
            _ => None,
        }
    }

    /// HTML's **legacy-canceled-activation behavior**: a listener called
    /// `preventDefault()`, so the speculative toggle is put back.
    pub fn legacy_canceled_activation(&mut self, a: ClickActivation) {
        self.set_indeterminate(a.node, a.was_indeterminate);
        match a.checked_before {
            // Re-checking the group's previous member also unchecks `a.node`,
            // because `set_checkedness` owns the radio-group exclusivity rule.
            Some(prev) if prev != a.node => self.set_checkedness(prev, true),
            _ => self.set_checkedness(a.node, a.was_checked),
        }
    }

    // ----------------------------------------------------------- checkedness

    /// `input.checked` / `option.selected`. The dirty flag wins; otherwise the
    /// `checked` / `selected` content attribute is the default.
    #[must_use]
    pub fn checkedness(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if let Some(c) = el.form_state().and_then(|f| f.checkedness) {
            return c;
        }
        el.has_attr(if el.is_html(&local_name!("option")) {
            local_name!("selected")
        } else {
            local_name!("checked")
        })
    }

    /// The default checkedness: presence of the `checked` / `selected` content
    /// attribute. Backs `defaultChecked` / `defaultSelected`.
    #[must_use]
    pub fn default_checkedness(&self, id: NodeId) -> bool {
        self.get(id).and_then(|n| n.as_element()).is_some_and(|el| {
            el.has_attr(if el.is_html(&local_name!("option")) {
                local_name!("selected")
            } else {
                local_name!("checked")
            })
        })
    }

    /// Sets the dirty checkedness flag and the checkedness, then runs the
    /// **radio group** invariant: checking a radio unchecks every other radio
    /// in its group, so `:checked` can never match two of them at once.
    pub fn set_checkedness(&mut self, id: NodeId, checked: bool) {
        let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) else {
            return;
        };
        el.form_state_mut().checkedness = Some(checked);
        self.update_element_state(id);

        if checked && self.is_radio(id) {
            for other in self.radio_group(id) {
                if other != id {
                    if let Some(el) = self.arena.get_mut(other).and_then(|n| n.as_element_mut()) {
                        el.form_state_mut().checkedness = Some(false);
                    }
                    self.update_element_state(other);
                }
            }
        }
    }

    /// `input.indeterminate`.
    #[must_use]
    pub fn indeterminate(&self, id: NodeId) -> bool {
        self.get(id)
            .and_then(|n| n.as_element())
            .and_then(ElementData::form_state)
            .is_some_and(|f| f.indeterminate)
    }

    pub fn set_indeterminate(&mut self, id: NodeId, value: bool) {
        let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut()) else {
            return;
        };
        el.form_state_mut().indeterminate = value;
        self.update_element_state(id);
    }

    fn is_radio(&self, id: NodeId) -> bool {
        self.get(id)
            .and_then(|n| n.as_element())
            .is_some_and(|el| el.is_html(&local_name!("input")) && input_type(el) == "radio")
    }

    /// The radio button group of `id`: the radios with the same (non-empty)
    /// `name` sharing a form owner. Radios with no name are in no group.
    fn radio_group(&self, id: NodeId) -> Vec<NodeId> {
        let Some(name) = self
            .get(id)
            .and_then(|n| n.as_element())
            .and_then(|el| el.attr_str(local_name!("name")))
            .filter(|n| !n.is_empty())
        else {
            return Vec::new();
        };
        let owner = self.form_owner(id);
        // With no form owner the group is scoped to the containing tree, which
        // for a connected control is the document.
        let root = self.root_of(id);
        self.inclusive_descendants(root)
            .filter(|&other| self.is_radio(other))
            .filter(|&other| self.form_owner(other) == owner)
            .filter(|&other| {
                self.get(other)
                    .and_then(|n| n.as_element())
                    .and_then(|el| el.attr_str(local_name!("name")))
                    .is_some_and(|n| n == name)
            })
            .collect()
    }

    /// The root of the tree `id` is in (the document, a shadow root, or a
    /// detached subtree's topmost node).
    fn root_of(&self, id: NodeId) -> NodeId {
        self.inclusive_ancestors(id).last().unwrap_or(id)
    }

    // ------------------------------------------------------------- ownership

    /// The control's **form owner**: the form named by its `form` content
    /// attribute if that resolves to a `<form>`, else its nearest `<form>`
    /// ancestor.
    #[must_use]
    pub fn form_owner(&self, id: NodeId) -> Option<NodeId> {
        let el = self.get(id)?.as_element()?;
        if let Some(form_id) = el.attr_str(local_name!("form")) {
            // An explicit `form=""` attribute detaches the control from any
            // ancestor form, even when it names nothing.
            return self
                .element_by_id(&form_id)
                .filter(|&f| self.is_html_element(f, &local_name!("form")));
        }
        self.ancestors(id)
            .find(|&a| self.is_html_element(a, &local_name!("form")))
    }

    fn is_html_element(&self, id: NodeId, local: &html5ever::LocalName) -> bool {
        self.get(id)
            .and_then(|n| n.as_element())
            .is_some_and(|el| el.is_html(local))
    }

    /// A **listed** form-associated element (HTML's "listed" category), which is
    /// what `form.elements` and `fieldset.elements` enumerate. `<input
    /// type=image>` is deliberately excluded, as HTML excludes it.
    fn is_listed_control(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if !el.is_html_element() {
            return false;
        }
        let listed = matches!(
            el.local_name().as_ref(),
            "button" | "fieldset" | "input" | "object" | "output" | "select" | "textarea"
        );
        listed && !(el.is_html(&local_name!("input")) && input_type(el) == "image")
    }

    /// The form's listed controls, in tree order — `form.elements`.
    ///
    /// The walk starts at the tree root, not at the form: a `form` content
    /// attribute can associate a control that lives nowhere near its form.
    #[must_use]
    pub fn form_controls(&self, form: NodeId) -> Vec<NodeId> {
        let root = self.root_of(form);
        self.inclusive_descendants(root)
            .filter(|&id| self.is_listed_control(id) && self.form_owner(id) == Some(form))
            .collect()
    }

    /// A fieldset's listed *descendants* — `fieldset.elements`. Unlike
    /// `form.elements` this is scoped by containment, not by form ownership.
    #[must_use]
    pub fn fieldset_controls(&self, fieldset: NodeId) -> Vec<NodeId> {
        self.inclusive_descendants(fieldset)
            .filter(|&id| id != fieldset && self.is_listed_control(id))
            .collect()
    }

    /// A `<select>`'s options in tree order — `select.options`. Options nested
    /// in `<optgroup>` count.
    #[must_use]
    pub fn select_options(&self, select: NodeId) -> Vec<NodeId> {
        self.inclusive_descendants(select)
            .filter(|&id| self.is_html_element(id, &local_name!("option")))
            .collect()
    }

    /// The `<label>` elements labelling this control — `input.labels`: labels
    /// whose `for` attribute names it, plus ancestor labels that contain it.
    #[must_use]
    pub fn labels_for(&self, id: NodeId) -> Vec<NodeId> {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return Vec::new();
        };
        let own_id = el.attr_str(local_name!("id"));
        let root = self.root_of(id);
        self.inclusive_descendants(root)
            .filter(|&l| self.is_html_element(l, &local_name!("label")))
            .filter(|&l| {
                let Some(label) = self.get(l).and_then(|n| n.as_element()) else {
                    return false;
                };
                match label.attr_str(local_name!("for")) {
                    // `for` wins outright: a label with `for` labels *that*
                    // element, even if it contains a different control.
                    Some(target) => own_id.as_deref() == Some(target.as_str()),
                    None => self.ancestors(id).any(|a| a == l),
                }
            })
            .collect()
    }

    /// The control a `<label>` labels — `label.control`: the element named by
    /// its `for` attribute (which must be labelable), else its first labelable
    /// descendant.
    #[must_use]
    pub fn label_control(&self, label: NodeId) -> Option<NodeId> {
        let el = self.get(label)?.as_element()?;
        if let Some(target) = el.attr_str(local_name!("for")) {
            return self
                .element_by_id(&target)
                .filter(|&t| self.is_labelable(t));
        }
        self.inclusive_descendants(label)
            .find(|&d| d != label && self.is_labelable(d))
    }

    /// HTML's "labelable" category.
    fn is_labelable(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if !el.is_html_element() {
            return false;
        }
        match el.local_name().as_ref() {
            "button" | "meter" | "output" | "progress" | "select" | "textarea" => true,
            // A hidden input is the one input that is not labelable.
            "input" => input_type(el) != "hidden",
            _ => false,
        }
    }

    /// `select.selectedIndex`: the index of the first selected option, or -1.
    #[must_use]
    pub fn select_selected_index(&self, select: NodeId) -> i32 {
        self.select_options(select)
            .iter()
            .position(|&o| self.checkedness(o))
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1)
    }

    /// `select.value`: the value of the first selected option, else `""`.
    #[must_use]
    pub fn select_value(&self, select: NodeId) -> String {
        self.select_options(select)
            .into_iter()
            .find(|&o| self.checkedness(o))
            .map(|o| self.form_value(o))
            .unwrap_or_default()
    }

    /// Selects the first option whose value equals `value`; if none matches,
    /// deselects everything (`selectedIndex` becomes -1), as browsers do.
    pub fn set_select_value(&mut self, select: NodeId, value: &str) {
        for option in self.select_options(select) {
            let matches = self.form_value(option) == value;
            self.set_option_selectedness(option, matches);
        }
    }

    /// Selects the option at `index`, deselecting the others. A negative or
    /// out-of-range index deselects everything.
    pub fn set_select_selected_index(&mut self, select: NodeId, index: i32) {
        for (i, option) in self.select_options(select).into_iter().enumerate() {
            let wanted = i32::try_from(i).is_ok_and(|i| i == index);
            self.set_option_selectedness(option, wanted);
        }
    }

    /// Sets an option's selectedness *without* the radio-group logic (that is
    /// for inputs) but with the single-select invariant applied by the caller.
    pub fn set_option_selectedness(&mut self, option: NodeId, selected: bool) {
        let Some(el) = self.arena.get_mut(option).and_then(|n| n.as_element_mut()) else {
            return;
        };
        el.form_state_mut().checkedness = Some(selected);
        self.update_element_state(option);
    }

    /// The `<select>` an `<option>` belongs to, if any (it may sit inside an
    /// `<optgroup>`, so this is an ancestor walk, not a parent check).
    #[must_use]
    pub fn owner_select(&self, option: NodeId) -> Option<NodeId> {
        self.ancestors(option)
            .find(|&a| self.is_html_element(a, &local_name!("select")))
    }

    /// `option.selected = true` from script: in a single-selection `<select>`
    /// this must deselect the siblings.
    pub fn set_option_selected(&mut self, option: NodeId, selected: bool) {
        self.set_option_selectedness(option, selected);
        if !selected {
            return;
        }
        let Some(select) = self.owner_select(option) else {
            return;
        };
        let multiple = self
            .get(select)
            .and_then(|n| n.as_element())
            .is_some_and(|el| el.has_attr(local_name!("multiple")));
        if multiple {
            return;
        }
        for other in self.select_options(select) {
            if other != option {
                self.set_option_selectedness(other, false);
            }
        }
    }

    /// HTML's **"ask for a reset"** for a single-selection `<select>`:
    ///
    /// 1. if two or more options are selected, all but the **last** are
    ///    deselected;
    /// 2. if none is, the first non-disabled option is selected.
    ///
    /// Both clauses matter, and the first is not optional bookkeeping: options
    /// arrive one at a time, so parsing `<select><option>x<option selected>y`
    /// auto-selects `x` under clause 2 the moment it is the only option, and
    /// `y` then arrives already selected. Without clause 2's counterpart, the
    /// select would end the parse with *both* selected and `:checked` would
    /// match twice.
    ///
    /// Runs after any change to the option list (parse, insert, remove) or to
    /// an option's selectedness attribute.
    pub(crate) fn ask_for_a_select_reset(&mut self, select: NodeId) {
        if !self.is_html_element(select, &local_name!("select")) {
            return;
        }
        // A `multiple` select, or one with a display size > 1, allows any
        // number of selected options — including none.
        let unconstrained = self
            .get(select)
            .and_then(|n| n.as_element())
            .is_some_and(|el| {
                el.has_attr(local_name!("multiple"))
                    || el
                        .attr_str(local_name!("size"))
                        .and_then(|s| s.trim().parse::<u32>().ok())
                        .is_some_and(|size| size > 1)
            });
        if unconstrained {
            return;
        }
        let options = self.select_options(select);
        let selected: Vec<NodeId> = options
            .iter()
            .copied()
            .filter(|&o| self.checkedness(o))
            .collect();
        match selected.len() {
            0 => {
                if let Some(&first) = options
                    .iter()
                    .find(|&&o| !self.is_actually_disabled(o))
                    .or(options.first())
                {
                    self.set_option_selectedness(first, true);
                }
            }
            1 => {}
            _ => {
                // Keep the last selected option in tree order; drop the rest.
                for &o in &selected[..selected.len() - 1] {
                    self.set_option_selectedness(o, false);
                }
            }
        }
    }

    /// Re-runs the select reset for the `<select>` ancestor of a node whose
    /// insertion or removal may have changed the option list.
    pub(crate) fn note_option_list_changed(&mut self, node: NodeId) {
        let select = self
            .inclusive_ancestors(node)
            .find(|&a| self.is_html_element(a, &local_name!("select")));
        if let Some(select) = select {
            self.ask_for_a_select_reset(select);
        }
    }

    /// HTML "reset the form owner" (`form.reset()`): drop every control's dirty
    /// flags, so its IDL attributes track the content attributes once more —
    /// which is exactly what clearing [`FormState`] means. The `<select>`s then
    /// re-run their reset, since dropping the dirty selectedness of every option
    /// can leave a single-selection select with nothing selected.
    pub fn reset_form(&mut self, form: NodeId) {
        // The options of an owned `<select>` are not themselves listed controls,
        // so reset each control's whole subtree.
        let controls = self.form_controls(form);
        for control in &controls {
            for id in self.inclusive_descendants(*control).collect::<Vec<_>>() {
                if let Some(el) = self.arena.get_mut(id).and_then(|n| n.as_element_mut())
                    && el.form.take().is_some()
                {
                    self.update_element_state(id);
                }
            }
        }
        for control in controls {
            if self.is_html_element(control, &local_name!("select")) {
                self.ask_for_a_select_reset(control);
            }
        }
    }

    // ------------------------------------------------------------- disabled

    /// HTML's **"actually disabled"**: the control's own `disabled` attribute,
    /// or a `<fieldset disabled>` ancestor — unless the control sits inside
    /// that fieldset's *first* `<legend>`, which stays interactive so the user
    /// can re-enable the group.
    #[must_use]
    pub fn is_actually_disabled(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if !is_disableable(el) {
            return false;
        }
        if el.has_attr(local_name!("disabled")) {
            return true;
        }
        // `<option>` also inherits from a disabled `<optgroup>`.
        if el.is_html(&local_name!("option"))
            && let Some(parent) = self.get(id).and_then(|n| n.parent())
            && self.is_html_element(parent, &local_name!("optgroup"))
            && self
                .get(parent)
                .and_then(|n| n.as_element())
                .is_some_and(|g| g.has_attr(local_name!("disabled")))
        {
            return true;
        }
        // A fieldset is not disabled *by* an ancestor fieldset's attribute for
        // the purposes of `:disabled`… but its descendants are, so walk anyway.
        let mut child = id;
        for ancestor in self.ancestors(id) {
            if self.is_html_element(ancestor, &local_name!("fieldset"))
                && self
                    .get(ancestor)
                    .and_then(|n| n.as_element())
                    .is_some_and(|f| f.has_attr(local_name!("disabled")))
                && !self.is_in_first_legend_of(child, ancestor)
            {
                return true;
            }
            child = ancestor;
        }
        false
    }

    /// Whether `child` (a direct child of `fieldset`, on the ancestor walk) is
    /// that fieldset's first `<legend>`.
    fn is_in_first_legend_of(&self, child: NodeId, fieldset: NodeId) -> bool {
        self.children(fieldset)
            .find(|&c| self.is_html_element(c, &local_name!("legend")))
            .is_some_and(|first_legend| first_legend == child)
    }

    // -------------------------------------------------------------- focus

    /// The focused element. Only a *connected* element can hold focus, so this
    /// can never hand back a detached (or freed) node — see
    /// [`clear_focus_if_disconnected`](Self::clear_focus_if_disconnected).
    #[must_use]
    pub fn focused(&self) -> Option<NodeId> {
        self.focused
            .filter(|&id| self.get(id).is_some_and(|n| n.is_connected()))
    }

    /// Moves focus, returning `(blurred, focused)` so the caller can fire the
    /// events. Re-deriving the state of both elements (and their ancestors, for
    /// `:focus-within`) is done here so `:focus` can never go stale.
    ///
    /// Focusing a disconnected element is a no-op, as in a browser.
    pub fn set_focused(&mut self, id: Option<NodeId>) -> (Option<NodeId>, Option<NodeId>) {
        let old = self.focused();
        let new = id.filter(|&id| self.get(id).is_some_and(|n| n.is_connected()));
        if old == new {
            return (None, None);
        }
        self.focused = new;
        // `:focus-within` matches every ancestor, so the whole old and new
        // chains change state, not just the two elements.
        if let Some(old) = old {
            for a in self.inclusive_ancestors(old).collect::<Vec<_>>() {
                self.update_element_state(a);
            }
        }
        if let Some(new) = new {
            for a in self.inclusive_ancestors(new).collect::<Vec<_>>() {
                self.update_element_state(a);
            }
        }
        (old, new)
    }

    /// Whether the element can take focus from a click or the Tab sequence.
    ///
    /// The set is deliberately the one this engine actually implements: form
    /// controls that are not `actually_disabled`, hyperlinks with an `href`,
    /// `<summary>`, and anything carrying a `tabindex` attribute. An element
    /// with `tabindex="-1"` *is* focusable (script and clicks can focus it) but
    /// is not in the sequential order — see [`DomTree::tab_index`].
    #[must_use]
    pub fn is_focusable(&self, id: NodeId) -> bool {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return false;
        };
        if !self.get(id).is_some_and(|n| n.is_connected()) {
            return false;
        }
        if el
            .attr(&crate::node::attr_name("tabindex".into()))
            .is_some()
        {
            return !self.is_actually_disabled(id);
        }
        if !el.is_html_element() {
            return false;
        }
        match &**el.local_name() {
            "input" => {
                // A hidden input has no box and cannot be focused; every other
                // type can, unless disabled.
                crate::input_type(el) != "hidden" && !self.is_actually_disabled(id)
            }
            "button" | "select" | "textarea" => !self.is_actually_disabled(id),
            "a" | "area" => el.attr(&crate::node::attr_name("href".into())).is_some(),
            "summary" => true,
            _ => false,
        }
    }

    /// The element's `tabindex` for sequential navigation: the parsed attribute,
    /// or the default for a natively focusable element (0). `None` means the
    /// element is not reachable by Tab at all.
    #[must_use]
    pub fn tab_index(&self, id: NodeId) -> Option<i32> {
        let el = self.get(id).and_then(|n| n.as_element())?;
        if let Some(attr) = el.attr(&crate::node::attr_name("tabindex".into())) {
            // A malformed value is ignored, leaving the native default.
            if let Ok(value) = attr.trim().parse::<i32>() {
                return (value >= 0).then_some(value);
            }
        }
        self.is_focusable(id).then_some(0)
    }

    /// The sequential focus order: the elements `Tab` visits, in order.
    ///
    /// HTML's rule, and the ordering real pages depend on: **positive
    /// `tabindex` first, ascending**, ties broken by document order; then
    /// everything with `tabindex="0"` or a native default, in document order.
    /// `tabindex="-1"` is focusable but absent here — that is exactly what the
    /// negative value means.
    #[must_use]
    pub fn sequential_focus_order(&self) -> Vec<NodeId> {
        let Some(root) = self.document_element() else {
            return Vec::new();
        };
        let mut ordered: Vec<(i32, usize, NodeId)> = Vec::new();
        for (position, id) in self.inclusive_descendants(root).enumerate() {
            if let Some(index) = self.tab_index(id) {
                ordered.push((index, position, id));
            }
        }
        // Positive indices sort before the zero/native group, ascending; within
        // a group, document order.
        ordered.sort_by_key(|&(index, position, _)| {
            let group = if index > 0 { 0 } else { 1 };
            (group, if index > 0 { index } else { 0 }, position)
        });
        ordered.into_iter().map(|(_, _, id)| id).collect()
    }

    /// The element the pointer is over (`:hover`), or `None`.
    #[must_use]
    pub fn hovered(&self) -> Option<NodeId> {
        self.hovered
            .filter(|&id| self.get(id).is_some_and(|n| n.is_connected()))
    }

    /// The element being pressed (`:active`), or `None`.
    #[must_use]
    pub fn active(&self) -> Option<NodeId> {
        self.active
            .filter(|&id| self.get(id).is_some_and(|n| n.is_connected()))
    }

    /// Moves the hover target. Returns `(left, entered)` — the old and new
    /// elements — so the caller can fire the `mouseover`/`mouseout` pair.
    ///
    /// Exactly [`DomTree::set_focused`]'s shape, and for the same reason: the
    /// state belongs to whole ancestor chains, so both of them are re-derived.
    pub fn set_hovered(&mut self, id: Option<NodeId>) -> (Option<NodeId>, Option<NodeId>) {
        let old = self.hovered();
        let new = id.filter(|&id| self.get(id).is_some_and(|n| n.is_connected()));
        if old == new {
            return (None, None);
        }
        self.hovered = new;
        self.restyle_state_chains(old, new);
        (old, new)
    }

    /// Sets or clears the `:active` element (the one under a held button).
    pub fn set_active(&mut self, id: Option<NodeId>) {
        let old = self.active();
        let new = id.filter(|&id| self.get(id).is_some_and(|n| n.is_connected()));
        if old == new {
            return;
        }
        self.active = new;
        self.restyle_state_chains(old, new);
    }

    /// Re-derives element state along two inclusive-ancestor chains. Shared by
    /// the hover and active setters, which change a state that ancestors carry.
    fn restyle_state_chains(&mut self, old: Option<NodeId>, new: Option<NodeId>) {
        for root in [old, new].into_iter().flatten() {
            for a in self.inclusive_ancestors(root).collect::<Vec<_>>() {
                self.update_element_state(a);
            }
        }
    }

    /// Drops hover/active if their element left the document. A removed node's
    /// `:hover` must not keep matching, and — worse — must not leave a stale
    /// `NodeId` behind for a later ancestor walk to touch.
    ///
    /// Called from the same place as [`DomTree::clear_focus_if_disconnected`]
    /// and for the same reason.
    pub(crate) fn clear_pointer_state_if_disconnected(&mut self) {
        for slot in [&mut self.hovered, &mut self.active] {
            if let Some(id) = *slot
                && !self.arena.get(id).is_some_and(|n| n.is_connected())
            {
                *slot = None;
            }
        }
    }

    /// Drops focus if the focused element is no longer connected — removing the
    /// focused element must not leave `document.activeElement` naming a detached
    /// node (and, once its wrapper is collected, a freed one). Browsers move
    /// focus back to the body, which is what `focused() == None` then reports.
    ///
    /// Called from `remove_internal` *after* the detach, with the parent the
    /// subtree was removed from: the focused element's own ancestor chain no
    /// longer reaches that parent, so it cannot be used to find the elements
    /// that just lost `:focus-within`.
    ///
    /// The focused element itself needs nothing here — `remove_internal` already
    /// re-derives the removed subtree, and by then `focused()` reports `None`
    /// (it filters on connectedness), so its `:focus` bit is dropped.
    pub(crate) fn clear_focus_if_disconnected(&mut self, old_parent: NodeId) {
        let Some(focused) = self.focused else { return };
        if self.get(focused).is_some_and(|n| n.is_connected()) {
            return;
        }
        self.focused = None;
        for a in self.inclusive_ancestors(old_parent).collect::<Vec<_>>() {
            self.update_element_state(a);
        }
    }

    // ------------------------------------------------------- element state

    /// Re-derives stylo's [`ElementState`] for one element and, if it changed,
    /// snapshots the old state and hints a restyle.
    ///
    /// This is the **only** writer of `stylo.element_state`. It must run after
    /// every mutation that can affect a state bit: the attributes below, the
    /// dirty flags above, focus, and tree moves (a `<fieldset disabled>` can
    /// gain descendants).
    pub(crate) fn update_element_state(&mut self, id: NodeId) {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return;
        };
        let old = el.stylo.element_state;
        let new = self.derive_element_state(id);
        if old == new {
            return;
        }
        // The snapshot must capture the state *before* the write — that is what
        // stylo diffs against to decide which selectors to re-run, and it is how
        // a *sibling* rule like `input:checked + label` gets invalidated.
        self.snapshot_element(id);
        if let Some(el) = self.arena.node_mut(id).as_element_mut() {
            el.stylo.element_state = new;
        }
        // `note_subtree_mutation` is the one invalidation entry point: it bumps
        // `style_version`, sets the dirty bits, and walks the ancestor chain for
        // both the engine's and stylo's traversals. Hinting a restyle *without*
        // it leaves `style_version` still, so the next style flush early-outs
        // and the restyle never runs at all.
        self.note_subtree_mutation(id);
    }

    /// `update_element_state` for a whole subtree. A `disabled` attribute on a
    /// `<fieldset>`, and an insertion into one, both change the state of every
    /// control below.
    pub(crate) fn update_element_state_subtree(&mut self, root: NodeId) {
        for id in self.inclusive_descendants(root).collect::<Vec<_>>() {
            self.update_element_state(id);
        }
    }

    fn derive_element_state(&self, id: NodeId) -> ElementState {
        let Some(el) = self.get(id).and_then(|n| n.as_element()) else {
            return ElementState::empty();
        };
        let mut state = ElementState::empty();

        // One `focused()` read, and the `:focus-within` walk only runs when
        // something is focused at all: this function runs for every element on
        // every insertion, so the common case must stay a couple of compares.
        if let Some(focused) = self.focused() {
            if focused == id {
                state |= ElementState::FOCUS | ElementState::FOCUSRING;
            }
            if self.inclusive_ancestors(focused).any(|a| a == id) {
                state |= ElementState::FOCUS_WITHIN;
            }
        }

        // `:hover` and `:active` are *inherited by ancestors*, unlike `:focus`:
        // hovering a `<span>` inside a `<li>` inside a `<nav>` puts all three in
        // `:hover`. That is the same ancestor walk `:focus-within` does above,
        // and the same reason `set_hovered`/`set_active` re-derive whole chains.
        if let Some(hovered) = self.hovered()
            && self.inclusive_ancestors(hovered).any(|a| a == id)
        {
            state |= ElementState::HOVER;
        }
        if let Some(active) = self.active()
            && self.inclusive_ancestors(active).any(|a| a == id)
        {
            state |= ElementState::ACTIVE;
        }

        if !el.is_html_element() {
            return state;
        }

        // "Actually disabled" walks the ancestor chain, and both the
        // ENABLED/DISABLED pair and the READONLY/READWRITE pair below need the
        // answer — compute it once.
        let disabled = is_disableable(el) && self.is_actually_disabled(id);

        if is_disableable(el) {
            if disabled {
                state |= ElementState::DISABLED;
            } else {
                state |= ElementState::ENABLED;
            }
        }

        // `:checked` covers both checkable inputs and selected options.
        if is_checkable_input(el) || el.is_html(&local_name!("option")) {
            if self.checkedness(id) {
                state |= ElementState::CHECKED;
            }
            // `:default` is the *initial* checkedness, not the current one.
            if self.default_checkedness(id) {
                state |= ElementState::DEFAULT;
            }
        }

        if is_checkable_input(el) && self.indeterminate(id) {
            state |= ElementState::INDETERMINATE;
        }

        if is_requireable(el) {
            if el.has_attr(local_name!("required")) {
                state |= ElementState::REQUIRED;
            } else {
                state |= ElementState::OPTIONAL_;
            }
        }

        if is_text_entry(el) {
            if el.has_attr(local_name!("readonly")) || disabled {
                state |= ElementState::READONLY;
            } else {
                state |= ElementState::READWRITE;
            }
            // `:placeholder-shown` — a placeholder is shown while the value is
            // empty. (The engine never renders one; the selector still matches,
            // which is what stylesheets key on.)
            if el.has_attr(local_name!("placeholder")) && self.form_value(id).is_empty() {
                state |= ElementState::PLACEHOLDER_SHOWN;
            }
        }

        state
    }

    /// Whether an attribute change on this element can move an element-state
    /// bit. Keeps `set_attribute`/`remove_attribute` from re-deriving state (and
    /// walking a subtree) on every unrelated attribute write.
    pub(crate) fn attr_affects_element_state(local: &html5ever::LocalName) -> bool {
        matches!(
            local.as_ref(),
            "disabled"
                | "checked"
                | "selected"
                | "required"
                | "readonly"
                | "placeholder"
                | "type"
                | "multiple"
                | "value"
                | "form"
                | "name"
        )
    }

    /// The hook `set_attribute`/`remove_attribute` call after an attribute that
    /// [`attr_affects_element_state`](Self::attr_affects_element_state) flags.
    ///
    /// `disabled` on a `<fieldset>` or `<optgroup>` re-states the whole subtree;
    /// everything else re-states just the element.
    pub(crate) fn note_form_attr(&mut self, element: NodeId, local: &html5ever::LocalName) {
        if !Self::attr_affects_element_state(local) {
            return;
        }
        if *local == local_name!("disabled")
            && (self.is_html_element(element, &local_name!("fieldset"))
                || self.is_html_element(element, &local_name!("optgroup")))
        {
            self.update_element_state_subtree(element);
        } else {
            self.update_element_state(element);
        }
        // A `type`/`multiple` change can turn a select into a multi-select, and
        // a `selected` change can leave a single select with nothing selected.
        if matches!(local.as_ref(), "selected" | "multiple" | "disabled") {
            self.note_option_list_changed(element);
        }
    }
}
