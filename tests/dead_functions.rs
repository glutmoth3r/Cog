use cog::{build, bytecode, project, sdk};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cog-dead-functions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.cog"), source).unwrap();
        Self(root)
    }
    fn build(&self) -> cog::backend::CompiledProgram {
        let prepared = project::prepare(&self.0.join("main.cog"), None, false).unwrap();
        build::project(prepared, &self.0.join("main.cogb"))
            .unwrap()
            .program
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn output(program: &cog::backend::CompiledProgram, jit: bool) -> String {
    let bytes = bytecode::encode(program).unwrap();
    let program = sdk::Engine::new().decode(&bytes).unwrap();
    let mut output = Vec::new();
    program
        .run_in(&mut sdk::RunContext::new(&mut output).with_jit(jit))
        .unwrap();
    String::from_utf8(output).unwrap()
}

#[test]
fn application_artifacts_drop_dead_functions_but_compilation_retains_them() {
    let source = r#"@!include("std") use io;
        @inline("never") fn unused() -> int { 101 }
        @inline("never") fn called(x: int) -> int { x + 1 }
        fn main() { io::println(called(4))!!; }
    "#;
    let full = project::compile_source("application.cog", source).unwrap();
    assert!(full.functions.values().any(|name| name == "unused"));
    let fixture = Fixture::new(source);
    let reduced = fixture.build();
    assert!(!reduced.functions.values().any(|name| name == "unused"));
    assert!(reduced.instructions.len() < full.instructions.len());
    for jit in [false, true] {
        assert_eq!(output(&reduced, jit), "5\n");
        assert_eq!(output(&full, jit), "5\n");
    }
    let cached = fixture.build();
    assert_eq!(cached.functions, reduced.functions);
}

#[test]
fn application_keeps_indirect_targets_closures_and_async_calls() {
    let fixture = Fixture::new(
        r#"@!include("std") use io;
        @inline("never") fn first(x: int) -> int { x + 1 }
        @inline("never") fn second(x: int) -> int { x + 2 }
        fn invoke(index: int) -> int {
            const functions = [first, second];
            (functions[index])(10)
        }
        @async fn child() { io::println("child")!!; }
        @async fn main() {
            const base = 40;
            const closure = fn[base](x: int) -> int { base + x };
            io::println((closure)(2))!!;
            io::println(invoke(0))!!;
            io::println(invoke(1))!!;
            await child();
        }
    "#,
    );
    let program = fixture.build();
    for jit in [false, true] {
        assert_eq!(output(&program, jit), "42\n11\n12\nchild\n");
    }
}

#[test]
fn registered_native_applications_keep_the_complete_callable_table() {
    let fixture = Fixture::new(
        r#"@!include("std") use host;
        @inline("never") fn unused() -> int { 17 }
        fn main() { host::value(); }
    "#,
    );
    let mut engine = sdk::Engine::new();
    engine
        .register(
            "host::value",
            sdk::Signature::new(vec![], sdk::Type::Int),
            |_, _| Ok(sdk::Value::int(1)),
        )
        .unwrap();
    let built = engine
        .build_project(
            &fixture.0.join("main.cog"),
            &fixture.0.join("host.cogb"),
            None,
            false,
        )
        .unwrap();
    assert!(built.program.disassemble().contains("unused"));
    built.program.run(&mut Vec::new()).unwrap();
}

#[test]
fn unused_global_initializers_keep_their_effects() {
    let fixture = Fixture::new(
        r#"@!include("std") use io;
        fn initialize() -> int { io::println("initialize")!!; 42 }
        const unused = initialize();
        fn main() { io::println("main")!!; }
    "#,
    );
    let program = fixture.build();
    assert_eq!(output(&program, false), "initialize\nmain\n");
}

#[test]
fn packaged_applications_contain_only_reachable_callables() {
    let fixture = Fixture::new(
        r#"@!include("std") use io;
        @inline("never") fn unused() -> int { 100 }
        fn main() { io::println("packaged")!!; }
    "#,
    );
    std::fs::write(
        fixture.0.join("package.kdl"),
        "package \"dce\" version=\"0.1.0\" kind=\"application\"\n",
    )
    .unwrap();
    let archive = fixture.0.join("application.cogpkg");
    cog::package::build_archive(&fixture.0, Some(&archive), false).unwrap();
    let package = cog::package::open_application(&archive).unwrap();
    let artifact = bytecode::decode(&package.bytecode).unwrap();
    assert!(!artifact.functions.values().any(|name| name == "unused"));
    assert_eq!(output(&artifact, false), "packaged\n");
}

#[test]
#[ignore = "requires a C compiler and the Cog AOT runtime archive"]
fn aot_application_prunes_functions_but_library_exports_remain_callable() {
    let source = r#"@!include("std") use io;
        @inline("never") public fn exported(x: int) -> int { x + 2 }
        @inline("never") fn unused() -> int { 100 }
        fn main() { io::println(7)!!; }
    "#;
    let fixture = Fixture::new(source);
    let program = fixture.build();
    let executable = fixture.0.join("application");
    cog::native::build_executable(&program, &executable).unwrap();
    let result = std::process::Command::new(&executable).output().unwrap();
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(result.stdout, b"7\n");

    let library_path = fixture.0.join("library.so");
    let library_program = project::compile_source(
        "aot-library.cog",
        "public fn exported(x: int) -> int { x + 2 } fn main() {}",
    )
    .unwrap();
    cog::native::build_library(&library_program, &library_path, &["exported".into()]).unwrap();
    // SAFETY: the library was generated above with these exact C signatures,
    // and stays loaded until its context has been closed.
    unsafe {
        let library = libloading::Library::new(&library_path).unwrap();
        let open: libloading::Symbol<unsafe extern "C" fn() -> *mut std::ffi::c_void> =
            library.get(b"cog_native_open").unwrap();
        let invoke: libloading::Symbol<
            unsafe extern "C" fn(*mut std::ffi::c_void, i64, *mut i64) -> i32,
        > = library.get(b"cog_export_exported").unwrap();
        let close: libloading::Symbol<unsafe extern "C" fn(*mut std::ffi::c_void) -> i32> =
            library.get(b"cog_native_close").unwrap();
        let context = open();
        assert!(!context.is_null());
        let mut value = 0;
        let status = invoke(context, 40, &mut value);
        assert_eq!(close(context), 0);
        assert_eq!(status, 0);
        assert_eq!(value, 42);
    }
}

#[test]
#[ignore = "requires rustc's wasm32-wasip1 target and Node with WASI Preview 1"]
fn wasi_application_executes_after_function_remapping() {
    let source = r#"@!include("std") use io;
        @inline("never") fn unused() -> int { 100 }
        @inline("never") fn called(x: int) -> int { x + 1 }
        fn main() { io::println(called(4))!!; }
    "#;
    let fixture = Fixture::new(source);
    let program = project::compile_source("wasi-application.cog", source).unwrap();
    let destination = fixture.0.join("application.wasm");
    cog::wasm::build_executable(&program, &destination).unwrap();
    let result = std::process::Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/wasm/run.mjs"
        ))
        .arg(&destination)
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(result.stdout, b"5\n");
}
