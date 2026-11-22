//! Linux AOT artifact production and the private ABI between linked code and Cog.

use crate::backend::CompiledProgram;
use crate::jit::JitEntry;
use crate::runtime::{RuntimeContext, VmCapabilities};
use crate::sdk::{Effects, Engine, Signature, Type};
use crate::value::{RuntimeType, Value};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub struct BuildReport {
    pub functions: usize,
    pub code_bytes: usize,
    pub codegen_time: Duration,
}

#[repr(C)]
struct LinkedEntry {
    pc: usize,
    function: JitEntry,
}

pub fn build_executable(
    artifact: &CompiledProgram,
    destination: &Path,
) -> Result<BuildReport, String> {
    link(artifact, destination, &[], &[], &[])
}

struct Export {
    name: String,
    pc: usize,
    arity: usize,
}

/// Link a shared library and its C header with explicit scalar exports.
pub fn build_library(
    artifact: &CompiledProgram,
    destination: &Path,
    names: &[String],
) -> Result<BuildReport, String> {
    use crate::instruction::Instruction;
    if names.is_empty() || destination.extension().is_none_or(|ext| ext != "so") {
        return Err("library builds require at least one --export and a .so output".into());
    }
    if artifact.callables.values().any(|sig| sig.asynchronous)
        || artifact.instructions.iter().any(|inst| {
            matches!(
                inst,
                Instruction::StoreGlobal(_) | Instruction::LoadGlobal(_)
            )
        })
    {
        return Err("native libraries currently require synchronous code without globals".into());
    }
    let mut exports = Vec::new();
    let mut symbols = std::collections::BTreeSet::new();
    for name in names {
        let symbol = name.replace("::", "_");
        if symbol.is_empty()
            || !symbol
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !symbols.insert(symbol.clone())
        {
            return Err(format!("invalid or duplicate C export name: {name:?}"));
        }
        let lookup = |qualified: bool| {
            artifact.functions.iter().filter(move |(_, function)| {
                if qualified {
                    *function == name
                } else {
                    function.rsplit("::").next() == Some(name.as_str())
                }
            })
        };
        let mut matches = lookup(true).peekable();
        let mut unqualified = lookup(false);
        let (entry, ambiguous) = if matches.peek().is_some() || name.contains("::") {
            (matches.next(), matches.next().is_some())
        } else {
            (unqualified.next(), unqualified.next().is_some())
        };
        let (&pc, _) = entry.ok_or_else(|| format!("unknown Cog export: {name:?}"))?;
        if ambiguous {
            return Err(format!(
                "ambiguous Cog export: {name:?}; use a qualified name"
            ));
        }
        let signature = artifact
            .callables
            .get(&pc)
            .ok_or("export has no callable signature")?;
        if signature.captures != 0
            || signature.parameters > 16
            || signature.result_type != Some(RuntimeType::Int)
            || (signature.parameters > 0
                && !signature
                    .parameter_types
                    .as_ref()
                    .is_some_and(|types| types.iter().all(|ty| *ty == Some(RuntimeType::Int))))
        {
            return Err(format!(
                "export {name:?} must have at most 16 int parameters and return int"
            ));
        }
        exports.push(Export {
            name: symbol,
            pc,
            arity: signature.parameters,
        });
    }
    link(artifact, destination, &exports, &[], &[])
}

struct Import {
    name: String,
    symbol: String,
    scalar: Scalar,
}

#[repr(u32)]
#[derive(Clone, Copy)]
enum Scalar {
    Int = 1,
    Float = 2,
}

impl Scalar {
    fn signature(self) -> Signature {
        let ty = match self {
            Self::Int => Type::Int,
            Self::Float => Type::Float,
        };
        Signature::new(vec![ty], ty).with_effects(Effects::ALL)
    }
}

