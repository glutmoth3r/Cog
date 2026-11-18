use cog::sdk::{Effects, Engine, PluginManifest, Signature, Type, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cog-sdk-Ω space-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn library(&self, name: &str) -> PathBuf {
        self.0.join(format!(
            "{}{name}{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn check(command: &mut Command) {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn cargo_link_search_directives(output: &str) -> impl Iterator<Item = &str> {
    output.lines().filter_map(|line| {
        line.strip_prefix("cargo:rustc-link-search=")
            .or_else(|| line.strip_prefix("cargo::rustc-link-search="))
    })
}

fn find_cog_rlib(profile_directory: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut directories = vec![(profile_directory.to_path_buf(), 0usize)];
    let mut candidates = Vec::new();

    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                if depth < 8 && entry.file_name() != "incremental" {
                    directories.push((path, depth + 1));
                }
                continue;
            }

            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            let is_cog_library = file_name == "libcog.rlib"
                || file_name.starts_with("libcog-")
                || file_name == "cog.rlib"
                || file_name.starts_with("cog-");
            if is_cog_library
                && path
                    .extension()
                    .is_some_and(|extension| extension == "rlib")
            {
                candidates.push((directory.clone(), path));
            }
        }
    }

    candidates.into_iter().max_by_key(|(_, library)| {
        std::fs::metadata(library)
            .and_then(|metadata| metadata.modified())
            .ok()
    })
}

#[test]
fn cargo_link_search_directives_accept_both_cargo_output_syntaxes() {
    let output = concat!(
        "cargo:rustc-link-search=native=C:\\sdk\\lib\n",
        "cargo::rustc-link-search=all=C:\\other\\lib\n",
        "cargo:rerun-if-changed=build.rs\n",
    );

    assert_eq!(
        cargo_link_search_directives(output).collect::<Vec<_>>(),
        [r"native=C:\sdk\lib", r"all=C:\other\lib"]
    );
}

#[test]
fn separately_compiled_rust_plugin_uses_c_abi_and_contains_panics() {
    let directory = Directory::new();
    let executable = std::env::current_exe().expect("locate plugin test executable");
    let profile_directory = executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == "debug" || name == "release")
        })
        .map(Path::to_path_buf)
        .expect("find the Cargo profile directory for the plugin test");
    let (dependency_directory, library) = find_cog_rlib(&profile_directory)
        .expect("find the compiled Cog library in Cargo's test profile");
    let output = directory.library("rust_plugin");
    let mut compiler = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    compiler
        .arg("--edition=2024")
        .arg("--crate-type=cdylib")
        .arg("--crate-name=cog_sdk_fixture")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/native_plugin.rs"))
        .arg("--extern")
        .arg(format!("cog={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", dependency_directory.display()))
        .arg("-o")
        .arg(&output);
    // Explicit-target Cargo builds keep host proc macros in a separate directory.
    let profile = profile_directory
        .file_name()
        .expect("Cargo profile directory has a name");
    for parent in profile_directory.ancestors().skip(1).take(3) {
        let host_profile_directory = parent.join(profile);
        let host_dependencies = host_profile_directory.join("deps");
        if host_dependencies.is_dir() {
            compiler
                .arg("-L")
                .arg(format!("dependency={}", host_dependencies.display()));
        }
        if let Ok(build_outputs) = std::fs::read_dir(host_profile_directory.join("build")) {
            for build in build_outputs.flatten() {
                let build_directory = build.path();
                let generated_output = build_directory.join("out");
                if generated_output.is_dir() {
                    compiler
                        .arg("-L")
                        .arg(format!("native={}", generated_output.display()));
                }
                if let Ok(build_output) = std::fs::read_to_string(build_directory.join("output")) {
                    for search_path in cargo_link_search_directives(&build_output) {
                        compiler.arg("-L").arg(search_path);
                    }
                }
            }
        }
    }
    check(&mut compiler);
    let mut engine = Engine::new();
    unsafe { engine.load_plugin(&output) }.unwrap();
    let program = engine.compile("plugin.cog", "@!include(\"std\") use io; use host; use err; fn main() { io::println(host::double(21)); try { host::fail(); } catch(e) { io::println(e.error_kind()); } }").unwrap();
    let program = engine.decode(&program.encode().unwrap()).unwrap();
    drop(engine);
    let mut result = Vec::new();
    program.run(&mut result).unwrap();
    assert_eq!(result, b"42\nNativeFailure\n");
}

