use cog::backend::{CompiledProgram, InlineFrame, SourceMapEntry};
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
        inline: None,
    });
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("source map")
    );

    program.source_map[0].end = 1;
    let mut encoded = bytecode::encode(&program).unwrap();
    // The entry ends with its column and an absent inline frame index.
    let column = encoded.len() - 8;
    encoded[column..column + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        bytecode::decode(&encoded)
            .unwrap_err()
            .contains("source map")
    );
}

fn inlined_division_trace(inline: &str) -> (CompiledProgram, cog::error::RuntimeDiagnostic) {
    let source = format!(
        r#"@!include("std")
{inline}
fn ratio(a: int, b: int) -> int {{
    a / b
}}
@inline("never")
fn scale(n: int) -> int {{
    const doubled = n * 2;
    ratio(10, n) + doubled
}}
fn main() -> nil {{ scale(0)!!; }}
"#
    );
    let program = project::compile_source("inline.cog", &source).unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    assert_eq!(decoded.source_map, program.source_map);
    assert_eq!(decoded.inline_frames, program.inline_frames);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let error = interpreter::run_with_context(&decoded, &mut context).unwrap_err();
    assert_eq!(error.kind(), "DivisionByZero");
    let diagnostic = context.last_runtime_diagnostic().unwrap().clone();
    (decoded, diagnostic)
}

#[test]
fn inlined_callee_keeps_its_logical_frame_and_call_site() {
    let (program, diagnostic) = inlined_division_trace("@inline");
    assert_eq!(
        program.inline_frames,
        [InlineFrame {
            function: "ratio".into(),
            call_site: Some(SourceLocation {
                file: "inline.cog".into(),
                line: 9,
                column: 5,
            }),
            parent: None,
        }]
    );
    let trace = diagnostic.provenance.as_ref().unwrap();
    assert_eq!(trace.origin.function.as_deref(), Some("ratio"));
    assert!(trace.origin.inlined);
    let origin = trace.origin.location.as_ref().unwrap();
    assert_eq!((origin.line, origin.column), (4, 5));
    let caller = &trace.callers[0];
    assert_eq!(caller.function.as_deref(), Some("scale"));
    assert!(!caller.inlined);
    assert_eq!(caller.instruction, trace.origin.instruction);
    assert_eq!(caller.location, program.inline_frames[0].call_site);
    assert_eq!(trace.callers[1].function.as_deref(), Some("main"));
    let rendered = diagnostic.render(None);
    assert!(
        rendered.contains(
            "origin: ratio at inline.cog:4:5 (inlined)\n  called from scale at inline.cog:9:5\n"
        ),
        "{rendered}"
    );
}

#[test]
fn inlining_does_not_change_the_logical_trace_of_a_runtime_error() {
    let logical = |diagnostic: &cog::error::RuntimeDiagnostic| {
        let trace = diagnostic.provenance.as_ref().unwrap();
        std::iter::once(&trace.origin)
            .chain(&trace.callers)
            .map(|frame| (frame.function.clone(), frame.location.clone()))
            .collect::<Vec<_>>()
    };
    let (inlined_program, inlined) = inlined_division_trace("@inline");
    let (called_program, called) = inlined_division_trace("@inline(\"never\")");
    assert_eq!(inlined_program.inline_frames.len(), 1);
    assert!(called_program.inline_frames.is_empty());
    assert_eq!(logical(&inlined), logical(&called));
}

#[test]
fn inline_frames_without_reachable_instructions_are_not_serialized() {
    let program = project::compile_source(
        "dead.cog",
        r#"@!include("std")
fn half(n: int) -> int { n / 2 }
@inline("never")
fn pick(n: int) -> int {
    if n > 100 { return half(n); }
    return n;
    half(n) + half(n)
}
fn main() -> int { pick(4) }
"#,
    )
    .unwrap();
    let [frame] = program.inline_frames.as_slice() else {
        panic!("unexpected inline frames: {:?}", program.inline_frames);
    };
    assert_eq!(frame.call_site.as_ref().unwrap().line, 5);
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
}

