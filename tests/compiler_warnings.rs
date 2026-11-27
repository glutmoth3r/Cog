use cog::{diagnostic::Severity, project};

#[test]
fn warns_about_unused_local_bindings_but_not_globals_or_underscore_names() {
    let source = "const GLOBAL = 7;\nfn main() -> int {\n    const unused = 1;\n    const _intentional = 2;\n    const used = GLOBAL;\n    return used;\n}\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "W0001");
    assert_eq!(warnings[0].severity(), Severity::Warning);
    assert!(warnings[0].message.contains("`unused`"), "{warnings:?}");
    let span = warnings[0].span.as_ref().expect("local declaration span");
    assert_eq!(&span.source.text[span.range.clone()], "const unused = 1;");
}

#[test]
fn warns_for_unreachable_statements_and_simplifiable_boolean_conditionals() {
    let source = "fn helper() { if false { const dead = 1; } }\nfn main() -> bool {\n    return if true { true } else { false };\n    const unreachable = 1;\n}\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        warnings
            .iter()
            .any(|warning| { warning.code == "W0002" && warning.message.contains("unreachable") }),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| { warning.code == "W0003" && warning.message.contains("always true") }),
        "{warnings:?}"
    );
    assert!(
        !warnings.iter().any(|warning| warning.code == "W0001"),
        "bindings in unreachable code should not get duplicate warnings: {warnings:?}"
    );
}

#[test]
fn warns_when_a_conditional_returns_its_boolean_condition() {
    let source = "fn helper(flag: bool) -> bool {\n    return if flag { true } else { false };\n}\nfn main() { helper(true); }\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        warnings.iter().any(|warning| {
            warning.code == "W0004" && warning.message.contains("use the condition directly")
        }),
        "{warnings:?}"
    );
}

#[test]
fn does_not_suggest_a_boolean_simplification_for_a_dynamic_condition() {
    let source = "fn helper(flag) -> bool {\n    return if flag { true } else { false };\n}\nfn main() { helper(1); }\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(
        !warnings.iter().any(|warning| warning.code == "W0004"),
        "{warnings:?}"
    );
}

/// Source text highlighted by each error interpolation warning.
fn error_interpolations(body: &str) -> Vec<String> {
    let source = format!(
        "@!include(\"std\")\nuse io;\nuse err;\nuse err::Error;\nfn risky() -> int {{ 1 / 0 }}\nfn log(message: str) {{ io::println(message)!!; }}\nfn main() -> nil {{\n{body}\n}}\n"
    );
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", &source)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    warnings
        .iter()
        .filter(|warning| warning.code == "W0008")
        .map(|warning| {
            assert_eq!(warning.severity(), Severity::Warning);
            let span = warning.span.as_ref().expect("hole span");
            span.source.text[span.range.clone()].to_owned()
        })
        .collect()
}

#[test]
fn warns_when_a_catch_binding_is_interpolated_into_a_string() {
    for body in [
        "try { risky()!!; } catch(error) { log($\"failed: {error}\"); }",
        "try { risky()!!; } catch(error) { io::println($\"failed: {error}\")!!; }",
        "const text = try { $\"{risky()}\" } catch(error) { $\"failed: {error}\" };\n log(text);",
        "try { risky()!!; } catch(_error) { log($\"failed: {_error}\"); }",
        "try { risky()!!; } catch(error) { const copy = error; log($\"failed: {copy}\"); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error as str}\"); }",
        "try { risky()!!; } catch(error) { log($\"outer {$\"inner {error}\"}\"); }",
        "try { risky()!!; } catch(error) { log(\"failed: \" + $\"{error}\"); }",
        "try { risky()!!; } catch(error) { log($\"{error}\")!!; }",
        "const result = risky();\n when result is Error::DivisionByZero | Error::Overflow { log($\"{result}\"); }",
        "const show = fn[](failure: Error) -> str { $\"{failure}\" };",
        "try { risky()!!; } catch(error) { const show = fn[error]() -> nil { log($\"{error}\"); }; (show)(); }",
    ] {
        assert_eq!(error_interpolations(body).len(), 1, "{body}");
    }
}

