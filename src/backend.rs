//! Structured stack-bytecode emission. Function and branch identities are
//! distinct, regardless of source names or their diagnostic representation.
use crate::instruction::Instruction;
use crate::value::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Label(u32);

#[derive(Debug, PartialEq, Eq, Hash)]
enum Target {
    Function(String),
    Block(Label),
}

#[derive(Debug, Default, Clone)]
pub struct CompiledProgram {
    pub instructions: Vec<Instruction>,
    pub imports: Vec<crate::runtime::NativeImport>,
    pub functions: BTreeMap<usize, String>,
    pub callables: BTreeMap<usize, CallableSignature>,
    pub source_map: Vec<SourceMapEntry>,
    /// Logical frames of callees whose bodies the compiler inlined. Source map
    /// entries refer to the innermost frame by index.
    pub inline_frames: Vec<InlineFrame>,
}

/// A half-open instruction range attributed to one source coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMapEntry {
    pub start: usize,
    pub end: usize,
    pub location: crate::diagnostic::SourceLocation,
    /// Innermost inlined callee that produced these instructions, if any.
    pub inline: Option<usize>,
}

/// One inlined call. The frame has no VM call record: its caller is `parent`,
/// or the compiled function containing the instruction when `parent` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFrame {
    pub function: String,
    /// Source coordinate of the call expression in the caller.
    pub call_site: Option<crate::diagnostic::SourceLocation>,
    /// Enclosing inline frame. It always has a smaller index.
    pub parent: Option<usize>,
}

/// Drops frames that no source map entry reaches and renumbers the rest in
/// their original order, so parents keep smaller indices.
pub(crate) fn retain_referenced_inline_frames(
    frames: &[InlineFrame],
    source_map: &mut [SourceMapEntry],
) -> Vec<InlineFrame> {
    let mut used = vec![false; frames.len()];
    for entry in source_map.iter() {
        let mut current = entry.inline;
        while let Some(index) = current {
            // Out-of-range references are left for the bytecode validator.
            let Some(seen) = used.get_mut(index) else {
                break;
            };
            if std::mem::replace(seen, true) {
                break;
            }
            current = frames[index].parent.filter(|parent| *parent < index);
        }
    }
    let mut renumbered = vec![None; frames.len()];
    let mut retained = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        if used[index] {
            renumbered[index] = Some(retained.len());
            retained.push(InlineFrame {
                parent: frame
                    .parent
                    .and_then(|parent| renumbered.get(parent).copied().flatten()),
                ..frame.clone()
            });
        }
    }
    for entry in source_map {
        if let Some(index) = entry.inline
            && let Some(renumbered) = renumbered.get(index)
        {
            entry.inline = *renumbered;
        }
    }
    retained
}

/// Runtime call contract. Entries partition the instruction table into regions;
/// names are diagnostic metadata and are not needed to validate an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallableSignature {
    pub parameters: usize,
    pub captures: usize,
    pub asynchronous: bool,
    /// Sparse metadata allocation: `None` means every parameter is dynamic.
    /// `Some` is positional and has the same length as `parameters`.
    pub parameter_types: Option<Vec<Option<crate::value::RuntimeType>>>,
    /// Missing result annotations remain dynamically typed.
    pub result_type: Option<crate::value::RuntimeType>,
}

