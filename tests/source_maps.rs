use cog::backend::{CompiledProgram, SourceMapEntry};
use cog::diagnostic::SourceLocation;
use cog::error::RuntimeError;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

#[test]
fn source_maps_round_trip_and_locate_terminal_runtime_errors() {
    let source = "@!include(\"std\")\nfn main() {\n    1 / 0\n}\n";
    let program = project::compile_source("src/main.cog", source).unwrap();
    let encoded = bytecode::encode(&program).unwrap();
    assert_eq!(&encoded[..4], b"COG\0");

    let decoded = bytecode::decode(&encoded).unwrap();
    assert_eq!(decoded.source_map, program.source_map);
    assert_eq!(decoded.functions, program.functions);
    let division = decoded
        .instructions
        .iter()
        .position(|instruction| matches!(instruction, cog::instruction::Instruction::Div))
        .unwrap();
    assert_eq!(
        decoded.source_location(division),
        Some(&SourceLocation {
            file: "src/main.cog".into(),
            line: 3,
            column: 5,
        })
    );

    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let result = interpreter::run_with_context(&decoded, &mut context);
    assert!(
        matches!(result, Err(RuntimeError::DivisionByZero)),
        "{result:?}"
    );
    assert_eq!(
        context.last_source_location(),
        Some(&SourceLocation {
            file: "src/main.cog".into(),
            line: 3,
            column: 5,
        })
    );
    let diagnostic = context.last_runtime_diagnostic().unwrap();
    assert_eq!(diagnostic.kind, "DivisionByZero");
    assert!(diagnostic.unhandled_error);
    assert_eq!(
        diagnostic.provenance.as_ref().unwrap().origin.instruction,
        division
    );
}

#[test]
fn stored_error_retains_origin_and_independent_propagation_after_cleanup() {
    let source = r#"@!include("std") use str;
@inline("never")
fn origin(n: int) -> int {
    12 / n
}
@inline("never")
fn store(n: int) -> int {
    const leaves: int[] = [origin(n), 1];
    leaves[0]
}
@inline("never")
fn relay() -> int {
    const saved = store(0);
    const probe = fn[saved]() -> int { saved!! };
    try { (probe)(); } catch(e) {
        if e.error_kind() != "DivisionByZero" { crash("unexpected probe result"); }
    }
    defer { const ignored = str::to_int("bad"); }
    saved!!
}
fn main() -> nil { relay()!!; }
"#;
    let program = project::compile_source("origin.cog", source).unwrap();
    let encoded = bytecode::encode(&program).unwrap();
    let program = bytecode::decode(&encoded).unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let error = interpreter::run_with_context(&program, &mut context).unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    let diagnostic = context.last_runtime_diagnostic().unwrap();
    let trace = diagnostic.provenance.as_ref().unwrap();
    assert!(trace.origin.function.as_ref().unwrap().ends_with("origin"));
    let location = trace.origin.location.as_ref().unwrap();
    assert_eq!(location.file, "origin.cog");
    assert_eq!(location.line, 4);
    assert_eq!(location.column, 5);
    let names = |frames: &[cog::error::ErrorTraceFrame]| {
        frames
            .iter()
            .filter_map(|frame| frame.function.clone())
            .collect::<Vec<_>>()
    };
    let callers = names(&trace.callers);
    assert!(callers[0].ends_with("store"));
    assert!(callers[1].ends_with("relay"));
    assert!(callers[2].ends_with("main"));
    let propagation = names(&trace.propagation);
    assert!(propagation.iter().any(|name| name.ends_with("main")));
    assert!(
        !trace.propagation.iter().any(|frame| {
            frame
                .location
                .as_ref()
                .is_some_and(|location| matches!(location.line, 15..=18))
        }),
        "the handled probe modified the saved Error: {trace:?}"
    );
    assert!(trace.observation.is_some());

    let [cleanup] = diagnostic.suppressed.as_slice() else {
        panic!("missing suppressed diagnostics: {diagnostic:?}");
    };
    assert_eq!(cleanup.kind, "ParseFailure");
    let cleanup_origin = &cleanup.provenance.as_ref().unwrap().origin;
    assert_eq!(cleanup_origin.location.as_ref().unwrap().line, 18);
    assert_eq!(
        cleanup_origin.native_operation.as_deref(),
        Some("str::to_int")
    );
}

#[test]
fn legacy_bytecode_without_names_is_still_readable() {
    // Version 38 contains the same Halt and empty source map, with no name table.
    let legacy = b"COG\0\x26\0\0\0\0\0\0\0\0\0\0\0\x01\0\0\0\x12\0\0\0\0";
    let program = bytecode::decode(legacy).unwrap();
    assert!(program.functions.is_empty());
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn provenance_is_bounded_on_recursion_and_the_host_context_resets() {
    let failing = project::compile_source(
        "deep.cog",
        r#"
        @inline("never")
        fn deep(n: int) -> int {
            if n == 0 { return 1 / n; }
            deep(n - 1)
        }
        fn main() { deep(80)!!; }
        "#,
    )
    .unwrap();
    let successful = project::compile_source("ok.cog", "fn main() {}").unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    assert!(interpreter::run_with_context(&failing, &mut context).is_err());
    let diagnostic = context.last_runtime_diagnostic().unwrap();
    let trace = diagnostic.provenance.as_ref().unwrap();
    assert!(trace.truncated);
    assert!(trace.callers.len() < 80 && trace.propagation.len() < 80);
    assert!(diagnostic.render(None).contains("trace truncated"));
    interpreter::run_with_context(&successful, &mut context).unwrap();
    assert!(context.last_runtime_diagnostic().is_none());
    assert!(context.last_source_location().is_none());
}

#[test]
fn source_map_ranges_and_coordinates_are_validated_before_encoding_and_loading() {
    let mut program = CompiledProgram::new(vec![cog::instruction::Instruction::Halt]);
    program.source_map.push(SourceMapEntry {
        start: 0,
        end: 2,
        location: SourceLocation {
            file: "test.cog".into(),
            line: 1,
            column: 1,
        },
    });
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("source map")
    );

    program.source_map[0].end = 1;
    let mut encoded = bytecode::encode(&program).unwrap();
    let last = encoded.len() - 4;
    encoded[last..].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        bytecode::decode(&encoded)
            .unwrap_err()
            .contains("source map")
    );
}
