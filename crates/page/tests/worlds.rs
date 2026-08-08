//! Isolated worlds at the `Page` level (ADR-0033): lifecycle, per-world task
//! sources, and the teardown whose failure mode is a process abort.

use oxidepage_page::{EvaluateOptions, EvaluateOutcome, Page, PageOptions};

fn page() -> Page {
    Page::new(PageOptions::default()).expect("page")
}

fn eval_in(page: &Page, context_id: Option<u64>, source: &str) -> String {
    let result = page
        .evaluate_in(context_id, source, &EvaluateOptions::default())
        .expect("the context exists")
        .expect_done();
    assert!(
        result.exception.is_none(),
        "{source} threw: {:?}",
        result.exception
    );
    result
        .result
        .value_json
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
}

#[test]
fn a_page_starts_with_only_the_main_world() {
    let page = page();
    let worlds = page.worlds();
    assert_eq!(worlds.len(), 1);
    assert!(worlds[0].is_default);
    assert_eq!(worlds[0].name, "");
    assert_eq!(worlds[0].context_id, page.execution_context_id());
}

#[test]
fn creating_a_world_is_idempotent_by_name() {
    let page = page();
    let first = page.create_isolated_world("utility").expect("created");
    let again = page.create_isolated_world("utility").expect("created");
    assert_eq!(first.context_id, again.context_id);
    assert_eq!(page.worlds().len(), 2);

    let other = page.create_isolated_world("other").expect("created");
    assert_ne!(first.context_id, other.context_id);
    assert_eq!(page.worlds().len(), 3);
}

#[test]
fn an_isolated_world_needs_a_name() {
    // The empty name is the main world's, and a driver must not be able to take
    // it over.
    assert!(page().create_isolated_world("").is_err());
}

#[test]
fn worlds_have_separate_globals_over_one_dom() {
    let page = page();
    page.load_html("<!doctype html><body><p id=x>hi</p></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    eval_in(&page, None, "globalThis.__page = 'p'");
    eval_in(&page, Some(utility.context_id), "globalThis.__util = 'u'");

    assert_eq!(
        eval_in(&page, Some(utility.context_id), "typeof globalThis.__page"),
        "undefined"
    );
    assert_eq!(
        eval_in(&page, None, "typeof globalThis.__util"),
        "undefined"
    );

    // …but the document is the same one.
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "document.getElementById('x').textContent"
        ),
        "hi"
    );
}

/// The free leak barrier of one runtime per world (ADR-0033 D1), asserted
/// positively: a wrapper minted in one world fails every brand check in another.
#[test]
fn a_node_wrapper_is_per_world_and_never_crosses() {
    let page = page();
    page.load_html("<!doctype html><body><p id=x>hi</p></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    // Each world's wrapper is stable *within* that world…
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "document.getElementById('x') === document.getElementById('x')"
        ),
        "true"
    );
    // …and each has its own prototypes, so the interfaces are distinct objects.
    assert_eq!(eval_in(&page, None, "typeof Element"), "function");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "typeof Element"),
        "function"
    );
}

/// `customElements` is **absent** in an isolated world, not a throwing stub, so
/// feature detection works (P6, ADR-0033 D8).
#[test]
fn custom_elements_is_absent_in_an_isolated_world() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    assert_eq!(eval_in(&page, None, "typeof customElements"), "object");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "typeof customElements"),
        "undefined"
    );
}

#[test]
fn an_unknown_context_id_is_an_error_not_a_silent_main_world_evaluation() {
    let page = page();
    assert!(
        page.evaluate_in(Some(9_999_999), "1", &EvaluateOptions::default())
            .is_err()
    );
}

#[test]
fn worlds_are_rebuilt_with_fresh_globals_and_ids_on_commit() {
    let page = page();
    let before = page.create_isolated_world("utility").expect("created");
    eval_in(&page, Some(before.context_id), "globalThis.__stale = 1");

    page.load_html("<!doctype html><title>next</title>")
        .expect("load");

    let worlds = page.worlds();
    let after = worlds
        .iter()
        .find(|w| w.name == "utility")
        .expect("the world is rebuilt under the same name");
    assert_ne!(
        before.context_id, after.context_id,
        "a rebuilt world must report a new id"
    );
    // A fresh global, not the old one renumbered — this is why the rebuild is
    // mandatory rather than an optimisation (ADR-0033 D9).
    assert_eq!(
        eval_in(&page, Some(after.context_id), "typeof globalThis.__stale"),
        "undefined"
    );
    // The stale id is now a clean error.
    assert!(
        page.evaluate_in(Some(before.context_id), "1", &EvaluateOptions::default())
            .is_err()
    );
}