/// Compile an application with explicitly declared unary C scalar functions.
/// The declarations are metadata during compilation. The native linker fixes
/// each symbol and the linked application registers it before decoding.
pub fn build_executable_with_c_imports(
    prepared: crate::project::Prepared,
    destination: &Path,
    declarations: &[String],
    objects: &[PathBuf],
) -> Result<(BuildReport, Vec<crate::diagnostic::Diagnostic>), String> {
    if prepared.kind != crate::config::PackageKind::Application
        || !prepared.plugins.is_empty()
        || declarations.is_empty()
    {
        return Err(
            "C imports require an application with explicit declarations and no plugins".into(),
        );
    }
    let mut engine = Engine::new();
    let mut imports = Vec::new();
    let mut symbols = std::collections::BTreeSet::new();
    for declaration in declarations {
        let (name, parts) = declaration
            .split_once('=')
            .ok_or("expected --c-import module::name=symbol:i64|f64")?;
        let (symbol, scalar) = parts
            .split_once(':')
            .ok_or("expected C symbol and scalar type")?;
        let scalar = match scalar {
            "i64" => Scalar::Int,
            "f64" => Scalar::Float,
            _ => return Err("C imports support i64 and f64 scalar signatures".into()),
        };
        // Prefixes belonging to the generated glue and runtime are reserved.
        if !c_identifier(symbol)
            || symbol == "main"
            || symbol.starts_with("cog_")
            || !symbols.insert(symbol)
            || !name.split("::").all(c_identifier)
        {
            return Err(format!("invalid or duplicate C import: {declaration:?}"));
        }
        engine
            .register_metadata(name, scalar.signature())
            .map_err(|error| error.to_string())?;
        imports.push(Import {
            name: name.to_owned(),
            symbol: symbol.to_owned(),
            scalar,
        });
    }
    let (program, warnings) = engine
        .compile_prepared_with_warnings(prepared)
        .map_err(|error| error.to_string())?;
    let report = link(
        &program.into_compiled(),
        destination,
        &[],
        &imports,
        objects,
    )?;
    Ok((report, warnings))
}

