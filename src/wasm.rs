//! WASI code generation from verified Cog bytecode.
use crate::backend::{CallableSignature, CompiledProgram};
use crate::instruction::{Instruction, ValueType};
use crate::value::{RuntimeType, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_INSTRUCTIONS: usize = 20_000;
const MAX_SLOTS: usize = 256;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const RUNTIME: &str = include_str!("wasm_guest.in");

/// Compile a supported Cog application to a WASI command using the installed Rust
/// compiler and its wasm32-wasip1 standard library.
pub fn build_executable(program: &CompiledProgram, destination: &Path) -> Result<(), String> {
    if destination
        .extension()
        .is_none_or(|extension| extension != "wasm")
    {
        return Err("WASI output path must end in .wasm".into());
    }
    let source = lower(program)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    static NEXT_BUILD: AtomicUsize = AtomicUsize::new(0);
    let directory = parent.join(format!(
        ".cog-wasi-{}-{}",
        std::process::id(),
        NEXT_BUILD.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let result = (|| {
        let source_path = directory.join("guest.rs");
        let artifact = directory.join("guest.wasm");
        std::fs::write(&source_path, source).map_err(|error| error.to_string())?;
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let result = Command::new(rustc)
            .args([
                "--target",
                "wasm32-wasip1",
                "--edition",
                "2024",
                "-O",
                "-C",
                "panic=abort",
            ])
            .arg(&source_path)
            .arg("-o")
            .arg(&artifact)
            .output()
            .map_err(|error| format!("cannot start rustc for WASI: {error}"))?;
        if !result.status.success() {
            return Err(format!(
                "WASI code generation failed. Install the Rust target with `rustup target add wasm32-wasip1`.\n{}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        std::fs::rename(artifact, destination)
            .map_err(|error| format!("cannot write {}: {error}", destination.display()))
    })();
    let _ = std::fs::remove_dir_all(directory);
    result
}

fn lower(program: &CompiledProgram) -> Result<String, String> {
    crate::bytecode::validate(program)?;
    let mut source = RUNTIME.to_owned();
    let mut called = VecDeque::from([0]);
    let mut processed = BTreeSet::new();
    let mut sites = BTreeSet::new();
    let mut globals = 0;
    let mut types = Vec::new();
    while let Some(entry) = called.pop_front() {
        if processed.insert(entry) {
            let function = Function::analyze(program, entry)?;
            sites.extend(function.heights.keys().copied());
            if sites.len() > MAX_INSTRUCTIONS {
                return Err("WASI code generation instruction limit exceeded".into());
            }
            let function = LowerFunction {
                program,
                function,
                source: &mut source,
                called: &mut called,
                globals: &mut globals,
                types: &mut types,
            };
            function.emit()?;
        }
    }
    writeln!(source, "const GLOBAL_SLOTS: usize = {globals};").unwrap();
    writeln!(
        source,
        "fn contract_types() -> Vec<Ty> {{ vec![{}] }}",
        types.join(",\n")
    )
    .unwrap();
    writeln!(source, "fn site(pc: usize) -> &'static str {{ match pc {{").unwrap();
    for pc in sites {
        let entry = program
            .callables
            .range(..=pc)
            .next_back()
            .map(|(entry, _)| entry);
        let function = entry
            .and_then(|entry| program.functions.get(entry))
            .map_or("<entry>", String::as_str);
        let location = program
            .source_location(pc)
            .map_or_else(String::new, |location| {
                format!(
                    "{}:{}:{} ",
                    location.file.escape_debug(),
                    location.line,
                    location.column
                )
            });
        writeln!(
            source,
            "{pc} => {:?},",
            format!(
                "{location}in {} (instruction {pc})",
                function.escape_debug()
            )
        )
        .unwrap();
        check_source_size(&source)?;
    }
    source.push_str("_ => \"unknown location\" } }\n");
    Ok(source)
}

fn check_source_size(source: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("WASI generated source exceeds the compiler budget".into());
    }
    Ok(())
}

fn unsupported(program: &CompiledProgram, pc: usize, reason: impl std::fmt::Display) -> String {
    let location = program
        .source_location(pc)
        .map_or_else(String::new, |location| {
            format!("{}:{}:{}: ", location.file, location.line, location.column)
        });
    format!("{location}WASI backend does not support {reason} at instruction {pc}")
}

struct Function<'a> {
    entry: usize,
    signature: Option<&'a CallableSignature>,
    heights: BTreeMap<usize, usize>,
    blocks: BTreeSet<usize>,
    slots: usize,
    locals: usize,
}

impl<'a> Function<'a> {
    fn analyze(program: &'a CompiledProgram, entry: usize) -> Result<Self, String> {
        let end = program
            .callables
            .range((std::ops::Bound::Excluded(entry), std::ops::Bound::Unbounded))
            .next()
            .map_or(program.instructions.len(), |(entry, _)| *entry);
        let signature = program.callables.get(&entry);
        let parameters = signature.map_or(0, |signature| signature.parameters);
        let mut this = Self {
            entry,
            signature,
            heights: BTreeMap::new(),
            blocks: BTreeSet::from([entry]),
            slots: parameters,
            locals: 0,
        };
        let mut pending = VecDeque::from([(entry, parameters)]);
        while let Some((pc, height)) = pending.pop_front() {
            if this.heights.contains_key(&pc) {
                continue;
            }
            if !(entry..end).contains(&pc) {
                return Err(unsupported(
                    program,
                    pc,
                    "control flow leaving the current function",
                ));
            }
            this.heights.insert(pc, height);
            let instruction = &program.instructions[pc];
            let (popped, pushed) = instruction
                .stack_effect(&program.imports)
                .map_err(|e| e.to_string())?;
            let next_height = match instruction {
                Instruction::StackTruncate(depth) => *depth,
                _ => height - popped + pushed,
            };
            if let Instruction::Load(index) | Instruction::Store(index) = instruction {
                this.locals = this.locals.max(slot_count(program, pc, *index)?);
            }
            this.slots = this.slots.max(next_height);
            if this.slots > MAX_SLOTS || this.heights.len() > MAX_INSTRUCTIONS {
                return Err(unsupported(
                    program,
                    pc,
                    "functions beyond the WASI compiler limits",
                ));
            }
            match instruction {
                Instruction::Halt
                | Instruction::Return
                | Instruction::ReturnPropagate
                | Instruction::Crash(_) => {}
                Instruction::Jump(target) => {
                    this.blocks.insert(*target);
                    pending.push_back((*target, next_height));
                }
                Instruction::JumpIfFalse(target)
                | Instruction::JumpIfTrue(target)
                | Instruction::JumpIfNoPropagation(target) => {
                    this.blocks.extend([pc + 1, *target]);
                    pending.extend([(pc + 1, next_height), (*target, next_height)]);
                }
                _ => pending.push_back((pc + 1, next_height)),
            }
        }
        Ok(this)
    }
}

fn slot_count(program: &CompiledProgram, pc: usize, index: usize) -> Result<usize, String> {
    if index >= MAX_SLOTS {
        return Err(unsupported(
            program,
            pc,
            "slot indices beyond the WASI compiler limit",
        ));
    }
    Ok(index + 1)
}

struct LowerFunction<'a, 'b> {
    program: &'a CompiledProgram,
    function: Function<'a>,
    source: &'b mut String,
    called: &'b mut VecDeque<usize>,
    globals: &'b mut usize,
    /// Distinct contract type expressions, indexed by `state.types`.
    types: &'b mut Vec<String>,
}

impl LowerFunction<'_, '_> {
    fn emit(mut self) -> Result<(), String> {
        let entry = self.function.entry;
        let arity = self
            .function
            .signature
            .map_or(0, |signature| signature.parameters);
        writeln!(
            self.source,
            "fn f_{entry}(state: &mut State, depth: usize, args: [V; {arity}]) -> Run {{"
        )
        .unwrap();
        if arity != 0 {
            write!(self.source, "let [").unwrap();
            for i in 0..arity {
                write!(self.source, "mut v{i},").unwrap();
            }
            writeln!(self.source, "] = args;").unwrap();
        }
        for i in arity..self.function.slots {
            writeln!(self.source, "let mut v{i} = V::Nil;").unwrap();
        }
        for i in 0..self.function.locals {
            writeln!(self.source, "let mut l{i}: Option<V> = None;").unwrap();
        }
        writeln!(
            self.source,
            "let mut propagation = false; let mut block = {entry}; loop {{ match block {{"
        )
        .unwrap();
        for pc in self.function.heights.keys().copied().collect::<Vec<_>>() {
            if self.function.blocks.contains(&pc) {
                writeln!(self.source, "{pc} => {{").unwrap();
            }
            let ends = self.instruction(pc, self.function.heights[&pc])?;
            check_source_size(self.source)?;
            if ends || self.function.blocks.contains(&(pc + 1)) {
                if !ends {
                    writeln!(self.source, "block = {};", pc + 1).unwrap();
                }
                self.source.push_str("}\n");
            }
        }
        self.source.push_str("_ => unreachable!() } } }\n");
        Ok(())
    }

    /// Intern a contract type and return the generated place that holds it.
    fn contract(&mut self, pc: usize, ty: &RuntimeType) -> Result<String, String> {
        let literal = type_literal(ty)
            .ok_or_else(|| unsupported(self.program, pc, format!("type contract {ty:?}")))?;
        let index = match self.types.iter().position(|known| *known == literal) {
            Some(index) => index,
            None => {
                self.types.push(literal);
                self.types.len() - 1
            }
        };
        Ok(format!("state.types[{index}]"))
    }

    /// A condition that holds when a call or return value satisfies `ty`.
    fn runtime_predicate(
        &mut self,
        pc: usize,
        ty: &RuntimeType,
        slot: usize,
    ) -> Result<String, String> {
        let pattern = match ty {
            RuntimeType::Nil => "V::Nil",
            RuntimeType::Int => "V::Int(_)",
            RuntimeType::UInt => "V::UInt(_)",
            RuntimeType::Bool => "V::Bool(_)",
            RuntimeType::Char => "V::Char(_)",
            RuntimeType::Str => "V::Str(_) | V::StrBuf(_)",
            _ => {
                let contract = self.contract(pc, ty)?;
                return Ok(format!("matches_type(&{contract}, &v{slot})"));
            }
        };
        Ok(format!("matches!(&v{slot}, {pattern} | V::Error(_))"))
    }

    /// Instructions that name a contract type, which is interned before the
    /// remaining instructions borrow the output.
    fn typed_instruction(
        &mut self,
        pc: usize,
        height: usize,
        a: usize,
    ) -> Result<Option<bool>, String> {
        use Instruction::*;
        let program = self.program;
        match &program.instructions[pc] {
            MakeTypedArray(count, element) => {
                let contract = self.contract(pc, element)?;
                write!(self.source, "v{a} = make_typed_array(vec![").unwrap();
                emit_arguments(self.source, a, *count);
                writeln!(self.source, "], &{contract}, \"array\", {pc});").unwrap();
            }
            ConstrainArrayType(element) => {
                let contract = self.contract(pc, element)?;
                writeln!(
                    self.source,
                    "v{a} = constrain_array_type(take(&mut v{a}), &{contract}, {pc});"
                )
                .unwrap();
            }
            ConstrainRecordFields(fields) => {
                let mut contracts = String::new();
                for (name, ty) in fields {
                    let contract = self.contract(pc, ty)?;
                    write!(contracts, "({name:?}, &{contract}),").unwrap();
                }
                writeln!(
                    self.source,
                    "v{a} = constrain_fields(take(&mut v{a}), &[{contracts}], {pc});"
                )
                .unwrap();
            }
            Call(target, arity) => {
                self.called.push_back(*target);
                let signature = &program.callables[target];
                let types = signature.parameter_types.as_deref().unwrap_or_default();
                let mut checks = Vec::new();
                for (i, ty) in types.iter().enumerate() {
                    if let Some(ty) = ty {
                        checks.push(self.runtime_predicate(pc, ty, a + i)?);
                    }
                }
                let out = &mut *self.source;
                // Rust evaluates arguments left to right. Checks inspect the
                // slots before the arguments are moved into the callee.
                if !checks.is_empty() {
                    writeln!(out, "if {} {{", checks.join(" && ")).unwrap();
                }
                writeln!(
                    out,
                    "if depth == 64 {{ return Err(Fault::new(\"CallDepthLimit\", {pc})); }}"
                )
                .unwrap();
                write!(out, "let (result, marked) = f_{target}(state, depth + 1, [").unwrap();
                emit_arguments(out, a, *arity);
                writeln!(out, "])?; v{a} = result; propagation |= marked;").unwrap();
                if !checks.is_empty() {
                    writeln!(out, "}} else {{").unwrap();
                    for i in a..height {
                        writeln!(out, "take(&mut v{i});").unwrap();
                    }
                    writeln!(out, "v{a} = type_mismatch({pc}, \"call\"); }}").unwrap();
                }
            }
            instruction @ (Return | ReturnPropagate) if self.function.entry != 0 => {
                let signature = self.function.signature;
                if let Some(expected) = signature.and_then(|s| s.result_type.as_ref()) {
                    let check = self.runtime_predicate(pc, expected, a)?;
                    writeln!(
                        self.source,
                        "if !{check} {{ v{a} = type_mismatch({pc}, \"return\"); }}"
                    )
                    .unwrap();
                }
                let propagates = matches!(instruction, ReturnPropagate);
                if propagates {
                    writeln!(self.source, "if !matches!(&v{a}, V::Error(_)) {{ return Err(Fault::new(\"InvalidArgument\", {pc})); }}").unwrap();
                }
                writeln!(self.source, "return Ok((v{a}, {propagates}));").unwrap();
                return Ok(Some(true));
            }
            _ => return Ok(None),
        }
        Ok(Some(false))
    }

    fn instruction(&mut self, pc: usize, height: usize) -> Result<bool, String> {
        use Instruction::*;
        let instruction = &self.program.instructions[pc];
        let (pops, _) = instruction
            .stack_effect(&self.program.imports)
            .map_err(|e| e.to_string())?;
        let a = height - pops;
        let b = a + 1;
        if let Some(ends) = self.typed_instruction(pc, height, a)? {
            return Ok(ends);
        }
        let out = &mut *self.source;
        match instruction {
            Push(value) => {
                let literal = match value {
                    Value::Int(x) => format!("V::Int({x}i64)"),
                    Value::UInt(x) => format!("V::UInt({x}u64)"),
                    Value::Bool(x) => format!("V::Bool({x})"),
                    Value::Char(x) => format!("V::Char({x:?})"),
                    Value::Nil => "V::Nil".into(),
                    Value::Str(x) => format!("V::Str({x:?}.into())"),
                    Value::TypeTag(x) => format!("V::Tag({x:?}.into())"),
                    _ => return Err(unsupported(self.program, pc, "this constant type")),
                };
                writeln!(out, "v{a} = {literal};").unwrap();
            }
            Load(index) => writeln!(out, "v{a} = l{index}.clone().ok_or(Fault::new(\"InvalidLocal\", {pc}))?;").unwrap(),
            Store(index) => writeln!(out, "l{index} = Some(take(&mut v{a}));").unwrap(),
            LoadGlobal(index) | StoreGlobal(index) => {
                *self.globals = (*self.globals).max(slot_count(self.program, pc, *index)?);
                if matches!(instruction, LoadGlobal(_)) {
                    writeln!(out, "v{a} = state.globals[{index}].clone().ok_or(Fault::new(\"InvalidLocal\", {pc}))?;").unwrap();
                } else {
                    writeln!(out, "state.globals[{index}] = Some(take(&mut v{a}));").unwrap();
                }
            }
            Pop => writeln!(out, "take(&mut v{a});").unwrap(),
            Dup => writeln!(out, "v{b} = v{a}.clone();").unwrap(),
            Swap => writeln!(out, "std::mem::swap(&mut v{a}, &mut v{b});").unwrap(),
            StackTruncate(depth) => for slot in *depth..height {
                writeln!(out, "take(&mut v{slot});").unwrap();
            },
            Add | Sub | Mul | Div | Mod => {
                let op = match instruction { Add => "add", Sub => "sub", Mul => "mul", Div => "div", _ => "rem" };
                writeln!(out, "v{a} = {op}(take(&mut v{a}), take(&mut v{b}), {pc});").unwrap();
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                writeln!(out, "v{a} = compare(take(&mut v{a}), take(&mut v{b}), Order::{instruction:?}, {pc});").unwrap();
            }
            Not => writeln!(out, "v{a} = unary_not(take(&mut v{a}), {pc});").unwrap(),
            And | Or | Xor => writeln!(out, "v{a} = logic(take(&mut v{a}), take(&mut v{b}), |a, b| a {op} b, {pc}, {name:?});",
                name = match instruction { And => "and", Or => "or", _ => "xor" },
                op = match instruction { And => "&", Or => "|", _ => "^" }).unwrap(),
            IsType(ty) => writeln!(out, "v{a} = V::Bool({});", type_predicate(ty, a)
                .ok_or_else(|| unsupported(self.program, pc, format!("type predicate {ty}")))?).unwrap(),
            Len => writeln!(out, "v{a} = length(take(&mut v{a}), {pc});").unwrap(),
            ConstrainArrayFrom => writeln!(out, "v{a} = constrain_array_from(take(&mut v{a}), take(&mut v{b}), {pc});").unwrap(),
            ArrayEmptyLike => writeln!(out, "v{a} = empty_like(take(&mut v{a}), {pc});").unwrap(),
            ArrayPush => writeln!(out, "v{a} = array_push(take(&mut v{a}), take(&mut v{b}), {pc});").unwrap(),
            ArrayPrepend => writeln!(out, "v{a} = array_prepend(take(&mut v{a}), take(&mut v{b}), {pc});").unwrap(),
            IndexSet => writeln!(out, "v{a} = index_set(take(&mut v{a}), take(&mut v{b}), take(&mut v{}), {pc});", a + 2).unwrap(),
            MakeArray(count) => {
                write!(out, "v{a} = make_array(vec![").unwrap();
                emit_arguments(out, a, *count);
                writeln!(out, "], \"array\", {pc});").unwrap();
            }
            Index => writeln!(out, "v{a} = index(take(&mut v{a}), take(&mut v{b}), {pc});").unwrap(),
            ErrorKind => writeln!(out, "v{a} = V::Str(match &v{a} {{ V::Error(error) => error.kind.as_ref(), _ => \"ok\" }}.into());").unwrap(),
            CallNative(index) => {
                let import = &self.program.imports[*index];
                let function = match import.name() {
                    "process::args" => "process_args",
                    "io::stdout" => "stdout",
                    "io::write_all" => "write_all",
                    "io::flush" => "flush",
                    "bytes::from_str" => "to_bytes",
                    "bytes::render" => "render",
                    "bytes::len" => "bytes_len",
                    "bytes::new" => "bytes_new",
                    "bytes::get" => "bytes_get",
                    "bytes::set" => "bytes_set",
                    "bytes::to_str" => "bytes_to_str",
                    "str::to_int" => "to_int",
                    "str::to_uint" => "to_uint",
                    "str::to_bool" => "to_bool",
                    "str::to_char" => "to_char",
                    "str::builder" => "str_builder",
                    "str::build" => "str_build",
                    "fmt::write" => "fmt_write",
                    "$vm::iterator_new" => "iterator_new",
                    "$vm::iterator_next" => "iterator_next",
                    "$vm::iterator_current" => "iterator_current",
                    name => return Err(unsupported(self.program, pc, format!("host operation `{name}`"))),
                };
                if import.is_registered() {
                    return Err(unsupported(self.program, pc, "registered native functions"));
                }
                write!(out, "v{a} = native({function}, {pc}, [").unwrap();
                emit_arguments(out, a, pops);
                writeln!(out, "]);").unwrap();
            }
            MakeRecord(count) => {
                if *count == 0 || !constructs_supported_record(self.program, &self.function, pc, a) {
                    return Err(unsupported(self.program, pc, "records other than structs and Error values"));
                }
                write!(out, "v{a} = make_record(vec![").unwrap();
                for i in 0..*count {
                    write!(out, "(take(&mut v{}), take(&mut v{})),", a + 2 * i, a + 2 * i + 1).unwrap();
                }
                writeln!(out, "], {pc});").unwrap();
            }
            Jump(target) => {
                writeln!(out, "block = {target};").unwrap();
                return Ok(true);
            }
            JumpIfFalse(target) | JumpIfTrue(target) => {
                let taken = matches!(instruction, JumpIfTrue(_));
                writeln!(out, "block = if condition(take(&mut v{a}), {pc})? == {taken} {{ {target} }} else {{ {} }};", pc + 1).unwrap();
                return Ok(true);
            }
            JumpIfNoPropagation(target) => {
                writeln!(out, "block = if std::mem::take(&mut propagation) {{ {} }} else {{ {target} }};", pc + 1).unwrap();
                return Ok(true);
            }
            Halt if self.function.entry == 0 => {
                let result = height.checked_sub(1).map_or("V::Nil".into(), |i| format!("v{i}"));
                writeln!(out, "return match {result} {{ V::Error(error) => Err(error), value => Ok((value, false)) }};").unwrap();
                return Ok(true);
            }
            _ => return Err(unsupported(self.program, pc, format!("{instruction:?}"))),
        }
        Ok(false)
    }
}

/// The compiler emits the type tag first, as the nearest earlier constant
/// written to the record's base slot. Only structs and core Error variants are
/// values the guest can build; enums and anonymous records remain unsupported.
///
/// This relies on codegen pushing the record's `TypeTag` before any field
/// value, and on user types carrying their own module-qualified tags that never
/// start with the `$core::err::Error::` prefix. Struct tags carry the
/// `@struct::` prefix. If codegen changes the order, the search finds no tag
/// and the record is rejected (fails closed).
fn constructs_supported_record(
    program: &CompiledProgram,
    function: &Function<'_>,
    pc: usize,
    base: usize,
) -> bool {
    let prefix = format!("{}::", crate::error::ERROR_TYPE);
    (function.entry..pc)
        .rev()
        .find(|candidate| {
            function.heights.get(candidate) == Some(&base)
                && matches!(program.instructions[*candidate], Instruction::Push(_))
        })
        .is_some_and(|candidate| {
            matches!(
                &program.instructions[candidate],
                Instruction::Push(Value::TypeTag(tag))
                    if tag.starts_with(&prefix) || tag.starts_with("@struct::")
            )
        })
}

fn emit_arguments(out: &mut String, start: usize, arity: usize) {
    for i in start..start + arity {
        write!(out, "take(&mut v{i}),").unwrap();
    }
}

/// The guest expression for a contract type, or `None` when it names values the
/// guest cannot hold.
fn type_literal(ty: &RuntimeType) -> Option<String> {
    Some(match ty {
        RuntimeType::Int => "Ty::Int".into(),
        RuntimeType::UInt => "Ty::UInt".into(),
        RuntimeType::Bool => "Ty::Bool".into(),
        RuntimeType::Char => "Ty::Char".into(),
        RuntimeType::Str => "Ty::Str".into(),
        RuntimeType::Nil => "Ty::Nil".into(),
        RuntimeType::Array(inner) => format!("Ty::Array(Rc::new({}))", type_literal(inner)?),
        RuntimeType::Nullable(inner) => {
            format!("Ty::Nullable(Rc::new({}))", type_literal(inner)?)
        }
        RuntimeType::Nominal(name) => format!("Ty::Nominal({name:?}.into())"),
        _ => return None,
    })
}

fn type_predicate(ty: &ValueType, slot: usize) -> Option<String> {
    let pattern = match ty {
        ValueType::Nil => "V::Nil",
        ValueType::Int => "V::Int(_)",
        ValueType::UInt => "V::UInt(_)",
        ValueType::Bool => "V::Bool(_)",
        ValueType::Char => "V::Char(_)",
        ValueType::Str => "V::Str(_)",
        ValueType::Bytes => "V::Bytes(_)",
        ValueType::Error => "V::Error(_)",
        ValueType::Variant(tag) if tag.starts_with(crate::error::ERROR_TYPE) => {
            return Some(format!(
                "matches!(&v{slot}, V::Error(error) if error.tag() == {tag:?})"
            ));
        }
        ValueType::Struct(name) => {
            return Some(format!(
                "matches!(&v{slot}, V::Record(record) if &*record.name == {:?})",
                name.as_ref()
            ));
        }
        _ => return None,
    };
    Some(format!("matches!(&v{slot}, {pattern})"))
}