/// The main world's realm *survives* a commit, so only its `context_id` marks
/// the document boundary — and every report of it has to come from the live
/// `WorldState`, never from a copy taken when the world was registered. A
/// registry that cached the id answered a navigated page with the dead one.
#[test]
fn the_main_worlds_context_id_is_reported_fresh_after_a_commit() {
    let page = page();
    let before = page.execution_context_id();

    page.load_html("<!doctype html><title>next</title>")
        .expect("load");

    let after = page.execution_context_id();
    assert_ne!(before, after, "a commit must renumber the main world");
    let main = page
        .worlds()
        .into_iter()
        .find(|w| w.is_default)
        .expect("the main world is always registered");
    assert_eq!(
        main.context_id, after,
        "the registry must report the live id, not the one it was built with"
    );
    assert!(
        page.evaluate_in(Some(before), "1", &EvaluateOptions::default())
            .is_err(),
        "the outgoing document's context id must be a clean error"
    );
}

#[test]
fn an_init_script_runs_only_in_its_named_world() {
    let page = page();
    page.create_isolated_world("utility").expect("created");
    page.add_init_script_for("globalThis.__injected = 'yes'", Some("utility"));

    page.load_html("<!doctype html><title>a</title>")
        .expect("load");

    let utility = page
        .worlds()
        .into_iter()
        .find(|w| w.name == "utility")
        .expect("world");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "globalThis.__injected"),
        "yes"
    );
    assert_eq!(
        eval_in(&page, None, "typeof globalThis.__injected"),
        "undefined"
    );
}

#[test]
fn naming_an_unknown_world_in_an_init_script_creates_it() {
    let page = page();
    page.add_init_script_for("globalThis.__made = 1", Some("late"));
    assert!(page.worlds().iter().any(|w| w.name == "late"));
}

#[test]
fn a_binding_is_installed_only_in_its_world_and_survives_a_commit() {
    let page = page();
    page.create_isolated_world("utility").expect("created");
    page.add_binding_in("__probe", Some("utility"))
        .expect("bound");

    let utility = page
        .worlds()
        .into_iter()
        .find(|w| w.name == "utility")
        .expect("world");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "typeof __probe"),
        "function"
    );
    assert_eq!(
        eval_in(&page, None, "typeof globalThis.__probe"),
        "undefined"
    );

    // A commit rebuilds the world; the binding must be re-applied to it, or a
    // driver's `exposeBinding` vanishes on the first navigation.
    page.load_html("<!doctype html><title>a</title>")
        .expect("load");
    let rebuilt = page
        .worlds()
        .into_iter()
        .find(|w| w.name == "utility")
        .expect("world");
    assert_eq!(
        eval_in(&page, Some(rebuilt.context_id), "typeof __probe"),
        "function"
    );
}

#[test]
fn a_binding_call_carries_the_world_it_came_from() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    page.add_binding_in("__probe", None).expect("bound");

    eval_in(&page, Some(utility.context_id), "__probe('from-utility')");
    eval_in(&page, None, "__probe('from-page')");

    let calls = page.drain_binding_calls();
    let by_payload = |p: &str| {
        calls
            .iter()
            .find(|c| c.payload == p)
            .unwrap_or_else(|| panic!("no call with payload {p}: {calls:?}"))
            .context_id
    };
    assert_eq!(by_payload("from-utility"), utility.context_id);
    assert_eq!(by_payload("from-page"), page.execution_context_id());
}

#[test]
fn a_timer_scheduled_in_a_world_fires_in_that_world() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    eval_in(
        &page,
        Some(utility.context_id),
        "globalThis.__ticked = false; setTimeout(() => { globalThis.__ticked = true; }, 1)",
    );
    page.settle(std::time::Duration::from_millis(500));

    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(__ticked)"),
        "true"
    );
    // The main world neither ran it nor gained the global.
    assert_eq!(
        eval_in(&page, None, "typeof globalThis.__ticked"),
        "undefined"
    );
}

#[test]
fn js_heap_used_sums_across_worlds() {
    let page = page();
    let before = page.js_heap_used();
    page.create_isolated_world("utility").expect("created");
    let after = page.js_heap_used();
    assert!(
        after > before,
        "a second runtime must show up in the page total ({before} -> {after})"
    );
}