#[test]
fn version_39_artifacts_without_inline_frames_remain_readable() {
    let mut legacy = b"COG\0\x27\0\0\0".to_vec();
    legacy.extend(0u32.to_le_bytes()); // imports
    legacy.extend(0u32.to_le_bytes()); // callables
    legacy.extend(1u32.to_le_bytes()); // instructions
    legacy.push(0x12); // halt
    legacy.extend(0u32.to_le_bytes()); // function names
    legacy.extend(1u32.to_le_bytes()); // source map entries
    legacy.extend(0u32.to_le_bytes());
    legacy.extend(1u32.to_le_bytes());
    legacy.extend(5u32.to_le_bytes());
    legacy.extend(b"a.cog");
    legacy.extend(2u32.to_le_bytes());
    legacy.extend(3u32.to_le_bytes());
    let program = bytecode::decode(&legacy).unwrap();
    assert!(program.inline_frames.is_empty());
    assert_eq!(
        program.source_map,
        [SourceMapEntry {
            start: 0,
            end: 1,
            location: SourceLocation {
                file: "a.cog".into(),
                line: 2,
                column: 3,
            },
            inline: None,
        }]
    );
    interpreter::run_with(&program, &mut Vec::new()).unwrap();
}

#[test]
fn version_38_artifacts_remain_readable_and_traces_print() {
    let mut legacy = b"COG\0\x26\0\0\0".to_vec();
    legacy.extend(0u32.to_le_bytes()); // imports
    legacy.extend(0u32.to_le_bytes()); // callables
    legacy.extend(4u32.to_le_bytes()); // instructions
    for operand in [1i64, 0] {
        legacy.push(46); // push int
        legacy.push(0);
        legacy.extend(operand.to_le_bytes());
    }
    legacy.push(4); // div
    legacy.push(0x12); // halt
    legacy.extend(1u32.to_le_bytes()); // source map entries
    legacy.extend(0u32.to_le_bytes());
    legacy.extend(4u32.to_le_bytes());
    legacy.extend(5u32.to_le_bytes());
    legacy.extend(b"a.cog");
    legacy.extend(2u32.to_le_bytes());
    legacy.extend(3u32.to_le_bytes());
    let program = bytecode::decode(&legacy).unwrap();
    assert!(program.inline_frames.is_empty());
    assert_eq!(program.source_map[0].inline, None);
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    interpreter::run_with_context(&program, &mut context).unwrap_err();
    let diagnostic = context.last_runtime_diagnostic().unwrap();
    let origin = &diagnostic.provenance.as_ref().unwrap().origin;
    assert!(!origin.inlined);
    assert!(diagnostic.render(None).contains("a.cog:2:3"));
}

fn propagation_steps(body: &str) -> Vec<(String, usize, usize)> {
    let source = format!(
        "@!include(\"std\")\n@inline(\"never\")\nfn fail(n: int) -> int {{ 10 / n }}\n{body}"
    );
    let program = project::compile_source("steps.cog", &source).unwrap();
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    interpreter::run_with_context(&program, &mut context).unwrap_err();
    let diagnostic = context.last_runtime_diagnostic().unwrap();
    let trace = diagnostic.provenance.as_ref().unwrap();
    assert!(!trace.truncated);
    trace
        .propagation
        .iter()
        .filter_map(|frame| {
            let location = frame.location.as_ref()?;
            Some((
                frame.function.clone().unwrap_or_default(),
                location.line,
                location.column,
            ))
        })
        .collect()
}

#[test]
fn each_logical_propagation_step_is_recorded_once() {
    let steps = propagation_steps(
        r#"@inline("never")
fn inner(n: int) -> int { fail(n)!! + 1 }
@inline("never")
fn outer(n: int) -> int { inner(n) * 2 }
fn main() -> nil { outer(0)!!; }
"#,
    );
    let functions = steps
        .iter()
        .map(|(function, _, _)| function.as_str())
        .collect::<Vec<_>>();
    assert_eq!(functions, ["inner", "outer", "main"], "{steps:?}");
}

#[test]
fn a_stored_error_keeps_its_call_and_later_propagation_sites() {
    let steps = propagation_steps(
        r#"@inline("never")
fn later(n: int) -> int {
    const saved = fail(n);
    saved!!
}
fn main() -> nil { later(0)!!; }
"#,
    );
    let later = steps
        .iter()
        .filter(|(function, _, _)| function == "later")
        .map(|(_, line, _)| *line)
        .collect::<Vec<_>>();
    assert_eq!(later, [6, 7], "{steps:?}");
}