#[test]
fn plugin_manifests_are_strict_bounded_and_never_load_libraries() {
    let text = "plugin version=1 library=\"does-not-exist\"\nfunction \"host::double\" result=\"int\" { param \"int\"; }";
    let manifest = PluginManifest::parse(text).unwrap();
    assert!(PluginManifest::parse(&format!("// ignored \0\u{001D}\n{text}")).is_ok());
    let mut engine = Engine::new();
    manifest.register_metadata(&mut engine).unwrap();
    let program = engine
        .compile(
            "metadata.cog",
            "@!include(\"std\") use host; fn main() { host::double(21); }",
        )
        .unwrap();
    assert!(program.run(&mut Vec::new()).is_err());
    for invalid in [
        text.replace("version=1", "version=2"),
        text.replace("param \"int\"", "param \"unknown\""),
        text.replace("param \"int\"", "mutates 0"),
        format!("{text}\nplugin version=1 library=\"x\""),
        text.replace("param \"int\"", "effect \"output\"; effect \"output\""),
        format!("{text}\0"),
        " ".repeat(256 * 1024 + 1),
    ] {
        assert!(PluginManifest::parse(&invalid).is_err(), "{invalid:.100}");
    }
    let mut duplicate = Engine::new();
    duplicate
        .register("host::double", Signature::new(vec![], Type::Nil), |_, _| {
            Ok(Value::nil())
        })
        .unwrap();
    assert!(manifest.register_metadata(&mut duplicate).is_err());
}

#[test]
fn package_manifests_embed_strict_plugin_metadata_without_loading_code() {
    let text = r#"
package "cog-regex" version="0.1.0" kind="library" {
    asset "schemas/regex.json"
    plugin version=1 library="native/linux-x86_64/libcog_regex.so" {
        function "regex::is_match" result="bool" {
            param "str"
            param "str"
            effect "fs_read"
        }
    }
}

"#;
    let package = cog::config::package_from_source(Path::new("package.kdl"), text).unwrap();
    assert_eq!(package.assets, [Path::new("schemas/regex.json")]);
    assert_eq!(package.plugins.len(), 1);
    assert_eq!(
        package.plugins[0].library,
        Path::new("native/linux-x86_64/libcog_regex.so")
    );
    let functions = package.plugins[0].functions();
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].0, "regex::is_match");
    assert_eq!(functions[0].1.parameters, [Type::Str, Type::Str]);
    assert_eq!(functions[0].1.result, Type::Bool);
    assert_eq!(functions[0].1.effects, Effects::FS_READ);

    for invalid in [
        text.replace("native/linux-x86_64/libcog_regex.so", "../libcog_regex.so"),
        text.replace("native/linux-x86_64/libcog_regex.so", "C:/plugins/regex.dll"),
        text.replace("native/linux-x86_64/libcog_regex.so", "native\\\\regex.dll"),
        text.replace(
            "native/linux-x86_64/libcog_regex.so",
            r#"native\nregex.dll"#,
        ),
        text.replace("effect \"fs_read\"", "effect \"unknown\""),
        text.replace("asset \"schemas/regex.json\"", "asset \"../secret.json\""),
        text.replace(
            "asset \"schemas/regex.json\"",
            "asset \"schemas/regex.json\"\n    asset \"schemas/regex.json\"",
        ),
        text.replace("param \"str\"\n            param \"str\"", "param \"str\"\n            param \"str\"\n            function \"regex::is_match\" result=\"bool\""),
    ] {
        assert!(
            cog::config::package_from_source(Path::new("package.kdl"), &invalid).is_err(),
            "accepted invalid package manifest: {invalid}"
        );
    }
}