/// The drop-order regression (ADR-0033 D4).
///
/// A `Persistent` outliving its `Runtime` aborts the process inside
/// `JS_FreeRuntime` on a non-empty `gc_obj_list` — not a test failure, a
/// `SIGABRT`. This drops a page holding live JS in three worlds at once:
/// wrappers, expandos, a pending timer, a pending animation frame, a retained
/// remote handle, and a registered listener.
#[test]
fn dropping_a_page_with_live_worlds_is_clean() {
    let page = page();
    page.load_html("<!doctype html><body><p id=x>hi</p></body>")
        .expect("load");
    let a = page.create_isolated_world("a").expect("created");
    let b = page.create_isolated_world("b").expect("created");

    for context in [None, Some(a.context_id), Some(b.context_id)] {
        eval_in(
            &page,
            context,
            "globalThis.__held = document.getElementById('x');
             globalThis.__held.__expando = { deep: [1, 2, 3] };
             globalThis.__cycle = { self: null }; __cycle.self = __cycle;
             setTimeout(() => {}, 100000);
             requestAnimationFrame(() => {});
             document.addEventListener('click', () => {});
             globalThis.__promise = new Promise(() => {});",
        );
    }
    // A retained remote handle in each world, too: the object store is per
    // world and holds `JsValue`s.
    for context in [None, Some(a.context_id), Some(b.context_id)] {
        let handle = page
            .evaluate_in(context, "({ retained: true })", &EvaluateOptions::default())
            .expect("context")
            .expect_done();
        assert!(handle.result.object_id.is_some());
    }
    // A deferred `awaitPromise` parked in each world (ADR-0034 D1). `Page`
    // holds the promise itself, not a handle to it, so this is the one owner
    // of a `JsValue` that lives outside `WorldState` — and a `Persistent`
    // outliving its `Runtime` aborts the process rather than failing a test.
    for context in [None, Some(a.context_id), Some(b.context_id)] {
        let outcome = page
            .evaluate_in(
                context,
                "new Promise(() => {})",
                &EvaluateOptions {
                    await_promise: true,
                    defer_await: true,
                    ..EvaluateOptions::default()
                },
            )
            .expect("context");
        assert!(
            matches!(outcome, EvaluateOutcome::Deferred(_)),
            "a promise nothing resolves must defer, not answer"
        );
    }
    // History state is page-level and must be serialized, not a live handle.
    eval_in(&page, None, "history.pushState({ a: 1 }, '', '#one')");

    drop(page);
}

/// The same teardown while unwinding: `engine`'s `catch_unwind` drops a `Page`
/// on the panic path, and the ordering must hold there too.
#[test]
fn dropping_a_page_while_unwinding_is_clean() {
    let outcome = std::panic::catch_unwind(|| {
        let page = page();
        let utility = page.create_isolated_world("utility").expect("created");
        eval_in(&page, Some(utility.context_id), "globalThis.__held = {}");
        eval_in(&page, None, "globalThis.__held = {}");
        panic!("unwind with three live worlds");
    });
    assert!(outcome.is_err());
}

#[test]
fn a_world_count_cap_bounds_a_runaway_driver() {
    let page = page();
    let mut refused = false;
    for i in 0..64 {
        if page.create_isolated_world(&format!("w{i}")).is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "creating worlds without limit must be refused");
}

// === Events across worlds (ADR-0033 D6) ===================================
//
// These live here rather than in a `bindings`-only harness because the
// cross-world hop goes through the page's world table: `WorldEnter` is what
// lets a dispatch started in one world reach another world's listeners, and a
// bare `install_world` embedder has no table to hop with.

/// A page with one element and a utility world, ready for dispatch tests.
fn page_with_utility() -> (Page, u64) {
    let page = page();
    page.load_html("<!doctype html><body><button id=b>go</button></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");
    (page, utility.context_id)
}

#[test]
fn a_utility_world_listener_sees_a_main_world_dispatch() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        Some(utility),
        "globalThis.__seen = 0;
         document.getElementById('b').addEventListener('click', () => { __seen++; });",
    );
    eval_in(&page, None, "document.getElementById('b').click()");
    assert_eq!(eval_in(&page, Some(utility), "String(__seen)"), "1");
}

#[test]
fn a_main_world_listener_sees_a_utility_world_dispatch() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        None,
        "globalThis.__seen = 0;
         document.getElementById('b').addEventListener('click', () => { __seen++; });",
    );
    eval_in(&page, Some(utility), "document.getElementById('b').click()");
    assert_eq!(eval_in(&page, None, "String(__seen)"), "1");
}