impl CallableSignature {
    pub fn dynamic(parameters: usize, captures: usize) -> Self {
        Self {
            parameters,
            captures,
            asynchronous: false,
            parameter_types: None,
            result_type: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Emitter {
    instructions: Vec<Instruction>,
    targets: HashMap<Target, usize>,
    patches: Vec<(usize, Target)>,
    next_label: u32,
    functions: BTreeMap<usize, String>,
    callables: BTreeMap<usize, CallableSignature>,
    stack_depth: usize,
    unreachable: bool,
    label_depths: HashMap<Label, usize>,
    imports: Vec<crate::runtime::NativeImport>,
    error: Option<String>,
    source_location: Option<crate::diagnostic::SourceLocation>,
    source_map: Vec<SourceMapEntry>,
    inline_frames: Vec<InlineFrame>,
    inline_frame: Option<usize>,
}

impl Emitter {
    pub fn emit(&mut self, instruction: Instruction) {
        if self.unreachable {
            return;
        }
        let pc = self.instructions.len();
        if let Some(location) = &self.source_location {
            if let Some(last) = self.source_map.last_mut()
                && last.end == pc
                && last.location == *location
                && last.inline == self.inline_frame
            {
                last.end += 1;
            } else {
                self.source_map.push(SourceMapEntry {
                    start: pc,
                    end: pc + 1,
                    location: location.clone(),
                    inline: self.inline_frame,
                });
            }
        }
        if !self.unreachable {
            if let Instruction::StackTruncate(depth) = &instruction {
                self.stack_depth = *depth;
            } else {
                let (pop, push) = instruction
                    .stack_effect(&self.imports)
                    .expect("checked compiler instruction has a valid stack effect");
                if let Some(depth) = self
                    .stack_depth
                    .checked_sub(pop)
                    .and_then(|depth| depth.checked_add(push))
                {
                    self.stack_depth = depth;
                } else {
                    self.error.get_or_insert_with(|| {
                        format!(
                            "internal error: invalid operand stack at instruction {}",
                            self.instructions.len()
                        )
                    });
                }
            }
        }
        if matches!(
            instruction,
            Instruction::Jump(_)
                | Instruction::Return
                | Instruction::ReturnPropagate
                | Instruction::Crash(_)
                | Instruction::Halt
        ) {
            self.unreachable = true;
        }
        self.instructions.push(instruction);
    }
    pub fn extend(&mut self, instructions: impl IntoIterator<Item = Instruction>) {
        for instruction in instructions {
            self.emit(instruction);
        }
    }

    pub fn stack_depth(&self) -> usize {
        self.stack_depth
    }

    pub fn enter_source(
        &mut self,
        span: Option<&crate::diagnostic::SourceSpan>,
    ) -> Option<crate::diagnostic::SourceLocation> {
        std::mem::replace(
            &mut self.source_location,
            span.map(crate::diagnostic::SourceLocation::from_span),
        )
    }

    pub fn leave_source(&mut self, previous: Option<crate::diagnostic::SourceLocation>) {
        self.source_location = previous;
    }

    /// Attributes following instructions to an inlined body of `function`,
    /// called at the current source coordinate.
    pub fn enter_inline(&mut self, function: &str) -> Option<usize> {
        self.inline_frames.push(InlineFrame {
            function: function.to_owned(),
            call_site: self.source_location.clone(),
            parent: self.inline_frame,
        });
        self.inline_frame.replace(self.inline_frames.len() - 1)
    }

    pub fn leave_inline(&mut self, previous: Option<usize>) {
        self.inline_frame = previous;
    }

    pub fn label(&mut self) -> Label {
        let label = Label(self.next_label);
        self.next_label += 1;
        label
    }

    pub fn bind(&mut self, label: Label) {
        if let Some(depth) = self.label_depths.get(&label) {
            self.stack_depth = *depth;
            self.unreachable = false;
        }
        assert!(
            self.targets
                .insert(Target::Block(label), self.instructions.len())
                .is_none(),
            "duplicate internal label"
        );
    }

    pub fn function_with_types(
        &mut self,
        name: &str,
        parameter_types: Vec<Option<crate::value::RuntimeType>>,
        result_type: Option<crate::value::RuntimeType>,
    ) {
        self.function_with_capture_types(name, parameter_types, result_type, 0);
    }

    pub fn function_with_capture_types(
        &mut self,
        name: &str,
        parameter_types: Vec<Option<crate::value::RuntimeType>>,
        result_type: Option<crate::value::RuntimeType>,
        captures: usize,
    ) {
        self.register_function(name, parameter_types, result_type, false, captures);
    }

    pub fn function_with_async_types(
        &mut self,
        name: &str,
        parameter_types: Vec<Option<crate::value::RuntimeType>>,
        result_type: Option<crate::value::RuntimeType>,
        asynchronous: bool,
    ) {
        self.register_function(name, parameter_types, result_type, asynchronous, 0);
    }

    fn register_function(
        &mut self,
        name: &str,
        parameter_types: Vec<Option<crate::value::RuntimeType>>,
        result_type: Option<crate::value::RuntimeType>,
        asynchronous: bool,
        captures: usize,
    ) {
        let parameters = parameter_types.len();
        let parameter_types = parameter_types
            .iter()
            .any(Option::is_some)
            .then_some(parameter_types);
        self.stack_depth = parameters;
        self.unreachable = false;
        let address = self.instructions.len();
        assert!(
            self.targets
                .insert(Target::Function(name.to_owned()), address)
                .is_none(),
            "duplicate checked function"
        );
        self.functions.insert(address, name.to_owned());
        self.callables.insert(
            address,
            CallableSignature {
                parameters,
                captures,
                asynchronous,
                parameter_types,
                result_type,
            },
        );
    }

    pub fn jump(&mut self, instruction: Instruction, label: Label) {
        if self.unreachable {
            return;
        }
        let reachable = !self.unreachable;
        self.patches
            .push((self.instructions.len(), Target::Block(label)));
        self.emit(instruction);
        if reachable {
            self.label_depths.entry(label).or_insert(self.stack_depth);
        }
    }

    pub fn call(&mut self, name: &str, arity: usize) {
        if self.unreachable {
            return;
        }
        self.patches
            .push((self.instructions.len(), Target::Function(name.to_owned())));
        self.emit(Instruction::Call(0, arity));
    }

    pub fn call_async(&mut self, name: &str, arity: usize) {
        if self.unreachable {
            return;
        }
        self.patches
            .push((self.instructions.len(), Target::Function(name.to_owned())));
        self.emit(Instruction::CallAsync(0, arity));
    }

    pub fn make_closure(&mut self, name: &str, captures: usize) {
        if self.unreachable {
            return;
        }
        self.patches
            .push((self.instructions.len(), Target::Function(name.to_owned())));
        self.emit(Instruction::MakeClosure(0, captures));
    }

    pub fn push_function(&mut self, name: &str) {
        if self.unreachable {
            return;
        }
        self.patches
            .push((self.instructions.len(), Target::Function(name.to_owned())));
        self.emit(Instruction::Push(Value::Function(0)));
    }

    pub fn call_native(&mut self, import: crate::runtime::NativeImport) {
        let index = self
            .imports
            .iter()
            .position(|item| item.name() == import.name())
            .unwrap_or_else(|| {
                self.imports.push(import);
                self.imports.len() - 1
            });
        self.emit(Instruction::CallNative(index));
    }

    pub fn finish(self) -> Result<CompiledProgram, String> {
        let program = self.resolve()?;
        let optimized = crate::optimizer::optimize(&program);
        match crate::bytecode::validate(&optimized) {
            Ok(()) => Ok(optimized),
            Err(_) => {
                // An optimizer invariant failure must preserve executable
                // compiler output when the pre-optimization program is valid.
                crate::bytecode::validate(&program)?;
                Ok(program)
            }
        }
    }

    pub fn check(self) -> Result<(), String> {
        if std::env::var_os("COG_OPT_TRACE").is_some() {
            return self.finish().map(drop);
        }
        let program = self.resolve()?;
        match crate::bytecode::validate(&program) {
            Ok(()) => Ok(()),
            // Optimization can shrink the program enough to satisfy validation limits.
            Err(error) => {
                crate::bytecode::validate(&crate::optimizer::optimize(&program)).map_err(|_| error)
            }
        }
    }

    fn resolve(mut self) -> Result<CompiledProgram, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut order: Vec<usize> = (0..self.imports.len()).collect();
        order.sort_by_key(|index| self.imports[*index].name());
        let mut remap = vec![0; order.len()];
        let imports = order
            .iter()
            .enumerate()
            .map(|(new, old)| {
                remap[*old] = new;
                self.imports[*old].clone()
            })
            .collect();
        for instruction in &mut self.instructions {
            if let Instruction::CallNative(index) = instruction {
                *index = remap[*index];
            }
        }
        for (index, target) in self.patches {
            let address =
                self.targets.get(&target).copied().ok_or_else(|| {
                    format!("internal error: unresolved bytecode target {target:?}")
                })?;
            match &mut self.instructions[index] {
                Instruction::Call(target, _)
                | Instruction::CallAsync(target, _)
                | Instruction::MakeClosure(target, _)
                | Instruction::Jump(target)
                | Instruction::JumpIfFalse(target)
                | Instruction::JumpIfTrue(target)
                | Instruction::JumpIfNoPropagation(target) => *target = address,
                Instruction::Push(Value::Function(target)) => *target = address,
                _ => return Err("internal error: invalid bytecode fixup".into()),
            }
        }
        let inline_frames =
            retain_referenced_inline_frames(&self.inline_frames, &mut self.source_map);
        Ok(CompiledProgram {
            instructions: self.instructions,
            imports,
            functions: self.functions,
            callables: self.callables,
            source_map: self.source_map,
            inline_frames,
        })
    }
}

impl CompiledProgram {
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions,
            ..Self::default()
        }
    }

