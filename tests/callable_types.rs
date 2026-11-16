use cog::backend::{CallableSignature, CompiledProgram};
use cog::error::RuntimeError;
use cog::instruction::Instruction;
use cog::value::{RuntimeType, Value};
use cog::{bytecode, interpreter, project};

#[test]
fn typed_callable_boundaries_preserve_owned_function_references() {
    use Instruction as I;
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Function(12)),
        I::Call(9, 1),
        I::CallValue(0),
        I::Push(Value::Int(42)),
        I::Eq,
        I::JumpIfTrue(8),
        I::Push(Value::Str("unexpected callable result".into())),
        I::Crash(None),
        I::Halt,
        I::Store(0),
        I::Load(0),
        I::Return,
        I::Push(Value::Int(42)),
        I::Return,
    ]);
    program.callables.insert(
        9,
        CallableSignature {
            parameters: 1,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(RuntimeType::Function)]),
            result_type: Some(RuntimeType::Function),
        },
    );
    program
        .callables
        .insert(12, CallableSignature::dynamic(0, 0));

    let bytes = bytecode::encode(&program).unwrap();
    assert_eq!(&bytes[..4], b"COG\0");
    let decoded = bytecode::decode(&bytes).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn typed_callable_contracts_round_trip_in_bytecode() {
    let program = project::compile_source(
        "callable_types.cog",
        "fn identity(value: int) -> int { value }\nfn main() { identity(7); }\n",
    )
    .unwrap();
    let identity = program
        .functions
        .iter()
        .find_map(|(entry, name)| (name == "identity").then_some(*entry))
        .unwrap();
    assert_eq!(
        program.callables[&identity],
        CallableSignature {
            parameters: 1,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        }
    );

    let bytes = bytecode::encode(&program).unwrap();
    assert_eq!(&bytes[..4], b"COG\0");
    let decoded = bytecode::decode(&bytes).unwrap();
    assert_eq!(decoded.callables, program.callables);
}

#[test]
fn callable_descriptors_cover_arrays_generic_structs_and_enums() {
    let program = project::compile_source(
        "callable_compound_types.cog",
        "struct Box[T: type] { value: T }\nenum Status { Ready(code: int), Done }\nfn consume(values: int[], item: Box[int], state: Status) -> int { values[0] + item.value }\nfn main() { consume([1], Box(41), Status::Ready(0)); }\n",
    )
    .unwrap();
    let bytes = bytecode::encode(&program).unwrap();
    let decoded = bytecode::decode(&bytes).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&decoded, &mut output).unwrap();
}

#[test]
fn tuple_mutation_preserves_the_contract_at_a_typed_call_boundary() {
    use Instruction as I;
    let tuple_type = RuntimeType::Tuple(vec![RuntimeType::Array(Box::new(RuntimeType::Int))]);
    let mut program = CompiledProgram::new(vec![
        I::Push(Value::Int(1)),
        I::MakeTypedArray(1, RuntimeType::Int),
        I::MakeTuple(1),
        I::Store(0),
        I::Load(0),
        I::Push(Value::Int(0)),
        I::MakeArray(0),
        I::IndexSet,
        I::Pop,
        I::Load(0),
        I::Call(12, 1),
        I::Halt,
        I::Store(0),
        I::Push(Value::Bool(true)),
        I::Return,
    ]);
    program.callables.insert(
        12,
        CallableSignature {
            parameters: 1,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(tuple_type)]),
            result_type: Some(RuntimeType::Bool),
        },
    );
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    interpreter::run_with(&decoded, &mut Vec::new()).unwrap();
}