/// One event, N wrappers over one shared payload: `target` and `currentTarget`
/// resolve through each world's own cache, so identity holds *within* a world.
#[test]
fn target_and_current_target_resolve_per_world() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        Some(utility),
        "globalThis.__ok = 'not run';
         const b = document.getElementById('b');
         b.addEventListener('click', (e) => {
             __ok = (e.target === b && e.currentTarget === b) ? 'yes' : 'no';
         });",
    );
    eval_in(&page, None, "document.getElementById('b').click()");
    assert_eq!(eval_in(&page, Some(utility), "__ok"), "yes");
}

/// The propagation flags live in the one shared `EventData`, so one event has
/// one propagation no matter which world cancels it.
#[test]
fn prevent_default_in_one_world_cancels_for_the_other() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        Some(utility),
        "document.getElementById('b')
            .addEventListener('click', (e) => e.preventDefault());",
    );
    let cancelled = eval_in(
        &page,
        None,
        "String(!document.getElementById('b')
            .dispatchEvent(new MouseEvent('click', { cancelable: true })))",
    );
    assert_eq!(cancelled, "true");
}

#[test]
fn stop_immediate_propagation_stops_both_worlds_on_that_node() {
    let (page, utility) = page_with_utility();
    // Main world first (the ordering rule), and it stops everything.
    eval_in(
        &page,
        None,
        "globalThis.__main = 0;
         const b = document.getElementById('b');
         b.addEventListener('click', (e) => { __main++; e.stopImmediatePropagation(); });",
    );
    eval_in(
        &page,
        Some(utility),
        "globalThis.__util = 0;
         document.getElementById('b')
            .addEventListener('click', () => { __util++; });",
    );
    eval_in(&page, None, "document.getElementById('b').click()");
    assert_eq!(eval_in(&page, None, "String(__main)"), "1");
    assert_eq!(
        eval_in(&page, Some(utility), "String(__util)"),
        "0",
        "stopImmediatePropagation must stop every world on that node"
    );
}

/// **Main world first, then creation order** — a documented divergence, since
/// cross-world order is unspecified. Main-first is what lets a utility-world
/// listener observe the page's own `defaultPrevented`.
#[test]
fn listener_order_is_main_world_first() {
    let (page, utility) = page_with_utility();
    page.add_binding("__order").expect("bound");
    eval_in(
        &page,
        Some(utility),
        "document.getElementById('b').addEventListener('click', () => __order('utility'));",
    );
    eval_in(
        &page,
        None,
        "document.getElementById('b').addEventListener('click', () => __order('main'));",
    );
    eval_in(&page, None, "document.getElementById('b').click()");

    let seen: Vec<String> = page
        .drain_binding_calls()
        .into_iter()
        .map(|c| c.payload)
        .collect();
    assert_eq!(seen, vec!["main", "utility"]);
}

/// `CustomEvent.detail` is a live object of the world that built it, so it
/// reads as `null` elsewhere. The isolation boundary, not a gap (D5).
#[test]
fn custom_event_detail_from_another_world_reads_null() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        Some(utility),
        "globalThis.__detail = 'not run';
         document.getElementById('b')
            .addEventListener('probe', (e) => { __detail = String(e.detail); });",
    );
    eval_in(
        &page,
        None,
        "document.getElementById('b').dispatchEvent(
            new CustomEvent('probe', { detail: { secret: 1 } }));",
    );
    assert_eq!(eval_in(&page, Some(utility), "__detail"), "null");

    // …and it is readable in the world that created it.
    eval_in(
        &page,
        None,
        "globalThis.__mine = 'not run';
         const b = document.getElementById('b');
         b.addEventListener('own', (e) => { __mine = String(e.detail.secret); });
         b.dispatchEvent(new CustomEvent('own', { detail: { secret: 7 } }));",
    );
    assert_eq!(eval_in(&page, None, "__mine"), "7");
}

/// An inline `onclick=` is page script and compiles once, in the main world —
/// a utility world must not get its own copy of the page's handler.
#[test]
fn an_inline_handler_stays_in_the_main_world() {
    let page = page();
    page.load_html(
        "<!doctype html><body><button id=b onclick=\"globalThis.__fired = (globalThis.__fired || 0) + 1\">go</button></body>",
    )
    .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    eval_in(&page, None, "document.getElementById('b').click()");
    assert_eq!(eval_in(&page, None, "String(__fired)"), "1");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "typeof globalThis.__fired"),
        "undefined",
        "the content attribute must not be compiled into a second world"
    );
}