#[test]
fn package_plugin_target_subset_never_falls_back() {
    let manifest = cog::config::package_from_source(
        Path::new("package.kdl"),
        r#"package "portable-plugin" version="1" kind="library" {
    plugin version=1 {
        library target="x86_64-unknown-linux-gnu" path="native/linux.so"
        library target="x86_64-pc-windows-msvc" path="native/windows.dll"
        library target="aarch64-apple-darwin" path="native/macos.dylib"
        function "host::probe" result="int"
    }
}
"#,
    )
    .unwrap();
    let plugin = &manifest.plugins[0];
    assert_eq!(plugin.target_libraries.len(), 3);
    let mut engine = Engine::new();
    let error = unsafe {
        plugin.load_for_target(&mut engine, Path::new("."), "riscv64gc-unknown-linux-gnu")
    }
    .unwrap_err();
    assert!(error.to_string().contains("no library for target"));
    assert!(error.to_string().contains("riscv64gc-unknown-linux-gnu"));

    let subset = r#"package "portable-plugin" version="1" kind="library" {
    plugin version=1 {
        library target="x86_64-unknown-linux-gnu" path="native/linux.so"
        library target="x86_64-pc-windows-msvc" path="native/windows.dll"
        function "host::probe" result="int"
    }
}
"#;
    let subset = cog::config::package_from_source(Path::new("package.kdl"), subset).unwrap();
    assert_eq!(subset.plugins[0].target_libraries.len(), 2);
    let missing = unsafe {
        subset.plugins[0].load_for_target(
            &mut Engine::new(),
            Path::new("."),
            "aarch64-apple-darwin",
        )
    }
    .unwrap_err();
    assert!(missing.to_string().contains("no library for target"));
}

#[test]
fn plugin_manifest_library_paths_must_be_relative_to_the_manifest() {
    let absolute = std::env::temp_dir().join("cog-plugin-library");
    let encoded = serde_json::to_string(absolute.to_str().unwrap()).unwrap();
    let text = format!("plugin version=1 library={encoded}");
    let error = PluginManifest::parse(&text).unwrap_err();
    assert!(error.to_string().contains("relative to the manifest"));

    // The library path is public and can be changed after parsing, so the
    // unsafe load boundary must enforce the same documented contract.
    let mut manifest = PluginManifest::parse("plugin version=1 library=\"plugin.so\"").unwrap();
    manifest.library = absolute;
    let error = unsafe { manifest.load(&mut Engine::new(), Path::new(".")) }.unwrap_err();
    assert!(error.to_string().contains("relative to the manifest"));
}

#[cfg(any(unix, windows))]
#[test]
fn package_plugin_loader_rejects_mutated_paths_outside_the_package_root() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let package_root = directory.0.join("package");
    std::fs::create_dir(&package_root).unwrap();
    let escaped_path = PathBuf::from("..").join(library.file_name().unwrap());
    let text = "plugin version=1 library=\"placeholder.so\"\nfunction \"host::probe\" result=\"any\" { param \"any\"; effect \"fs_write\"; }";

    let mut legacy_manifest = PluginManifest::parse(text).unwrap();
    legacy_manifest.library = escaped_path.clone();
    let error = unsafe {
        legacy_manifest.load_for_target(&mut Engine::new(), &package_root, "test-target")
    }
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("normalized path inside the package")
    );

    let mut target_manifest = PluginManifest::parse(text).unwrap();
    target_manifest
        .target_libraries
        .insert("test-target".into(), escaped_path);
    let error = unsafe {
        target_manifest.load_for_target(&mut Engine::new(), &package_root, "test-target")
    }
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("normalized path inside the package")
    );
}

#[cfg(any(unix, windows))]
fn c_plugin(directory: &Directory, flags: &[&str]) -> PathBuf {
    let output = directory.library("c_plugin");
    let mut compiler = Command::new(if cfg!(windows) { "cl.exe" } else { "cc" });
    compiler.current_dir(&directory.0);
    let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    if cfg!(windows) {
        compiler.args([
            "/nologo",
            "/LD",
            "/TC",
            "/std:c11",
            "/W4",
            "/WX",
            "/utf-8",
            "/D_CRT_SECURE_NO_WARNINGS",
        ]);
        for flag in flags {
            compiler.arg(format!(
                "/D{}",
                flag.strip_prefix("-D").expect("fixture define")
            ));
        }
        compiler.arg(format!("/I{}", include.display()));
        compiler.arg(format!("/Fe{}", output.display()));
        compiler.arg(format!(
            "/Fo{}",
            directory.0.join("native_plugin.obj").display()
        ));
    } else {
        compiler.args(["-std=c11", "-Wall", "-Wextra", "-Werror"]);
        if cfg!(target_os = "macos") {
            compiler.arg("-dynamiclib");
        } else {
            compiler.args(["-shared", "-fPIC"]);
        }
        compiler
            .args(flags)
            .arg("-I")
            .arg(include)
            .arg("-o")
            .arg(&output);
    }
    compiler.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_plugin.c"));
    check(&mut compiler);
    output
}

