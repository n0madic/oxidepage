//! The remote object model (ADR-0030), tested against a real realm.

use oxidepage_page::remote::{CallArgument, EvaluateOptions};
use oxidepage_page::{Page, PageOptions, RemoteError, RemoteSubtype, RemoteType};

fn page() -> Page {
    Page::new(PageOptions::default()).expect("page")
}

fn by_value() -> EvaluateOptions {
    EvaluateOptions {
        by_value: true,
        ..EvaluateOptions::default()
    }
}

#[test]
fn primitives_come_back_by_value_without_a_handle() {
    let page = page();
    for (source, kind, json) in [
        ("42", RemoteType::Number, "42"),
        ("'hi'", RemoteType::String, "\"hi\""),
        ("true", RemoteType::Boolean, "true"),
    ] {
        let outcome = page.evaluate(source, &EvaluateOptions::default());
        assert!(outcome.exception.is_none(), "{source}: {outcome:?}");
        assert_eq!(outcome.result.kind, Some(kind), "{source}");
        assert_eq!(outcome.result.value_json.as_deref(), Some(json), "{source}");
        // A primitive has no identity to preserve, so it must not consume a
        // handle a driver would then have to release.
        assert!(outcome.result.object_id.is_none(), "{source}");
    }
    assert_eq!(page.retained_object_count(), 0);
}

#[test]
fn numbers_json_cannot_carry_are_reported_unserializable() {
    let page = page();
    for (source, expected) in [
        ("NaN", "NaN"),
        ("Infinity", "Infinity"),
        ("-Infinity", "-Infinity"),
        ("-0", "-0"),
    ] {
        let outcome = page.evaluate(source, &EvaluateOptions::default());
        assert_eq!(
            outcome.result.unserializable.as_deref(),
            Some(expected),
            "{source}"
        );
        // `value` must stay absent: JSON has no spelling for these, and `null`
        // would be a different value.
        assert!(outcome.result.value_json.is_none(), "{source}");
    }
}

#[test]
fn an_object_gets_a_handle_that_survives_between_calls() {
    let page = page();
    let outcome = page.evaluate("({ a: 1, b: 'two' })", &EvaluateOptions::default());
    let id = outcome.result.object_id.expect("objectId");
    assert_eq!(outcome.result.kind, Some(RemoteType::Object));
    assert_eq!(page.retained_object_count(), 1);

    // The handle still names the same live object one command later.
    let properties = page.get_properties(id, None).expect("properties");
    let names: Vec<&str> = properties.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
    // A primitive property comes back by value — there is no identity to
    // preserve, and a handle per number is a handle a driver has to release.
    assert_eq!(
        properties[0].value.as_ref().unwrap().value_json.as_deref(),
        Some("1")
    );
    assert!(properties[0].value.as_ref().unwrap().object_id.is_none());
    assert_eq!(
        properties[1].value.as_ref().unwrap().description.as_deref(),
        Some("two")
    );
}