fn c_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn link(
    artifact: &CompiledProgram,
    destination: &Path,
    exports: &[Export],
    imports: &[Import],
    objects: &[PathBuf],
) -> Result<BuildReport, String> {
    if !cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        return Err("Cog AOT requires a Linux x86_64 GNU host".into());
    }
    if imports.is_empty()
        && artifact
            .imports
            .iter()
            .any(crate::runtime::NativeImport::is_registered)
    {
        return Err("AOT builds do not support external imports yet".into());
    }
    let runtime = std::env::var_os("COG_NATIVE_RUNTIME")
        .map(PathBuf::from)
        .or_else(|| {
            Some(
                std::env::current_exe()
                    .ok()?
                    .with_file_name("libcog_native.a"),
            )
        })
        .ok_or("set COG_NATIVE_RUNTIME to the matching Cog runtime archive")?;
    if !runtime.is_file() {
        return Err(format!(
            "Cog AOT runtime archive missing at {}. Set COG_NATIVE_RUNTIME to the matching archive",
            runtime.display()
        ));
    }

    let runtime = runtime.canonicalize().map_err(|error| error.to_string())?;
    let started = Instant::now();
    let bytecode = crate::bytecode::encode(artifact).map_err(|error| error.to_string())?;
    let length = bytecode.len();
    let (object, entries, code_bytes) = crate::jit::emit_aot_object(artifact, bytecode)?;
    let codegen_time = started.elapsed();
    if entries.is_empty() {
        return Err("no callable can be compiled to native code".into());
    }

    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let parent = parent.canonicalize().map_err(|error| error.to_string())?;
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".cog-native-{:032x}", u128::from_ne_bytes(random)));
    fs::create_dir(&temporary).map_err(|error| error.to_string())?;
    let temporary = BuildDirectory(temporary);
    let object_path = temporary.0.join("program.o");
    let launcher_path = temporary.0.join("launcher.c");
    let executable = temporary.0.join("program");
    fs::write(&object_path, object).map_err(|error| error.to_string())?;
    let mut source = launcher(&entries);
    let header = temporary.0.join("api.h");
    let mut command = Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()));
    command.args(["-O2", "-Wl,--gc-sections", "-Wl,--strip-debug"]);
    if exports.is_empty() {
        let bindings = if imports.is_empty() {
            "NULL"
        } else {
            emit_import_glue(&mut source, imports);
            "bindings"
        };
        writeln!(source, "int main(void) {{ return cog_aot_run_v2(cog_aot_bytecode, {length}, entries, {}, {bindings}, {}); }}", entries.len(), imports.len()).unwrap();
    } else {
        let (glue, api) = library_glue(length, entries.len(), exports);
        fs::write(&header, api).map_err(|error| error.to_string())?;
        source.push_str(&glue);
        command.args([
            "-shared",
            "-fPIC",
            "-Wl,--exclude-libs,ALL",
            "-Wl,-z,defs",
            "-Wl,-z,nodelete",
        ]);
    }
    fs::write(&launcher_path, source).map_err(|error| error.to_string())?;
    let result = command
        .arg(&launcher_path)
        .arg(&object_path)
        .args(
            objects
                .iter()
                .map(|path| {
                    if !matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("o" | "a")
                    ) {
                        return Err(format!(
                            "--c-link expects a C .o or .a file: {}",
                            path.display()
                        ));
                    }
                    path.canonicalize()
                        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .arg(&runtime)
        .args([
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
            "-o",
        ])
        .arg(&executable)
        .output()
        .map_err(|error| format!("cannot start native linker driver: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "native link failed: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    if !exports.is_empty() {
        fs::rename(header, destination.with_extension("h"))
            .map_err(|error| format!("cannot publish C header: {error}"))?;
    }
    fs::rename(executable, destination)
        .map_err(|error| format!("cannot publish executable: {error}"))?;
    Ok(BuildReport {
        functions: entries.len(),
        code_bytes,
        codegen_time,
    })
}

struct BuildDirectory(PathBuf);

impl Drop for BuildDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn launcher(entries: &[usize]) -> String {
    let mut source = String::from(
        "#include <stddef.h>\n#include <stdint.h>\n\
         typedef int32_t (*entry_fn)(void *);\n\
         struct entry { size_t pc; entry_fn fn; };\n\
         extern const unsigned char cog_aot_bytecode[];\n\
         struct binding { const char *name; uint32_t scalar; uint64_t (*call)(uint64_t); };\n\
         extern int32_t cog_aot_run_v2(const unsigned char *, size_t, const struct entry *, size_t, const struct binding *, size_t);\n",
    );
    for pc in entries {
        writeln!(source, "extern int32_t cog_aot_fn_{pc}(void *);").unwrap();
    }
    source.push_str("static const struct entry entries[] = {\n");
    for pc in entries {
        writeln!(source, "    {{{pc}, cog_aot_fn_{pc}}},").unwrap();
    }
    source.push_str("};\n");
    source
}

fn emit_import_glue(source: &mut String, imports: &[Import]) {
    source.push_str("#include <string.h>\n");
    for (index, import) in imports.iter().enumerate() {
        let ty = match import.scalar {
            Scalar::Int => "int64_t",
            Scalar::Float => "double",
        };
        // A local C adapter gives the linker and compiler the actual C ABI.
        // Only scalar bits cross the private adapter ABI to the runtime.
        writeln!(source, "extern {ty} {}({ty});", import.symbol).unwrap();
        writeln!(source, "static uint64_t cog_import_{index}(uint64_t bits) {{ {ty} x; memcpy(&x, &bits, sizeof(x)); x = {}(x); memcpy(&bits, &x, sizeof(x)); return bits; }}", import.symbol).unwrap();
    }
    source.push_str("static const struct binding bindings[] = {\n");
    for (index, import) in imports.iter().enumerate() {
        writeln!(
            source,
            "    {{\"{}\", {}, cog_import_{index}}},",
            import.name, import.scalar as u32
        )
        .unwrap();
    }
    source.push_str("};\n");
}

#[repr(C)]
struct LinkedImport {
    name: *const std::ffi::c_char,
    scalar: u32,
    call: unsafe extern "C" fn(u64) -> u64,
}

// SAFETY: this symbol is the private launcher ABI. Only a launcher linked from
// the same verified artifact may provide the bytecode and machine code table.
#[unsafe(no_mangle)]
unsafe extern "C" fn cog_aot_run_v2(
    bytes: *const u8,
    length: usize,
    entries: *const LinkedEntry,
    count: usize,
    bindings: *const LinkedImport,
    binding_count: usize,
) -> i32 {
    match std::panic::catch_unwind(|| {
        // SAFETY: the linker binds immutable objects with these lengths and
        // keeps every code entry mapped for the process lifetime.
        let bytes = unsafe { std::slice::from_raw_parts(bytes, length) };
        let entries = unsafe { std::slice::from_raw_parts(entries, count) };
        let bindings = if binding_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(bindings, binding_count) }
        };
        run(bytes, entries, bindings)
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("{error}");
            1
        }
        Err(_) => 1,
    }
}

fn run(bytes: &[u8], entries: &[LinkedEntry], bindings: &[LinkedImport]) -> Result<(), String> {
    let mut engine = Engine::new();
    for binding in bindings {
        // SAFETY: generated glue provides a static NUL-terminated identifier
        // and a C adapter of the private u64 -> u64 signature.
        let name = unsafe { std::ffi::CStr::from_ptr(binding.name) }
            .to_str()
            .map_err(|e| e.to_string())?;
        let scalar = match binding.scalar {
            1 => Scalar::Int,
            2 => Scalar::Float,
            _ => return Err("invalid linked C scalar type".into()),
        };
        let call = binding.call;
        engine.register(name, scalar.signature(), move |_, arguments| {
            Ok(match scalar {
                Scalar::Int => crate::sdk::Value::int(unsafe { call(arguments[0].as_int()? as u64) } as i64),
                Scalar::Float => crate::sdk::Value::float(f64::from_bits(unsafe { call(arguments[0].as_float()?.to_bits()) })),
            })
        }).map_err(|error| error.to_string())?;
    }
    let artifact = if bindings.is_empty() {
        crate::bytecode::decode(bytes).map_err(|error| error.to_string())?
    } else {
        engine
            .decode_compiled(bytes)
            .map_err(|error| error.to_string())?
    };
    let entries: Vec<_> = entries
        .iter()
        .map(|entry| (entry.pc, entry.function))
        .collect();
    let plan = crate::interpreter::prepare_aot_plan(&artifact, &entries)?;
    let mut stdout = std::io::stdout();
    let mut context = RuntimeContext::new(&mut stdout, VmCapabilities::default());
    context.set_process_arguments(std::env::args_os().skip(1).collect());
    context.configure_jit(true, None, std::env::var_os("COG_AOT_TRACE").is_some());
    crate::interpreter::run_with_callable_owner_and_plan(
        &artifact,
        &mut context,
        &Rc::new(()),
        &plan,
    )
    .map_err(|error| match context.last_source_location() {
        Some(location) => format!(
            "runtime error at {}:{}:{}: {error}",
            location.file.escape_debug(),
            location.line,
            location.column
        ),
        None => format!("runtime error: {error}"),
    })
}

fn library_glue(length: usize, count: usize, exports: &[Export]) -> (String, String) {
    let mut header = String::from(
        "#pragma once\n#include <stdint.h>\n\
        #ifdef __cplusplus\nextern \"C\" {\n#endif\n\
        typedef struct cog_native_context cog_native_context;\n\
        enum { COG_NATIVE_OK = 0, COG_NATIVE_ERROR = 1, COG_NATIVE_FAILURE = 2 };\n\
        cog_native_context *cog_native_open(void);\n\
        int32_t cog_native_close(cog_native_context *context);\n",
    );
    let mut glue = format!(
        "#include \"api.h\"\n\
         extern cog_native_context *cog_aot_open_v2(const unsigned char *, size_t, const struct entry *, size_t);\n\
         extern int32_t cog_aot_close_v2(cog_native_context *, const unsigned char *);\n\
         extern int32_t cog_aot_invoke_int_v2(cog_native_context *, const unsigned char *, size_t, const int64_t *, size_t, int64_t *);\n\
         cog_native_context *cog_native_open(void) {{ return cog_aot_open_v2(cog_aot_bytecode, {length}, entries, {count}); }}\n\
         int32_t cog_native_close(cog_native_context *context) {{ return cog_aot_close_v2(context, cog_aot_bytecode); }}\n",
    );
    for export in exports {
        let Export { name, pc, arity } = export;
        let mut prototype = format!("int32_t cog_export_{name}(cog_native_context *context");
        for index in 0..*arity {
            write!(prototype, ", int64_t arg{index}").unwrap();
        }
        prototype.push_str(", int64_t *result)");
        writeln!(header, "{prototype};").unwrap();
        writeln!(glue, "{prototype} {{").unwrap();
        let arguments = if *arity == 0 {
            "NULL"
        } else {
            let values = (0..*arity)
                .map(|n| format!("arg{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(glue, "    const int64_t arguments[] = {{{values}}};").unwrap();
            "arguments"
        };
        writeln!(
            glue,
            "    return cog_aot_invoke_int_v2(context, cog_aot_bytecode, {pc}, {arguments}, {arity}, result);\n}}"
        )
        .unwrap();
    }
    header.push_str("#ifdef __cplusplus\n}\n#endif\n");
    (glue, header)
}

struct LibraryProgram {
    artifact: CompiledProgram,
    plan: Rc<crate::interpreter::VmExecutionPlan>,
    callable_owner: Rc<()>,
}

struct LibraryContext {
    image: *const u8,
    thread: std::thread::ThreadId,
    program: RefCell<LibraryProgram>,
}

// These private entry points are called only by the generated C wrappers.
// The caller owns the context pointer until close and keeps it live during calls.
#[unsafe(no_mangle)]
unsafe extern "C" fn cog_aot_open_v2(
    bytes: *const u8,
    length: usize,
    entries: *const LinkedEntry,
    count: usize,
) -> *mut LibraryContext {
    std::panic::catch_unwind(|| {
        let artifact =
            crate::bytecode::decode(unsafe { std::slice::from_raw_parts(bytes, length) }).ok()?;
        let entries = unsafe { std::slice::from_raw_parts(entries, count) };
        let entries: Vec<_> = entries
            .iter()
            .map(|entry| (entry.pc, entry.function))
            .collect();
        let plan = crate::interpreter::prepare_aot_plan(&artifact, &entries).ok()?;
        Some(Box::into_raw(Box::new(LibraryContext {
            image: bytes,
            thread: std::thread::current().id(),
            program: RefCell::new(LibraryProgram {
                artifact,
                plan,
                callable_owner: Rc::new(()),
            }),
        })))
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
unsafe extern "C" fn cog_aot_close_v2(context: *mut LibraryContext, image: *const u8) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Check the thread using only the immutable thread identifier. The
        // RefCell and the contained VM state are confined to the owner.
        if context.is_null()
            || unsafe { (*context).image } != image
            || unsafe { (*context).thread } != std::thread::current().id()
        {
            return 2;
        }
        if unsafe { (*context).program.try_borrow_mut() }.is_err() {
            return 2;
        }
        drop(unsafe { Box::from_raw(context) });
        0
    }))
    .unwrap_or(2)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn cog_aot_invoke_int_v2(
    context: *mut LibraryContext,
    image: *const u8,
    entry: usize,
    arguments: *const i64,
    count: usize,
    result: *mut i64,
) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if context.is_null()
            || result.is_null()
            || unsafe { (*context).image } != image
            || unsafe { (*context).thread } != std::thread::current().id()
        {
            return 2;
        }
        let Ok(program) = (unsafe { (*context).program.try_borrow_mut() }) else {
            return 2;
        };
        let arguments = if count == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(arguments, count) }
                .iter()
                .copied()
                .map(Value::Int)
                .collect()
        };
        match crate::interpreter::run_aot_export(
            &program.artifact,
            &program.plan,
            &program.callable_owner,
            entry,
            arguments,
        ) {
            Ok(Value::Int(value)) => {
                unsafe { result.write(value) };
                0
            }
            Ok(Value::Error(_)) => 1,
            Err(error) if error.is_recoverable() => 1,
            _ => 2,
        }
    }))
    .unwrap_or(2)
}