/// A `<script>` inserted from the utility world runs as **page** script.
#[test]
fn a_script_inserted_from_the_utility_world_runs_in_the_main_one() {
    let (page, utility) = page_with_utility();
    eval_in(
        &page,
        Some(utility),
        "const s = document.createElement('script');
         s.textContent = 'globalThis.__ranHere = true';
         document.body.appendChild(s);",
    );
    page.settle(std::time::Duration::from_millis(200));
    assert_eq!(eval_in(&page, None, "String(__ranHere)"), "true");
    assert_eq!(
        eval_in(&page, Some(utility), "typeof globalThis.__ranHere"),
        "undefined"
    );
}

#[test]
fn mutation_observers_in_both_worlds_both_deliver() {
    let (page, utility) = page_with_utility();
    for context in [None, Some(utility)] {
        eval_in(
            &page,
            context,
            "globalThis.__records = 0;
             new MutationObserver((rs) => { __records += rs.length; })
                 .observe(document.body, { childList: true });",
        );
    }
    eval_in(
        &page,
        None,
        "document.body.appendChild(document.createElement('span'))",
    );
    page.settle(std::time::Duration::from_millis(200));

    assert_eq!(eval_in(&page, None, "String(__records)"), "1");
    assert_eq!(eval_in(&page, Some(utility), "String(__records)"), "1");
}

/// `localStorage` is a different object per world over the *same* area.
#[test]
fn local_storage_is_a_distinct_object_per_world_over_one_area() {
    let (page, utility) = page_with_utility();
    eval_in(&page, None, "localStorage.setItem('k', 'v')");
    assert_eq!(
        eval_in(&page, Some(utility), "localStorage.getItem('k')"),
        "v"
    );
}

#[test]
fn animation_frames_fire_in_every_world() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    for context in [None, Some(utility.context_id)] {
        eval_in(
            &page,
            context,
            "globalThis.__frames = 0;
             requestAnimationFrame(() => { __frames++; });",
        );
    }
    page.settle(std::time::Duration::from_millis(500));
    assert_eq!(eval_in(&page, None, "String(__frames)"), "1");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(__frames)"),
        "1",
        "a utility world's rAF must fire: a driver's polling waits are built on it"
    );
}

#[test]
fn a_fetch_started_in_a_world_resolves_in_that_world() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    // `data:` needs no network and is decoded above the scheme gate (ADR-0029),
    // so this stays a loopback-free test.
    eval_in(
        &page,
        Some(utility.context_id),
        "globalThis.__body = 'pending';
         fetch('data:text/plain,hello').then(r => r.text()).then(t => { __body = t; });",
    );
    // Polled rather than settled once: the whole test binary runs in parallel,
    // and a fixed budget is a flake under load.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while eval_in(&page, Some(utility.context_id), "__body") == "pending"
        && std::time::Instant::now() < deadline
    {
        page.settle(std::time::Duration::from_millis(100));
    }
    assert_eq!(eval_in(&page, Some(utility.context_id), "__body"), "hello");
}

/// The free leak barrier, asserted positively: an object minted in one world
/// fails every brand check in another (ADR-0033 D1).
///
/// This is why `JsListener.signal` did **not** need to become `(WorldId, u64)`:
/// a foreign-world `AbortSignal` cannot be mistaken for a local slab key,
/// because `host_payload` cannot even read its payload. The plan's worry — a
/// silently ignored foreign signal leaking a listener — is structurally
/// impossible rather than handled.
#[test]
fn a_foreign_world_object_fails_every_brand_check() {
    let page = page();
    page.load_html("<!doctype html><body><p id=x>hi</p></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    // Hand the main world a reference to a utility-world object by way of a
    // binding payload — the only channel that crosses, and it carries text.
    // Direct value passing is impossible, which is the point; what this asserts
    // is that a *wrapper* cannot be smuggled either.
    let probe = "(() => {
        try {
            // A brand check against a value from another world must throw a
            // TypeError, never silently accept.
            const foreign = globalThis.__foreign;
            if (foreign === undefined) return 'absent';
            document.body.appendChild(foreign);
            return 'accepted';
        } catch (e) {
            return e instanceof TypeError ? 'TypeError' : 'other';
        }
    })()";
    // The utility world's node wrapper is not reachable from the main world at
    // all: there is no channel for it. `absent` is the honest outcome and is
    // itself the isolation.
    eval_in(
        &page,
        Some(utility.context_id),
        "globalThis.__own = document.getElementById('x')",
    );
    assert_eq!(eval_in(&page, None, probe), "absent");

    // A foreign `AbortSignal` is the case the plan called out. Each world has
    // its own `AbortController`, and neither can see the other's.
    eval_in(
        &page,
        Some(utility.context_id),
        "globalThis.__ac = new AbortController()",
    );
    assert_eq!(
        eval_in(&page, None, "typeof globalThis.__ac"),
        "undefined",
        "an AbortController must not be visible across worlds"
    );
    // …and within a world it works normally, so the barrier costs nothing.
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "(() => {
                 let fired = 0;
                 const ac = new AbortController();
                 document.body.addEventListener('probe', () => { fired++; }, { signal: ac.signal });
                 document.body.dispatchEvent(new Event('probe'));
                 ac.abort();
                 document.body.dispatchEvent(new Event('probe'));
                 return String(fired);
             })()"
        ),
        "1"
    );
}