    pub fn source_location(
        &self,
        instruction: usize,
    ) -> Option<&crate::diagnostic::SourceLocation> {
        self.source_entry(instruction).map(|entry| &entry.location)
    }

    pub fn source_entry(&self, instruction: usize) -> Option<&SourceMapEntry> {
        let index = self
            .source_map
            .partition_point(|entry| entry.end <= instruction);
        self.source_map
            .get(index)
            .filter(|entry| entry.start <= instruction && instruction < entry.end)
    }

    pub fn source_location_before(
        &self,
        instruction: usize,
    ) -> Option<&crate::diagnostic::SourceLocation> {
        self.source_map
            .partition_point(|entry| entry.start <= instruction)
            .checked_sub(1)
            .and_then(|index| self.source_map.get(index))
            .map(|entry| &entry.location)
    }

    /// Human-readable inspection only; the compiler never parses this output.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (index, import) in self.imports.iter().enumerate() {
            out.push_str(&format!(
                "import {index} {} v{} {:?} -> {:?}\n",
                import.name(),
                import.version(),
                import.parameters(),
                import.result()
            ));
        }
        for (pc, instruction) in self.instructions.iter().enumerate() {
            if let Some(signature) = self.callables.get(&pc) {
                out.push_str(&format!(
                    "function {pc} args={} captures={} parameter_types={:?} result_type={:?}\n",
                    signature.parameters,
                    signature.captures,
                    signature.parameter_types,
                    signature.result_type
                ));
            }
            if let Some(name) = self.functions.get(&pc) {
                out.push_str(&format!("{name}:\n"));
            }
            let text = match instruction {
                Instruction::Push(Value::Str(s)) => format!("push {s:?}"),
                Instruction::Push(Value::Float(n)) => format!("push {}", float_text(*n)),
                Instruction::Push(value) => format!("push {value}"),
                Instruction::Load(n) => format!("load {n}"),
                Instruction::Store(n) => format!("store {n}"),
                Instruction::LoadGlobal(n) => format!("load_global {n}"),
                Instruction::StoreGlobal(n) => format!("store_global {n}"),
                Instruction::Call(target, arity) => format!(
                    "call {} {arity}",
                    self.functions
                        .get(target)
                        .map(String::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| target.to_string())
                ),
                Instruction::CallAsync(target, arity) => format!(
                    "call_async {} {arity}",
                    self.functions
                        .get(target)
                        .map(String::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| target.to_string())
                ),
                Instruction::Await => "await".into(),
                Instruction::Spawn => "spawn".into(),
                Instruction::YieldNow => "yield_now".into(),
                Instruction::TimeoutAsync => "timeout".into(),
                Instruction::Select2 => "select2".into(),
                Instruction::Select => "select".into(),
                Instruction::SleepAsync => "sleep".into(),
                Instruction::ReadBytesAsync => "read_bytes".into(),
                Instruction::ProcessRunAsync => "run".into(),
                Instruction::ProcessRunTimeoutAsync => "run_timeout".into(),
                Instruction::ProcessRunInputAsync => "run_input".into(),
                Instruction::ProcessRunInputTimeoutAsync => "run_input_timeout".into(),
                Instruction::ProcessRunInputTimeoutOutputLimitAsync => {
                    "run_input_timeout_output_limit".into()
                }
                Instruction::NetworkExchangeAsync => "tcp_exchange".into(),
                Instruction::TcpListenAsync => "tcp_listen".into(),
                Instruction::TcpAcceptAsync => "tcp_accept".into(),
                Instruction::TcpConnectAsync => "tcp_connect".into(),
                Instruction::TcpReadAsync => "tcp_read".into(),
                Instruction::TcpReadExactAsync => "tcp_read_exact".into(),
                Instruction::TcpReadAllAsync => "tcp_read_all".into(),
                Instruction::TcpWriteAsync => "tcp_write".into(),
                Instruction::TcpWriteAllAsync => "tcp_write_all".into(),
                Instruction::TcpShutdownWriteAsync => "tcp_shutdown_write".into(),
                Instruction::AsyncIoOpen => "io_open".into(),
                Instruction::AsyncIoRead => "io_read".into(),
                Instruction::AsyncIoReadExact => "io_read_exact".into(),
                Instruction::AsyncIoReadAll => "io_read_all".into(),
                Instruction::AsyncIoWrite => "io_write".into(),
                Instruction::AsyncIoWriteAll => "io_write_all".into(),
                Instruction::AsyncIoFlush => "io_flush".into(),
                Instruction::AsyncIoSeek => "io_seek".into(),
                Instruction::FsMetadataAsync => "metadata".into(),
                Instruction::FsReadDirAsync => "read_dir".into(),
                Instruction::FsWriteBytesAsync => "write_bytes".into(),
                Instruction::FsCreateDirAsync => "create_dir".into(),
                Instruction::FsCreateDirAllAsync => "create_dir_all".into(),
                Instruction::FsRemoveFileAsync => "remove_file".into(),
                Instruction::FsRemoveDirAsync => "remove_dir".into(),
                Instruction::FsReadTextAsync => "read_text".into(),
                Instruction::FsWriteTextAsync => "write_text".into(),
                Instruction::FsReadLinesAsync => "read_lines".into(),
                Instruction::FsAppendBytesAsync => "append_bytes".into(),
                Instruction::FsAppendTextAsync => "append_text".into(),
                Instruction::FsCopyAsync => "copy".into(),
                Instruction::FsRenameAsync => "rename".into(),
                Instruction::Return => "ret".into(),
                Instruction::ReturnPropagate => "retprop".into(),
                Instruction::CallNative(index) => format!("call_native {index}"),
                Instruction::Jump(n) => format!("jmp {n}"),
                Instruction::JumpIfFalse(n) => format!("jz {n}"),
                Instruction::JumpIfTrue(n) => format!("jnz {n}"),
                Instruction::JumpIfNoPropagation(n) => format!("jnp {n}"),
                Instruction::MakeRecord(n) => format!("record {n}"),
                Instruction::MakeArray(n) => format!("array {n}"),
                Instruction::MakeTuple(n) => format!("tuple {n}"),
                Instruction::MakeTypedArray(n, ty) => format!("array_typed {n} {ty:?}"),
                Instruction::ConstrainArrayType(ty) => format!("array_check {ty:?}"),
                Instruction::ConstrainArrayFrom => "array_context".to_string(),
                Instruction::ArrayEmptyLike => "array_empty_like".to_string(),
                Instruction::Crash(Some(s)) => format!("crash {s:?}"),
                Instruction::Crash(None) => "crash".into(),
                Instruction::ArrayPush => "arrpush".into(),
                Instruction::ArrayPrepend => "arrprepend".into(),
                Instruction::IsType(ty) => format!("is_type {ty}"),
                Instruction::ErrorKind => "error_kind".into(),
                Instruction::IndexSet => "indexset".into(),
                Instruction::StackTruncate(n) => format!("stack_truncate {n}"),
                other => format!("{other:?}").to_lowercase(),
            };
            out.push_str(&text);
            out.push('\n');
        }
        out
    }
}

fn float_text(n: f64) -> String {
    let text = n.to_string();
    if text.contains('.') || text.contains('e') || !n.is_finite() {
        text
    } else {
        format!("{text}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_accepts_bytecode_that_needs_optimization_to_fit_type_limits() {
        fn emit() -> Emitter {
            use crate::value::RuntimeType;

            let oversized = (0..crate::value::MAX_RUNTIME_TYPE_DEPTH)
                .fold(RuntimeType::Int, |ty, _| RuntimeType::Array(Box::new(ty)));
            let mut out = Emitter::default();
            let end = out.label();
            out.emit(Instruction::Push(Value::Bool(false)));
            out.jump(Instruction::JumpIfFalse(0), end);
            out.extend([
                Instruction::Push(Value::Nil),
                Instruction::ConstrainArrayType(oversized),
                Instruction::Pop,
            ]);
            out.bind(end);
            out.emit(Instruction::Halt);
            out
        }

        assert!(crate::bytecode::validate(&emit().resolve().unwrap()).is_err());
        emit().check().unwrap();
    }
}