#[cfg(any(unix, windows))]
#[test]
fn c_plugin_validates_handles_and_pins_state_and_library_until_program_drops() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let marker = directory.0.join("destroyed");
    let mut engine = Engine::new();
    unsafe { engine.load_plugin(&library) }.unwrap();
    let source = format!(
        "@!include(\"std\") use io; use host; use err; fn main() {{ host::probe({}); host::probe(-4); io::println(host::probe(1)); try {{ host::probe(-1); }} catch(e) {{ io::println(e.error_kind()); }} io::println(host::probe(-2)); try {{ host::probe(0); }} catch(e) {{ io::println(e.error_kind()); }} try {{ host::probe(-3); }} catch(e) {{ io::println(e.error_kind()); }} }}",
        serde_json::to_string(marker.to_str().unwrap()).unwrap()
    );
    let program = engine
        .decode(&engine.compile("c.cog", &source).unwrap().encode().unwrap())
        .unwrap();
    drop(engine);
    assert!(!marker.exists());
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(
        output,
        b"1\nNativeFailure\n2\nNativeFailure\nNativeFailure\n"
    );
    assert!(!marker.exists(), "program retains the library's state");
    drop(program);
    assert_eq!(std::fs::read(marker).unwrap(), b"destroyed");
}

#[cfg(any(unix, windows))]
#[test]
fn incompatible_descriptors_and_manifest_mismatch_leave_engine_unchanged() {
    for flag in [
        "-DTEST_PLUGIN_VERSION=2",
        "-DTEST_PLUGIN_SIZE=4",
        "-DTEST_FUNCTION_SIZE=4",
    ] {
        let directory = Directory::new();
        let library = c_plugin(&directory, &[flag]);
        assert!(unsafe { Engine::new().load_plugin(&library) }.is_err());
    }
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let library_name = library.file_name().unwrap().to_str().unwrap();
    let manifest = PluginManifest::parse(&format!("plugin version=1 library={}\nfunction \"host::probe\" result=\"int\" {{ param \"any\"; effect \"fs_write\"; }}", serde_json::to_string(library_name).unwrap())).unwrap();
    let mut engine = Engine::new();
    assert!(unsafe { manifest.load(&mut engine, &directory.0) }.is_err());
    engine
        .register(
            "host::probe",
            Signature::new(vec![Type::Int], Type::Int),
            |_, args| Ok(args[0].clone()),
        )
        .unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn failed_plugin_create_destroys_partial_state_without_registering_functions() {
    let directory = Directory::new();
    let marker = directory.0.join("destroyed-after-create-failure");
    let marker_argument = format!(
        "-DTEST_CREATE_MARKER={}",
        serde_json::to_string(marker.to_str().unwrap()).unwrap()
    );
    let library = c_plugin(&directory, &["-DTEST_CREATE_FAILURE", &marker_argument]);
    let mut engine = Engine::new();

    let error = unsafe { engine.load_plugin(&library) }.unwrap_err();
    assert!(error.message().contains("plugin initialization failed"));
    assert_eq!(std::fs::read(marker).unwrap(), b"destroyed");
    assert!(
        engine
            .compile(
                "failed-plugin.cog",
                "@!include(\"std\") use host; fn main() { host::probe(1); }",
            )
            .is_err()
    );
}

#[test]
fn cli_check_and_build_use_metadata_without_loading_native_code() {
    let directory = Directory::new();
    let manifest = directory.0.join("plugin.kdl");
    let source = directory.0.join("main.cog");
    let output = directory.0.join("program");
    std::fs::write(&manifest, "plugin version=1 library=\"does-not-exist\"\nfunction \"host::double\" result=\"int\" { param \"int\"; }").unwrap();
    std::fs::write(
        &source,
        "@!include(\"std\") use host; fn main() { host::double(21); }",
    )
    .unwrap();
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest),
    );
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest)
            .arg("-o")
            .arg(&output),
    );
    assert!(output.is_file());
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&output)
            .arg("--plugin")
            .arg(&manifest),
    );
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&output)
            .arg("--plugin")
            .arg(&manifest)
            .arg("-o")
            .arg(directory.0.join("copy")),
    );
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&output)
        .arg("--plugin")
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does-not-exist"));
}