/// The `[SameObject]` singletons behind `navigator` are **per world**.
///
/// They used to be cached on the shared `NavigatorData`, which was wrong twice:
/// the cached value is a `JsValue` of whichever world asked first, so a second
/// world got a foreign handle, and a page-level holder of JS values breaks the
/// teardown invariant (ADR-0033 D3). Each world must mint — and keep — its own.
#[test]
fn navigator_same_object_singletons_are_per_world() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");

    for context in [None, Some(utility.context_id)] {
        // Readable at all: a foreign handle would fail to restore here.
        assert_eq!(
            eval_in(
                &page,
                context,
                "navigator.languages.length > 0 ? 'ok' : 'empty'"
            ),
            "ok"
        );
        // …and stable within the world, which is what `[SameObject]` means.
        for member in ["languages", "plugins", "mimeTypes"] {
            assert_eq!(
                eval_in(
                    &page,
                    context,
                    &format!("String(navigator.{member} === navigator.{member})")
                ),
                "true",
                "navigator.{member} must be the same object within a world"
            );
        }
    }

    // The two worlds' copies are distinct objects — they cannot be otherwise,
    // since no value crosses. Asserted through a property write, the only
    // channel that could reveal accidental sharing.
    eval_in(&page, None, "navigator.plugins.__mark = 'main'");
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "String(navigator.plugins.__mark)"
        ),
        "undefined",
        "a navigator singleton leaked between worlds"
    );
}

/// Re-entering a world that is already on the stack is **refused**, not
/// attempted (ADR-0033 D4).
///
/// A → B → A is ordinary script: page script clicks, a utility-world listener
/// runs, and it dispatches an event the main world listens for. Entering a live
/// `Context` is a `BorrowMutError` inside rquickjs, so the innermost hop is
/// skipped. Regression: `with_cx_in` used to short-circuit the main world
/// straight to `with_cx`, which never armed the latch — so this exact sequence
/// panicked and took the page thread with it.
#[test]
fn re_entering_the_main_world_mid_dispatch_is_refused_not_a_panic() {
    let page = page();
    page.load_html("<!doctype html><body><button id=b>go</button></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    eval_in(
        &page,
        None,
        "globalThis.__ping = 0;
         document.addEventListener('ping', () => { __ping++; });",
    );
    eval_in(
        &page,
        Some(utility.context_id),
        "document.getElementById('b').addEventListener('click', () => {
             document.dispatchEvent(new Event('ping'));
         });",
    );

    // The page survives, which is the whole point.
    assert_eq!(
        eval_in(
            &page,
            None,
            "document.getElementById('b').click(); 'survived'"
        ),
        "survived"
    );
    // …and the re-entrant hop was skipped rather than run.
    assert_eq!(eval_in(&page, None, "String(__ping)"), "0");

    // The latch is released afterwards: the page is still fully usable.
    eval_in(&page, None, "document.dispatchEvent(new Event('ping'))");
    assert_eq!(eval_in(&page, None, "String(__ping)"), "1");
}

