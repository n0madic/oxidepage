//! Synchronous execution of script-inserted inline classic scripts.
//!
//! "Prepare the script element" ends, for a classic script with no `src` that
//! was not inserted by the parser, with *immediately execute the script block*
//! — before the DOM operation that connected it returns to its caller. Real
//! loaders depend on it: Cloudflare's Rocket Loader installs a `document.write`
//! shim, inserts the script, and restores the native `write` once the insertion
//! call has returned. Running the script one task later would write into the
//! restored native `write` instead of the shim.
//!
//! External and module scripts stay asynchronous and are left queued for the
//! event loop ([`crate::state::PageState`]'s owner drains them).

use oxidepage_dom::NodeId;

use crate::cx::BindCx;

/// The classic script MIME types from the HTML "prepare the script element"
/// steps. `None` (no `type` attribute) and the empty string are classic too.
#[must_use]
pub fn is_classic_script_type(script_type: Option<&str>) -> bool {
    matches!(
        script_type,
        None | Some(
            "" | "application/ecmascript"
                | "application/javascript"
                | "application/x-ecmascript"
                | "application/x-javascript"
                | "text/ecmascript"
                | "text/javascript"
                | "text/javascript1.0"
                | "text/javascript1.1"
                | "text/javascript1.2"
                | "text/javascript1.3"
                | "text/javascript1.4"
                | "text/javascript1.5"
                | "text/jscript"
                | "text/livescript"
                | "text/x-ecmascript"
                | "text/x-javascript"
        )
    )
}

/// The source of the next queued script that must execute synchronously, and
/// the element it belongs to. Claims the element before returning it, so the
/// event loop's own preparation pass skips it.
fn claim_next_inline_script(cx: &BindCx<'_>) -> Option<(NodeId, String)> {
    let candidate = {
        let dom = cx.state.dom.borrow();
        dom.script_updates().iter().copied().find(|&node| {
            let Some(dom_node) = dom.get(node) else {
                return false;
            };
            let Some(el) = dom_node.as_element() else {
                return false;
            };
            if !dom_node.is_connected()
                || !el.is_html_element()
                || &*el.name.local != "script"
                || dom.script_already_started(node)
            {
                return false;
            }
            let attr = |name: &str| {
                el.attrs()
                    .iter()
                    .find(|a| a.name.ns.is_empty() && &*a.name.local == name)
                    .map(|a| a.value.to_string())
            };
            // `src` scripts fetch, `nomodule` classics are skipped, and a
            // non-classic type is not a classic script: all stay with the loop.
            attr("src").is_none()
                && attr("nomodule").is_none()
                && is_classic_script_type(
                    attr("type")
                        .map(|value| value.trim().to_ascii_lowercase())
                        .as_deref(),
                )
                && !dom.text_content(node).is_empty()
        })
    };
    let node = candidate?;
    let mut dom = cx.state.dom.borrow_mut();
    if !dom.mark_script_already_started(node) {
        return None;
    }
    Some((node, dom.text_content(node)))
}

/// Runs every script-inserted inline classic script queued by the DOM
/// mutation that just returned.
///
/// A script executed here may itself connect more scripts; those run from the
/// nested host-call boundary inside it, so this loop normally finds nothing
/// left. No microtask checkpoint runs: the JS stack is not empty.
pub(crate) fn run_pending_inline_scripts(cx: &BindCx<'_>) {
    while let Some((node, source)) = claim_next_inline_script(cx) {
        let url = cx.state.dom.borrow().document_url().to_owned();
        let previous = cx.state.current_script.replace(Some(node));
        let result = cx.scope.eval(&source, &url);
        cx.state.current_script.set(previous);
        if let Err(error) = result {
            cx.state.hooks.report_error(error.to_string());
        }
    }
}