#[test]
fn retained_resource_values_do_not_revive_resources_in_later_contexts() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let directory = Directory::new();
    let saved = Rc::new(RefCell::new(Value::nil()));
    let keep = saved.clone();
    let mut engine = Engine::new();
    engine
        .register(
            "host::keep",
            Signature::new(vec![Type::Handle], Type::Nil),
            move |_, arguments| {
                *keep.borrow_mut() = arguments[0].clone();
                Ok(Value::nil())
            },
        )
        .unwrap();
    engine
        .register(
            "host::restore",
            Signature::new(vec![], Type::Handle),
            move |_, _| Ok(saved.borrow().clone()),
        )
        .unwrap();
    let source = format!(
        "@!include(\"std\") use host; use io; fn main() {{ host::keep(io::open({}, \"w\", 0))!!; }}",
        serde_json::to_string(directory.0.join("file").to_str().unwrap()).unwrap()
    );
    engine
        .compile("open.cog", &source)
        .unwrap()
        .run(&mut Vec::new())
        .unwrap();
    let source = "@!include(\"std\") use host; use io; use err; fn main() { try { io::flush(host::restore()); } catch(e) { io::println(e.error_kind()); } }";
    let mut output = Vec::new();
    engine
        .compile("stale.cog", source)
        .unwrap()
        .run(&mut output)
        .unwrap();
    assert_eq!(output, b"InvalidResource\n");
}

#[test]
fn build_cache_includes_registered_native_metadata() {
    let directory = Directory::new();
    let source = directory.0.join("main.cog");
    let artifact = directory.0.join("main.cogb");
    std::fs::write(&source, "fn main() {}").unwrap();
    let first = Engine::new();
    assert!(
        !first
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    assert!(
        first
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    let mut second = Engine::new();
    second
        .register_metadata("host::added", Signature::new(vec![], Type::Nil))
        .unwrap();
    assert!(
        !second
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    assert!(
        second
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
}

#[cfg(any(unix, windows))]
#[test]
fn cli_links_and_executes_explicit_plugins_from_source_and_bytecode() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let manifest = directory.0.join("plugin.kdl");
    let source = directory.0.join("main.cog");
    let artifact = directory.0.join("program.cogb");
    let library_name = library.file_name().unwrap().to_str().unwrap();
    std::fs::write(&manifest, format!("plugin version=1 library={}\nfunction \"host::probe\" result=\"any\" {{ param \"any\"; effect \"fs_write\"; }}", serde_json::to_string(library_name).unwrap())).unwrap();
    std::fs::write(
        &source,
        "@!include(\"std\") use host; use io; fn main() { io::println(host::probe(1)); }",
    )
    .unwrap();
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest)
            .arg("-o")
            .arg(&artifact),
    );
    for path in [&source, &artifact] {
        let result = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .arg(path)
            .arg("--plugin")
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"1\n");
    }
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .current_dir(&directory.0)
        .args(["run", "program.cogb", "--plugin", "plugin.kdl"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"1\n");
}

#[cfg(any(unix, windows))]
#[test]
fn package_native_plugins_are_discovered_without_plugin_manifest_or_cli_flag() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let library_name = library.file_name().unwrap().to_str().unwrap();
    let package_manifest = |library: &str| {
        format!(
            "package \"auto-plugin-app\" version=\"0.1.0\" {{\n    plugin version=1 library={} {{\n        function \"host::probe\" result=\"any\" {{ param \"any\"; effect \"fs_write\"; }}\n    }}\n}}\n",
            serde_json::to_string(library).unwrap()
        )
    };
    let manifest_path = directory.0.join("package.kdl");
    std::fs::write(&manifest_path, package_manifest("native/does-not-exist.so")).unwrap();
    std::fs::create_dir_all(directory.0.join("src")).unwrap();
    std::fs::write(
        directory.0.join("src/main.cog"),
        "@!include(\"std\") use host; use io; fn main() { io::println(host::probe(1)); }",
    )
    .unwrap();

    let restricted = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&directory.0)
        .arg("--restricted")
        .output()
        .unwrap();
    assert!(!restricted.status.success());
    assert!(
        String::from_utf8_lossy(&restricted.stderr)
            .contains("--restricted cannot be combined with native plugins")
    );

    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&directory.0),
    );
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&directory.0),
    );
    std::fs::write(&manifest_path, package_manifest(library_name)).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&directory.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"1\n");
}