/// A world-owned `JsValue` inside a **shared** `EventData` must not outlive its
/// runtime (ADR-0033 D5).
///
/// `wrap_event` files the same `Rc<RefCell<EventData>>` into every world's slab
/// that mints a wrapper, and a slab is not cleared on navigation — so a
/// main-world listener holding the event kept a utility world's `detail` alive
/// past `take_isolated`, and freeing that runtime aborted the process in
/// `JS_FreeRuntime`. Both payload slots that could carry one are covered:
/// `CustomEvent.detail`, and a UI event's `relatedTarget`.
#[test]
fn an_event_payload_does_not_outlive_the_world_that_filled_it() {
    for dispatch in [
        "document.dispatchEvent(new CustomEvent('probe', { detail: { a: 1 } }))",
        "document.dispatchEvent(new MouseEvent('probe', \
             { relatedTarget: document.getElementById('x') }))",
    ] {
        let page = page();
        page.load_html("<!doctype html><body><p id=x>hi</p></body>")
            .expect("load");
        let utility = page.create_isolated_world("utility").expect("created");

        // The main world retains the event object across the commit.
        eval_in(
            &page,
            None,
            "globalThis.__kept = null;
             document.addEventListener('probe', (e) => { globalThis.__kept = e; });",
        );
        eval_in(&page, Some(utility.context_id), dispatch);

        // The commit tears the utility world down and frees its runtime.
        page.load_html("<!doctype html><title>next</title>")
            .expect("load");
        page.collect_garbage();
        // The retained event is still readable, and its foreign payload is
        // simply gone — which is what every other world already saw.
        drop(page);
    }
}

/// A commit must forget every handle it invalidated, in **both** directions.
///
/// The world's `ObjectStore` was cleared but the page's `objectId -> world`
/// index was not, so it grew by one entry per handle the main world ever
/// minted, for the life of the page.
#[test]
fn a_commit_forgets_the_handles_it_invalidates() {
    let page = page();
    let utility = page.create_isolated_world("utility").expect("created");
    for context in [None, Some(utility.context_id)] {
        for _ in 0..20 {
            let handle = page
                .evaluate_in(context, "({})", &EvaluateOptions::default())
                .expect("context")
                .expect_done();
            assert!(handle.result.object_id.is_some());
        }
    }
    assert_eq!(page.retained_object_count(), 40);

    page.load_html("<!doctype html><title>next</title>")
        .expect("load");
    assert_eq!(
        page.retained_object_count(),
        0,
        "a commit invalidates every handle"
    );
    // …and a handle from before the commit is a clean error, not a stale hit.
    assert!(page.get_properties(1, None).is_err());
}

/// A `callFunctionOn` that replaces the document must not leave its `this`
/// handle alive across the commit.
///
/// The handle is a `JsValue` of the world it was minted in, and the trailing
/// `run_until_stalled` is where a queued replacement runs. Holding it to the
/// end of the call let it outlive the runtime the commit freed, and
/// `JS_FreeRuntime` **aborts the process** on a non-empty `gc_obj_list`
/// (ADR-0033 D4) — a crash, not a failing assertion. Playwright's `setContent`
/// is exactly this shape: `document.close()` from a utility-world handle.
#[test]
fn a_call_that_replaces_the_document_releases_its_this_handle() {
    let page = page();
    page.load_html("<!doctype html><p>old</p>").expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    let holder = page
        .evaluate_in(
            Some(utility.context_id),
            "({ marker: 1 })",
            &EvaluateOptions::default(),
        )
        .expect("context")
        .expect_done();
    let id = holder.result.object_id.expect("objectId");

    page.call_function_on(
        "function () { document.open(); \
         document.write('<!doctype html><p id=s>new</p>'); \
         document.close(); }",
        Some(id),
        None,
        &[],
        &EvaluateOptions {
            await_promise: true,
            defer_await: true,
            ..EvaluateOptions::default()
        },
    )
    .expect("call");
    page.settle(std::time::Duration::from_secs(5));

    assert_eq!(
        eval_in(&page, None, "document.getElementById('s').textContent"),
        "new"
    );
}

/// The replacement keeps its execution contexts (ADR-0034 D2): HTML's
/// `document.open()` reuses the `Document`, the `Window` and the environment
/// settings object, so ids stay put and handles minted before it still resolve.
/// Telling a driver otherwise makes it reject every command in flight —
/// including the one that asked for the replacement.
#[test]
fn a_script_created_parser_replacement_keeps_its_contexts() {
    let page = page();
    page.load_html("<!doctype html><p>old</p>").expect("load");
    let utility = page.create_isolated_world("utility").expect("created");
    let main_before = page.execution_context_id();

    let handle = page
        .evaluate_in(
            Some(utility.context_id),
            "({ kept: 'yes' })",
            &EvaluateOptions::default(),
        )
        .expect("context")
        .expect_done()
        .result
        .object_id
        .expect("objectId");

    eval_in(
        &page,
        None,
        "document.open(); document.write('<!doctype html><p id=s>new</p>'); document.close();",
    );
    page.settle(std::time::Duration::from_secs(5));

    assert_eq!(
        eval_in(&page, None, "document.getElementById('s').textContent"),
        "new"
    );
    assert_eq!(
        page.execution_context_id(),
        main_before,
        "a replacement must not renumber the main world"
    );
    assert!(
        page.worlds()
            .iter()
            .any(|w| w.context_id == utility.context_id),
        "the utility world must keep its id across a replacement"
    );
    assert!(
        page.get_properties(handle, None).is_ok(),
        "a handle minted before the replacement must survive it"
    );

    // A real navigation still does the opposite, so ADR-0033 D9 is intact.
    page.load_html("<!doctype html><p>next</p>").expect("load");
    assert_ne!(page.execution_context_id(), main_before);
    assert!(page.get_properties(handle, None).is_err());
}