#[test]
fn releasing_a_handle_makes_it_unusable() {
    let page = page();
    let id = page
        .evaluate("({})", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();
    assert!(page.release_object(id));
    assert_eq!(page.retained_object_count(), 0);
    assert_eq!(
        page.get_properties(id, None).unwrap_err(),
        RemoteError::NoSuchObject(id)
    );
    // Releasing twice is not an error, just a no-op.
    assert!(!page.release_object(id));
}

#[test]
fn an_object_group_releases_together() {
    let page = page();
    let options = EvaluateOptions {
        group: Some(String::from("probe")),
        ..EvaluateOptions::default()
    };
    let first = page.evaluate("({})", &options).result.object_id.unwrap();
    let second = page.evaluate("[]", &options).result.object_id.unwrap();
    let loose = page
        .evaluate("({})", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();

    assert_eq!(page.release_object_group("probe"), 2);
    assert!(page.get_properties(first, None).is_err());
    assert!(page.get_properties(second, None).is_err());
    assert!(
        page.get_properties(loose, None).is_ok(),
        "an ungrouped handle must survive a group release"
    );
}

#[test]
fn subtypes_a_driver_branches_on_are_reported() {
    let page = page();
    for (source, subtype) in [
        ("[]", RemoteSubtype::Array),
        ("null", RemoteSubtype::Null),
        ("new Error('x')", RemoteSubtype::Error),
        ("Promise.resolve(1)", RemoteSubtype::Promise),
        ("new Date()", RemoteSubtype::Date),
        ("/x/", RemoteSubtype::Regexp),
        ("new Map()", RemoteSubtype::Map),
        ("new Set()", RemoteSubtype::Set),
    ] {
        let outcome = page.evaluate(source, &EvaluateOptions::default());
        assert_eq!(outcome.result.subtype, Some(subtype), "{source}");
    }
}

#[test]
fn an_array_description_carries_its_length() {
    let page = page();
    let outcome = page.evaluate("[1,2,3]", &EvaluateOptions::default());
    assert_eq!(outcome.result.description.as_deref(), Some("Array(3)"));
    assert_eq!(outcome.result.class_name.as_deref(), Some("Array"));
}

#[test]
fn a_plain_object_is_described_by_its_class_not_as_object_object() {
    let page = page();
    let outcome = page.evaluate("({})", &EvaluateOptions::default());
    assert_eq!(outcome.result.description.as_deref(), Some("Object"));

    let outcome = page.evaluate("class Widget {}; new Widget()", &EvaluateOptions::default());
    assert_eq!(outcome.result.class_name.as_deref(), Some("Widget"));
}

#[test]
fn by_value_uses_the_realms_own_json_stringify() {
    let page = page();
    let outcome = page.evaluate("({ a: [1, 2], b: 'x' })", &by_value());
    assert!(outcome.result.object_id.is_none());
    assert_eq!(
        outcome.result.value_json.as_deref(),
        Some(r#"{"a":[1,2],"b":"x"}"#)
    );

    // `toJSON` is honored, because the engine's own stringify is doing it — a
    // Rust re-implementation would have to re-derive this and would drift.
    let outcome = page.evaluate("({ toJSON: () => 'custom' })", &by_value());
    assert_eq!(outcome.result.value_json.as_deref(), Some("\"custom\""));
}

#[test]
fn a_cycle_yields_no_value_rather_than_a_hang() {
    let page = page();
    // `JSON.stringify` throws on a cycle. The result must simply carry no
    // value; looping or panicking here would be reachable from any page.
    let outcome = page.evaluate("const a = {}; a.self = a; a", &by_value());
    assert!(outcome.result.value_json.is_none());
    assert!(outcome.exception.is_none());
}

#[test]
fn a_thrown_error_becomes_exception_details() {
    let page = page();
    let outcome = page.evaluate("throw new TypeError('boom')", &EvaluateOptions::default());
    let exception = outcome.exception.expect("exception");
    assert!(
        exception.text.contains("boom"),
        "unhelpful text: {}",
        exception.text
    );
    assert!(
        exception.text.contains("TypeError"),
        "the error name must survive: {}",
        exception.text
    );
    // The thrown value itself is reachable, so a driver can inspect it.
    let thrown = exception.exception.expect("thrown value");
    assert_eq!(thrown.subtype, Some(RemoteSubtype::Error));
}

#[test]
fn a_syntax_error_is_an_exception_not_a_silent_undefined() {
    let page = page();
    let outcome = page.evaluate("this is not javascript", &EvaluateOptions::default());
    assert!(outcome.exception.is_some(), "{outcome:?}");
}

#[test]
fn call_function_on_binds_this_to_a_handle() {
    let page = page();
    let id = page
        .evaluate("({ n: 7 })", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();

    let outcome = page
        .call_function_on(
            "function () { return this.n * 2; }",
            Some(id),
            &[],
            &by_value(),
        )
        .expect("call");
    assert!(outcome.exception.is_none(), "{outcome:?}");
    assert_eq!(outcome.result.value_json.as_deref(), Some("14"));
}

#[test]
fn call_function_on_accepts_arrow_functions_and_literal_arguments() {
    let page = page();
    // Both drivers ship arrow functions; a bare declaration would be a syntax
    // error without the parenthesization `call_function_on` adds.
    let outcome = page
        .call_function_on(
            "(a, b) => a + b",
            None,
            &[
                CallArgument {
                    value_json: Some(String::from("2")),
                    ..CallArgument::default()
                },
                CallArgument {
                    value_json: Some(String::from("40")),
                    ..CallArgument::default()
                },
            ],
            &by_value(),
        )
        .expect("call");
    assert_eq!(outcome.result.value_json.as_deref(), Some("42"));
}

#[test]
fn call_function_on_accepts_a_handle_as_an_argument() {
    let page = page();
    let id = page
        .evaluate("({ v: 'passed' })", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();
    let outcome = page
        .call_function_on(
            "(o) => o.v",
            None,
            &[CallArgument {
                object_id: Some(id),
                ..CallArgument::default()
            }],
            &by_value(),
        )
        .expect("call");
    assert_eq!(outcome.result.value_json.as_deref(), Some("\"passed\""));
}

#[test]
fn call_function_on_reports_a_stale_handle() {
    let page = page();
    let err = page
        .call_function_on("() => 1", Some(9999), &[], &by_value())
        .unwrap_err();
    assert_eq!(err, RemoteError::NoSuchObject(9999));
}

#[test]
fn a_function_that_throws_reports_the_exception() {
    let page = page();
    let outcome = page
        .call_function_on(
            "() => { throw new Error('inner'); }",
            None,
            &[],
            &by_value(),
        )
        .expect("call");
    assert!(
        outcome
            .exception
            .as_ref()
            .is_some_and(|e| e.text.contains("inner")),
        "{outcome:?}"
    );
}

#[test]
fn await_promise_settles_an_already_resolved_promise() {
    let page = page();
    let outcome = page.evaluate(
        "Promise.resolve(5)",
        &EvaluateOptions {
            await_promise: true,
            by_value: true,
            ..EvaluateOptions::default()
        },
    );
    assert!(outcome.exception.is_none(), "{outcome:?}");
    assert_eq!(outcome.result.value_json.as_deref(), Some("5"));
}

#[test]
fn await_promise_settles_one_resolved_by_a_timer() {
    let page = page();
    // The point of running the event loop rather than reading promise state:
    // this settles in a *later* task, and a state read would say `pending`
    // forever.
    let outcome = page.evaluate(
        "new Promise(r => setTimeout(() => r('late'), 10))",
        &EvaluateOptions {
            await_promise: true,
            by_value: true,
            ..EvaluateOptions::default()
        },
    );
    assert!(outcome.exception.is_none(), "{outcome:?}");
    assert_eq!(outcome.result.value_json.as_deref(), Some("\"late\""));
}

#[test]
fn a_rejected_promise_becomes_an_exception() {
    let page = page();
    let outcome = page.evaluate(
        "Promise.reject(new Error('nope'))",
        &EvaluateOptions {
            await_promise: true,
            ..EvaluateOptions::default()
        },
    );
    assert!(
        outcome
            .exception
            .as_ref()
            .is_some_and(|e| e.text.contains("nope")),
        "{outcome:?}"
    );
}

#[test]
fn await_promise_on_a_non_promise_is_refused() {
    let page = page();
    let id = page
        .evaluate("({})", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();
    assert!(matches!(
        page.await_promise(id, &EvaluateOptions::default()),
        Err(RemoteError::WrongType(_))
    ));
}

#[test]
fn a_binding_delivers_its_payload_to_the_embedder() {
    let page = page();
    page.add_binding("__probe").expect("binding installed");

    // No event sink is installed on a bare `Page`, so the payloads stay in the
    // queue for the pull API rather than being pushed and dropped.
    page.evaluate(
        "__probe('hello'); __probe('again')",
        &EvaluateOptions::default(),
    );
    let calls = page.drain_binding_calls();
    assert_eq!(
        calls,
        vec![
            (String::from("__probe"), String::from("hello")),
            (String::from("__probe"), String::from("again")),
        ]
    );
    // Draining is destructive: the same payload must not be reported twice.
    assert!(page.drain_binding_calls().is_empty());
}

#[test]
fn a_binding_name_must_be_an_identifier() {
    let page = page();
    for name in ["", "has space", "1abc", "a-b", "a.b"] {
        assert!(
            page.add_binding(name).is_err(),
            "{name:?} should be refused"
        );
    }
    assert!(page.add_binding("$ok_1").is_ok());
}

#[test]
fn navigation_invalidates_every_handle_and_bumps_the_context() {
    let page = page();
    let id = page
        .evaluate("({ a: 1 })", &EvaluateOptions::default())
        .result
        .object_id
        .unwrap();
    let context = page.execution_context_id();
    assert_eq!(page.retained_object_count(), 1);

    page.load_html("<p>a new document</p>").expect("load");

    // The handle named a value of the outgoing document. Keeping it would pin
    // that document's whole object graph *and* let a driver read it as live.
    assert_eq!(page.retained_object_count(), 0);
    assert_eq!(
        page.get_properties(id, None).unwrap_err(),
        RemoteError::NoSuchObject(id)
    );
    assert!(
        page.execution_context_id() > context,
        "the context id must change so a driver can tell its handles died"
    );
}

#[test]
fn integral_numbers_beyond_i64_are_not_saturated() {
    let page = page();
    // `f64 as i64` saturates in Rust, so an unguarded cast printed `2**64` as
    // `9223372036854775807` — a wrong number, silently.
    let outcome = page.evaluate("2**64", &EvaluateOptions::default());
    assert_eq!(
        outcome.result.description.as_deref(),
        Some("18446744073709551616")
    );
    // The ordinary integral case still prints without a trailing `.0`, which is
    // what JavaScript does and what a driver compares against.
    assert_eq!(
        page.evaluate("7", &EvaluateOptions::default())
            .result
            .description
            .as_deref(),
        Some("7")
    );
}

#[test]
fn an_exhausted_handle_table_reports_an_exception() {
    let page = page();
    // A `RemoteObject` with no `objectId` for a non-primitive is handle-shaped
    // but names nothing — exactly what the cap exists to prevent.
    let mut last = None;
    for _ in 0..(oxidepage_page::MAX_REMOTE_OBJECTS + 2) {
        last = Some(page.evaluate("({})", &EvaluateOptions::default()));
    }
    let outcome = last.expect("at least one evaluation");
    assert!(
        outcome.exception.is_some(),
        "expected an exception once the table filled: {outcome:?}"
    );
    assert!(outcome.result.object_id.is_none());
}

#[test]
fn a_binding_payload_does_not_survive_a_navigation() {
    let page = page();
    page.add_binding("__probe").expect("binding installed");
    page.evaluate("__probe('old document')", &EvaluateOptions::default());

    page.load_html("<p>new</p>").expect("load");

    // The payload belonged to a world that is gone; reporting it now would
    // attribute it to the wrong execution context.
    assert!(page.drain_binding_calls().is_empty());
}