#[cfg(any(unix, windows))]
#[test]
fn packaged_application_loads_only_the_exact_host_plugin_target() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let package = directory.0.join("portable-app");
    std::fs::create_dir_all(package.join("native")).unwrap();
    let host_target = if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "windows",
        target_env = "msvc"
    )) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else {
        panic!("test host is outside the packaged plugin target matrix")
    };
    let paths = [
        ("x86_64-unknown-linux-gnu", "native/linux.so"),
        ("x86_64-pc-windows-msvc", "native/windows.dll"),
        ("aarch64-apple-darwin", "native/macos.dylib"),
    ];
    for (target, relative) in paths {
        let path = package.join(relative);
        if target == host_target {
            std::fs::copy(&library, &path).unwrap();
        } else {
            std::fs::write(path, b"wrong-target library; must not be loaded").unwrap();
        }
    }
    std::fs::write(
        package.join("package.kdl"),
        r#"package "portable-plugin-app" version="1" {
    plugin version=1 {
        library target="x86_64-unknown-linux-gnu" path="native/linux.so"
        library target="x86_64-pc-windows-msvc" path="native/windows.dll"
        library target="aarch64-apple-darwin" path="native/macos.dylib"
        function "host::probe" result="any" { param "any"; effect "fs_write"; }
    }
}
"#,
    )
    .unwrap();
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(
        package.join("src/main.cog"),
        "@!include(\"std\") use io; use host; fn main() { io::println(host::probe(1)); }\n",
    )
    .unwrap();
    let archive = package.join("target/portable.cogpkg");
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(["package", "build"])
            .arg(&package)
            .arg("-o")
            .arg(&archive),
    );
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .args(["run"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"1\n");
}

#[cfg(any(unix, windows))]
#[test]
fn archived_library_plugin_checks_without_loading_and_runs_from_its_package() {
    let workspace = Directory::new();
    let library = c_plugin(&workspace, &[]);
    let plugin_bytes = std::fs::read(&library).unwrap();
    let plugin_name = library.file_name().unwrap().to_string_lossy().into_owned();

    let package_root = workspace.0.join("library");
    let package_native = package_root.join("native").join(&plugin_name);
    std::fs::create_dir_all(package_native.parent().unwrap()).unwrap();
    std::fs::copy(&library, &package_native).unwrap();
    let relative_plugin = format!("native/{plugin_name}");
    let encoded_plugin = serde_json::to_string(&relative_plugin).unwrap();
    std::fs::write(
        package_root.join("package.kdl"),
        format!(
            "package \"cog-native-library\" version=\"0.1.0\" kind=\"library\" {{\n    plugin version=1 library={encoded_plugin} {{\n        function \"host::probe\" result=\"any\" {{ param \"any\"; effect \"fs_write\"; }}\n    }}\n}}\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(package_root.join("src")).unwrap();
    std::fs::write(
        package_root.join("src/lib.cog"),
        "@!include(\"std\") use probe;\n",
    )
    .unwrap();
    std::fs::write(
        package_root.join("src/probe.cog"),
        "use host; public fn call(value) { host::probe(value) }\n",
    )
    .unwrap();

    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("package")
            .arg("build")
            .arg(&package_root),
    );
    let archive = package_root
        .join("target")
        .join("package-cog-native-library.cogpkg");
    assert!(archive.is_file());

    let app = workspace.0.join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::create_dir_all(app.join("vendor")).unwrap();
    let archived_dependency = app.join("vendor/native.cogpkg");
    std::fs::copy(&archive, &archived_dependency).unwrap();
    std::fs::write(
        app.join("package.kdl"),
        "package \"plugin-consumer\" version=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("dependencies.kdl"),
        "native archive=\"vendor/native.cogpkg\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("src/main.cog"),
        "@!include(\"std\") use io; use probe from native; fn main() { io::println(probe::call(41))!!; }\n",
    )
    .unwrap();

    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&app),
    );
    let cached_package = std::fs::read_dir(app.join("target/.cog-package-cache"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let cached_plugin = cached_package.join(&relative_plugin);
    std::fs::remove_file(&cached_plugin).unwrap();
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&app)
            .arg("--locked"),
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&app)
        .arg("--locked")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains(&plugin_name),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );

    std::fs::write(&cached_plugin, &plugin_bytes).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&app)
        .arg("--locked")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"1\n");
}

#[test]
fn library_package_native_metadata_checks_without_main_or_loading_library() {
    let directory = Directory::new();
    std::fs::write(
        directory.0.join("package.kdl"),
        "package \"native-library\" version=\"0.1.0\" kind=\"library\" {\n    plugin version=1 library=\"missing.so\" {\n        function \"host::probe\" result=\"any\" { param \"any\"; }\n    }\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(directory.0.join("src")).unwrap();
    std::fs::write(
        directory.0.join("src/lib.cog"),
        "@!include(\"std\") use host; public fn call() { host::probe(1); }",
    )
    .unwrap();

    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&directory.0),
    );
    assert!(!directory.0.join("target/native-library.cogb").exists());
}