#[test]
fn runtime_rejects_mismatched_typed_arguments_and_results() {
    let mut wrong_argument = CompiledProgram::new(vec![
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Call(3, 1),
        Instruction::Halt,
        Instruction::Store(0),
        Instruction::Load(0),
        Instruction::Return,
    ]);
    wrong_argument.callables.insert(
        3,
        CallableSignature {
            parameters: 1,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_argument).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_argument, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "call" })
    ));

    let mut wrong_result = CompiledProgram::new(vec![
        Instruction::Call(3, 0),
        Instruction::Halt,
        Instruction::Halt,
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Return,
    ]);
    wrong_result.callables.insert(
        3,
        CallableSignature {
            parameters: 0,
            captures: 0,
            asynchronous: false,
            parameter_types: None,
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_result).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_result, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "return" })
    ));

    let mut wrong_indirect_argument = CompiledProgram::new(vec![
        Instruction::Push(Value::Str("wrong".into())),
        Instruction::Push(Value::Function(4)),
        Instruction::CallValue(1),
        Instruction::Halt,
        Instruction::Store(0),
        Instruction::Load(0),
        Instruction::Return,
    ]);
    wrong_indirect_argument.callables.insert(
        4,
        CallableSignature {
            parameters: 1,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: Some(RuntimeType::Int),
        },
    );
    bytecode::validate(&wrong_indirect_argument).unwrap();
    assert!(matches!(
        interpreter::run_with(&wrong_indirect_argument, &mut Vec::new()),
        Err(RuntimeError::TypeMismatch { op: "call" })
    ));
}

#[test]
fn typed_boundaries_preserve_existing_error_value_propagation() {
    let program = project::compile_source(
        "callable_errors.cog",
        "fn identity(value: int) -> int { value }\nfn main() { identity(1 / 0) }\n",
    )
    .unwrap();
    let result = interpreter::run_with(&program, &mut Vec::new());
    assert!(
        matches!(&result, Err(RuntimeError::DivisionByZero)),
        "{result:?}"
    );
}

#[test]
fn tuple_call_boundaries_preserve_nested_error_values() {
    for source in [
        "fn first(pair: (int, str)) -> int { pair.0 } fn main() { first((1 / 0, \"kept\")) }",
        "fn make_pair() -> (int, str) { (1 / 0, \"kept\") } fn main() { const pair = make_pair(); pair.0 }",
        "fn first(pair: ((int, str), bool)) -> int { (pair.0).0 } fn main() { first(((1 / 0, \"kept\"), true)) }",
    ] {
        let program = project::compile_source("tuple_errors.cog", source).unwrap();
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        for artifact in [&program, &decoded] {
            let result = interpreter::run_with(artifact, &mut Vec::new());
            assert!(
                matches!(result, Err(RuntimeError::DivisionByZero)),
                "original tuple component Error was replaced: {result:?}; {source}"
            );
        }
    }
}

#[test]
fn typed_tuple_boundaries_do_not_poison_unrelated_components() {
    let program = project::compile_source(
        "tuple_component.cog",
        r#"
        @!include("std") use io; use err::Error;
        fn first(pair: (int, str)) -> int { pair.0 }
        fn second(pair: (int, str)) -> str { pair.1 }
        fn make_pair() -> (int, str) { (1 / 0, "returned") }
        fn main() {
            io::println(second((1 / 0, "argument")));
            io::println(make_pair().1);
            const failure = first((Error::MissingField("sentinel"), "kept"));
            when failure is Error::MissingField { io::println(failure.field); }
            else { crash("tuple boundary replaced the Error payload"); }
        }
        "#,
    )
    .unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    for artifact in [&program, &decoded] {
        let mut output = Vec::new();
        interpreter::run_with(artifact, &mut output).unwrap();
        assert_eq!(output, b"argument\nreturned\nsentinel\n");
    }
}

#[test]
fn indirect_tuple_boundaries_preserve_component_errors() {
    use Instruction as I;
    let tuple_type = RuntimeType::Tuple(vec![RuntimeType::Int, RuntimeType::Str]);
    for returns_tuple in [false, true] {
        let mut instructions = vec![
            I::Push(Value::Int(1)),
            I::Push(Value::Int(0)),
            I::Div,
            I::Push(Value::Str("kept".into())),
            I::MakeTuple(2),
            I::Push(Value::Nil),
            I::CallValue(1),
        ];
        if returns_tuple {
            instructions.extend([I::Push(Value::Int(0)), I::Index]);
        }
        instructions.push(I::Halt);
        let entry = instructions.len();
        instructions[5] = I::Push(Value::Function(entry));
        instructions.extend([I::Store(0), I::Load(0)]);
        if !returns_tuple {
            instructions.extend([I::Push(Value::Int(0)), I::Index]);
        }
        instructions.push(I::Return);
        let mut program = CompiledProgram::new(instructions);
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                asynchronous: false,
                parameter_types: Some(vec![Some(tuple_type.clone())]),
                result_type: Some(if returns_tuple {
                    tuple_type.clone()
                } else {
                    RuntimeType::Int
                }),
            },
        );
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        for artifact in [&program, &decoded] {
            let result = interpreter::run_with(artifact, &mut Vec::new());
            assert!(
                matches!(result, Err(RuntimeError::DivisionByZero)),
                "{result:?}"
            );
        }
    }
}