#[test]
fn warns_when_a_value_known_to_be_an_error_is_interpolated() {
    for body in [
        "const result = risky();\n when result is Error { log($\"failed: {result}\"); }",
        "const result = risky();\n when result is err::Error::DivisionByZero { log($\"failed: {result}\"); }",
        "const result = risky();\n match result { Error => { log($\"failed: {result}\"); }, _ => {} }",
    ] {
        assert_eq!(error_interpolations(body), ["result"], "{body}");
    }
}

#[test]
fn warns_when_a_parameter_declared_as_error_is_interpolated() {
    let source = "@!include(\"std\")\nuse err::Error;\nfn describe(error: Error) -> str { $\"failed: {error}\" }\nfn main() { describe(Error::MissingKey()); }\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "W0008");
}

#[test]
fn does_not_treat_a_user_type_named_error_as_an_error() {
    let source = "@!include(\"std\")\nuse io;\nenum Error { Custom }\nfn describe(error: Error) -> str { $\"{error}\" }\nfn main() {\n    const value = Error::Custom();\n    when value is Error { io::println(describe(value))!!; io::println($\"{value}\")!!; }\n}\n";
    let (_, warnings) = project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn does_not_warn_when_error_handling_in_an_interpolation_is_visible() {
    for body in [
        "try { risky()!!; } catch(error) { log($\"failed: {error!!}\"); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error}\"!!); }",
        "try { risky()!!; } catch(error) { log((\"a\" + $\"failed: {error}\")!!); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error ?! \"unknown\"}\"); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error}\" ?! \"unknown\"); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error}\".error_kind()); }",
        "try { risky()!!; } catch(error) { log($\"failed: {error.error_kind()}\"); }",
        "try { risky()!!; } catch(error) { log($\"failed: {err::kind(error)}\"); }",
        "try { risky()!!; } catch(error) { io::println(error)!!; }",
        "const result = risky();\n log($\"value: {result}\");",
        "const result = risky();\n when result is Error {} else { log($\"value: {result}\"); }",
        "const result = risky();\n when result is int { log($\"value: {result}\"); }",
        "const result = risky();\n when result is Error {}\n log($\"value: {result}\");",
        "try { risky()!!; } catch(error) { const error = \"text\"; log($\"{error}\"); }",
        "try { risky()!!; } catch(error) { var copy = error; copy = \"text\"; log($\"{copy}\"); }",
        "const result = risky();\n when result is Error::ParseFailure(result, _) { log($\"{result}\"); }",
        "const result = risky();\n when result is Error | int { log($\"{result}\"); }",
        "var value = risky();\n for _i in 0..2 { when value is Error { value = 0; log($\"{value}\"); } }",
        "try { risky()!!; } catch(error) { var copy = error; for _i in 0..2 { log($\"{copy}\"); copy = \"text\"; } }",
        "try { risky()!!; } catch(error) { var copy = error; const reset = fn[var copy]() -> nil { copy = \"text\"; }; (reset)(); log($\"{copy}\"); }",
        "try { risky()!!; } catch(error) { log(($\"failed: {error}\" as str)!!); }",
        "try { risky()!!; } catch(error) { log(($\"failed: {error}\" ?? \"none\")!!); }",
    ] {
        assert!(error_interpolations(body).is_empty(), "{body}");
    }
}

#[test]
fn error_interpolation_warning_suggests_working_replacements_without_changing_behavior() {
    let source = "@!include(\"std\")\nuse io;\nuse err;\nfn main() {\n    try { io::println(1 / 0); } catch(failure) {\n        const lost = $\"failed: {failure}\";\n        io::println(lost.error_kind())!!;\n        io::println($\"kept: {failure.error_kind()}\")!!;\n        io::println(failure)!!;\n        io::println($\"text: {err::message(failure)}\")!!;\n    }\n}\n";
    let (program, warnings) =
        project::compile_source_with_warnings("warnings.cog", source).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].message.contains("whole string an Error"));
    assert!(
        warnings[0].notes[0].contains("`failure`")
            && warnings[0].helps[0].contains("{failure.error_kind()}"),
        "{warnings:?}"
    );
    assert!(
        warnings[0].helps[1].contains("{err::message(failure)}")
            && warnings[0].helps[1].contains("use err;"),
        "{warnings:?}"
    );
    let decoded = cog::bytecode::decode(&cog::bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    cog::interpreter::run_with(&decoded, &mut output).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "DivisionByZero\nkept: DivisionByZero\nError: division by zero\ntext: division by zero\n"
    );
}