/// A replacement keeps its isolated worlds *and their globals*, and they see
/// the new document (ADR-0034 D2).
///
/// What this cannot assert from JS is the other half of the same change: the
/// world's per-document state — its listener registry, its observers, and the
/// **strong** connected-wrapper retentions — is reset rather than carried over.
/// None of that is observable to script (node ids of the replaced arena can
/// never alias, since the fresh arena is seeded above the old generation
/// high-water mark, so a stale handler is silent rather than wrong). It is
/// enforced structurally instead: `reset_worlds_pending_net` runs the same
/// `reset_for_navigation_with` on a surviving world that the main world gets.
/// Skipping it leaks one pinned detached document per replacement.
#[test]
fn a_replacement_keeps_its_worlds_and_shows_them_the_new_document() {
    let page = page();
    page.load_html("<!doctype html><body><p id=x>old</p></body>")
        .expect("load");
    let utility = page.create_isolated_world("utility").expect("created");

    // A listener and an observer in the utility world, both on the old tree.
    eval_in(
        &page,
        Some(utility.context_id),
        "globalThis.__hits = 0;
         document.getElementById('x').addEventListener('click', () => { __hits++; });
         globalThis.__ro = new ResizeObserver(() => {});
         __ro.observe(document.getElementById('x'));",
    );

    eval_in(
        &page,
        None,
        "document.open(); \
         document.write('<!doctype html><body><p id=y>new</p></body>'); \
         document.close();",
    );
    page.settle(std::time::Duration::from_secs(5));

    // The world survived with its global intact — that is D2's whole point.
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(globalThis.__hits)"),
        "0",
        "the world's global must survive the replacement"
    );
    // …and it can see the new document.
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "document.getElementById('y').textContent"
        ),
        "new"
    );
    // The old document's listener is gone rather than lingering on a dead id.
    assert_eq!(
        eval_in(
            &page,
            Some(utility.context_id),
            "String(globalThis.__ro instanceof ResizeObserver)"
        ),
        "true",
        "the observer object itself is a plain JS value and survives"
    );
}

/// Init scripts do **not** re-run into a global that survived the replacement
/// (ADR-0034 D2).
///
/// `Page.addScriptToEvaluateOnNewDocument` promises a *fresh* global. A
/// replacement has none — the script already ran into this one — and running it
/// again is a `SyntaxError: redeclaration` for the top-level `let`/`const`/
/// `class` a driver's injected bootstrap is made of. It would be swallowed into
/// `report_script_error`, leaving the utility world half-initialised. Chrome
/// does not re-run them for `document.open()` either.
#[test]
fn a_replacement_does_not_re_run_init_scripts_into_a_surviving_world() {
    let page = page();
    page.create_isolated_world("utility").expect("created");
    // The shape that breaks: a top-level `let`, plus a counter that would show
    // a second run even if the redeclaration were somehow tolerated.
    page.add_init_script_for(
        "let __bootstrap = 1; globalThis.__runs = (globalThis.__runs || 0) + 1;",
        Some("utility"),
    );

    page.load_html("<!doctype html><p>first</p>").expect("load");
    let utility = page
        .worlds()
        .into_iter()
        .find(|w| w.name == "utility")
        .expect("world");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(__runs)"),
        "1",
        "a real navigation runs the init script once, against a fresh global"
    );

    eval_in(
        &page,
        None,
        "document.open(); document.write('<!doctype html><p>second</p>'); document.close();",
    );
    page.settle(std::time::Duration::from_secs(5));

    let utility = page
        .worlds()
        .into_iter()
        .find(|w| w.name == "utility")
        .expect("the world survives a replacement");
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(__runs)"),
        "1",
        "a replacement must not run the init script a second time"
    );
    assert_eq!(
        eval_in(&page, Some(utility.context_id), "String(__bootstrap)"),
        "1",
        "and the binding it declared must still be intact"
    );
}