#[test]
fn tuple_call_contracts_reject_wrong_shapes_and_invariant_array_types() {
    use Instruction as I;
    use RuntimeType as T;
    let scalar_tuple = T::Tuple(vec![T::Int, T::Str]);
    for (argument, expected) in [
        (
            vec![
                I::Push(Value::Bool(false)),
                I::Push(Value::Str("kept".into())),
                I::MakeTuple(2),
            ],
            scalar_tuple.clone(),
        ),
        (
            vec![
                I::Push(Value::Int(1)),
                I::Push(Value::Int(0)),
                I::Div,
                I::Push(Value::Bool(true)),
                I::MakeTuple(2),
            ],
            scalar_tuple.clone(),
        ),
        (
            vec![I::Push(Value::Int(1)), I::MakeTuple(1)],
            scalar_tuple.clone(),
        ),
        (vec![I::MakeTypedArray(0, T::Int)], scalar_tuple),
        (
            vec![
                I::MakeTypedArray(0, T::Nullable(Box::new(T::Int))),
                I::Push(Value::Str("kept".into())),
                I::MakeTuple(2),
            ],
            T::Tuple(vec![T::Array(Box::new(T::Int)), T::Str]),
        ),
    ] {
        let mut instructions = argument;
        let entry = instructions.len() + 2;
        instructions.extend([
            I::Call(entry, 1),
            I::Halt,
            I::Pop,
            I::Push(Value::Nil),
            I::Return,
        ]);
        let mut program = CompiledProgram::new(instructions);
        program.callables.insert(
            entry,
            CallableSignature {
                parameters: 1,
                captures: 0,
                asynchronous: false,
                parameter_types: Some(vec![Some(expected)]),
                result_type: None,
            },
        );
        let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
        for artifact in [&program, &decoded] {
            let result = interpreter::run_with(artifact, &mut Vec::new());
            assert!(
                matches!(result, Err(RuntimeError::TypeMismatch { op: "call" })),
                "{result:?}"
            );
        }
    }
}

#[test]
fn tuple_call_contracts_accept_normal_nullable_components() {
    let program = project::compile_source(
        "tuple_nullable.cog",
        r#"
        @!include("std") use io;
        fn first(pair: (int?, str)) -> int? { pair.0 }
        fn make_pair() -> (int?, str) { (7, "kept") }
        fn main() {
            io::println(first((nil, "nil")));
            io::println(first((7, "int")));
            io::println(make_pair().0);
        }
        "#,
    )
    .unwrap();
    let decoded = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    for artifact in [&program, &decoded] {
        let mut output = Vec::new();
        interpreter::run_with(artifact, &mut output).unwrap();
        assert_eq!(output, b"nil\n7\n7\n");
    }
}

#[test]
fn encoder_rejects_inconsistent_callable_type_vector_lengths() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Halt,
        Instruction::Halt,
        Instruction::Return,
    ]);
    program.callables.insert(
        2,
        CallableSignature {
            parameters: 2,
            captures: 0,
            asynchronous: false,
            parameter_types: Some(vec![Some(RuntimeType::Int)]),
            result_type: None,
        },
    );
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("callable descriptor")
    );
}

#[test]
fn decoder_rejects_unknown_callable_type_presence_tags() {
    let mut program = CompiledProgram::new(vec![
        Instruction::Halt,
        Instruction::Push(Value::Nil),
        Instruction::Return,
    ]);
    program
        .callables
        .insert(1, CallableSignature::dynamic(0, 0));
    let mut bytes = bytecode::encode(&program).unwrap();
    bytes[30] = 2; // Invalid result-type presence tag.
    assert!(
        bytecode::decode(&bytes)
            .unwrap_err()
            .contains("optional runtime type tag")
    );
}
