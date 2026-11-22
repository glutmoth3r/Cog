//! Command-line orchestration for checking, building, packaging, and running Cog.
//!
//! Project discovery and bytecode validation happen before execution. Native
//! plugins are loaded only on run paths and are treated as trusted process code;
//! command presentation is kept in `cli_output`.
use cog::backend::CompiledProgram;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{build, bytecode, config, frontend, interpreter, parser, project};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

mod cli_output;
use cli_output::Color;

#[allow(dead_code)]
const NAME: &str = env!("CARGO_PKG_NAME");
#[allow(dead_code)]
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Run,
    Build,
}

#[derive(Default)]
struct Options {
    debug: bool,
    jit: bool,
    native: bool,
    exports: Vec<String>,
    c_imports: Vec<String>,
    c_objects: Vec<PathBuf>,
    allow_asm: bool,
    restricted: bool,
    target: Option<String>,
    use_modules: Vec<String>,
    locked: bool,
    output: Option<PathBuf>,
    install_path: Option<PathBuf>,
    init_library: bool,
    program_args: Vec<OsString>,
    program_args_separator: bool,
    plugins: Vec<PathBuf>,
}

fn main() {
    let mut color = Color::default();
    if let Err(error) = command(&mut color) {
        color.error(&error);
        std::process::exit(1);
    }
}

fn command(color: &mut Color) -> Result<(), String> {
    let mut options = Options::default();
    let mut action: Option<String> = None;
    let mut target: Option<OsString> = None;
    let mut bytecode_input = false;
    let mut package_build = false;
    let mut args = std::env::args_os().skip(1).peekable();
    while let Some(argument) = args.next() {
        if argument == OsStr::new("--") {
            options.program_args_separator = true;
            options.program_args = args.collect();
            break;
        }
        let cli_word = argument.to_str().is_some_and(|word| {
            word.starts_with('-')
                || (action.is_none()
                    && matches!(
                        word,
                        "init" | "check" | "run" | "build" | "eval" | "package" | "install"
                    ))
        });
        if target.is_none()
            && action
                .as_deref()
                .is_none_or(|action| matches!(action, "check" | "run" | "build"))
            && !cli_word
            && is_bytecode_path(Path::new(&argument))
        {
            bytecode_input = true;
            target = Some(argument);
            if action.is_none() {
                // binfmt_misc invokes `cog <artifact> <arguments...>`. The
                // complete tail belongs to the program, including `--` and
                // names that are otherwise Cog options or subcommands.
                options.program_args = args.collect();
                break;
            }
            continue;
        }
        let arg = argument
            .into_string()
            .map_err(|_| "CLI options and source paths before `--` must be UTF-8".to_owned())?;
        match arg.as_str() {
            "--debug" | "-v" => options.debug = true,
            "--jit" => options.jit = true,
            "--native" => options.native = true,
            "--allow-asm" => options.allow_asm = true,
            "--restricted" => options.restricted = true,
            "--target" => options.target = Some(required_cli_arg(&mut args, "--target")?),
            "--use" => options
                .use_modules
                .push(required_cli_arg(&mut args, "--use")?),
            "--locked" => options.locked = true,
            "--lib" => options.init_library = true,
            "--export" => options
                .exports
                .push(required_cli_arg(&mut args, "--export")?),
            "--c-import" => options
                .c_imports
                .push(required_cli_arg(&mut args, "--c-import")?),
            "--c-link" => options.c_objects.push(
                args.next()
                    .ok_or("--c-link requires an object or archive path")?
                    .into(),
            ),
            "--plugin" => options.plugins.push(
                args.next()
                    .ok_or("--plugin requires a KDL manifest path")?
                    .into(),
            ),
            "--color" => *color = Color::parse(&required_cli_arg(&mut args, "--color")?)?,
            "--output" | "-o" => {
                options.output = Some(args.next().ok_or("--output requires a path")?.into())
            }
            "--path" => {
                if options.install_path.is_some() {
                    return Err("--path may only be specified once".into());
                }
                options.install_path = Some(required_cli_arg(&mut args, "--path")?.into());
            }
            "--version" | "-V" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            "init" | "check" | "run" | "build" | "eval" | "package" | "install"
                if action.is_none() =>
            {
                action = Some(arg)
            }
            "build" if action.as_deref() == Some("package") && !package_build => {
                package_build = true;
            }
            _ if action.as_deref() == Some("package")
                && !package_build
                && !arg.starts_with('-') =>
            {
                return Err(format!("unknown package command `{arg}`; expected `build`"));
            }
            _ if action.as_deref() == Some("package")
                && package_build
                && target.is_none()
                && !arg.starts_with('-') =>
            {
                target = Some(arg.into());
            }
            _ if arg.starts_with('-')
                && !(action.as_deref() == Some("eval") && target.is_none()) =>
            {
                return Err(format!("unknown option `{arg}`"));
            }
            _ if target.is_none() => target = Some(arg.into()),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }
    if action.is_none() && target.is_none() {
        print_usage();
        return Err("expected a command or a source/bytecode path".into());
    }
    let action = action.as_deref().unwrap_or("run");
    if options.native && action != "build" {
        return Err("--native is only valid with `build`".into());
    }
    if options.jit && action != "run" {
        return Err("--jit is only valid with `run`".into());
    }
    if options.program_args_separator && action != "run" {
        return Err("program arguments after `--` are only valid with `run`".into());
    }
    if options.output.is_some() && action != "build" && action != "package" {
        return Err("--output is only valid with `build` or `package build`".into());
    }
    if options.install_path.is_some() && action != "install" {
        return Err("--path is only valid with `install`".into());
    }
    if options.init_library && !matches!(action, "init" | "build") {
        return Err("--lib is only valid with `init` or `build`".into());
    }
    let native_library = action == "build" && options.init_library;
    if native_library && options.native {
        return Err("choose either `build --native` or `build --lib`".into());
    }
    if (native_library || options.native) && options.restricted {
        return Err("--restricted is only available when running through the CLI".into());
    }
    if !options.c_imports.is_empty() || !options.c_objects.is_empty() {
        if !options.native {
            return Err("C imports require an unrestricted `build --native` executable".into());
        }
        if options.c_imports.is_empty() {
            return Err("--c-link requires an explicit --c-import".into());
        }
    }
    if !options.exports.is_empty() && !native_library {
        return Err("--export is only valid with `build --lib`".into());
    }
    if !options.use_modules.is_empty() && action != "eval" {
        return Err("--use is only valid with `eval`".into());
    }
    if action == "install" {
        let path = options
            .install_path
            .as_deref()
            .ok_or("usage: cog install --path <project-directory>")?;
        if target.is_some()
            || options.debug
            || options.allow_asm
            || options.restricted
            || options.target.is_some()
            || !options.use_modules.is_empty()
            || !options.plugins.is_empty()
            || options.program_args_separator
        {
            return Err("usage: cog install --path <project-directory>".into());
        }
        let started = Instant::now();
        let installed = cog::package::install_path(path, options.locked)?;
        if !installed.warnings.is_empty() {
            color.warning(&frontend::render(&installed.warnings));
        }
        color.status("installed", &installed.path, started.elapsed());
        eprintln!(
            "{} {} {} package, {} bytes",
            installed.package_name,
            installed.package_version,
            match installed.kind {
                config::PackageKind::Library => "library",
                config::PackageKind::Application => "application",
            },
            installed.archive_bytes
        );
        return Ok(());
    }
    if action == "package" {
        if !package_build {
            return Err(
                "usage: cog package build [project-directory] [--target <triple>] [-o archive.cogpkg]"
                    .into(),
            );
        }
        if !options.plugins.is_empty() {
            return Err(
                "--plugin is not valid with `package build`; declare plugins in package.kdl".into(),
            );
        }
        if options.allow_asm {
            return Err("--allow-asm is not valid with `package build`".into());
        }
        if options.restricted || options.debug {
            return Err("--restricted and --debug are not valid with `package build`".into());
        }
        let project_path = Path::new(target.as_deref().unwrap_or(OsStr::new(".")));
        let started = Instant::now();
        let result = if let Some(target) = options.target.as_deref() {
            cog::package::build_archive_for_target(
                project_path,
                options.output.as_deref(),
                options.locked,
                target,
            )?
        } else {
            cog::package::build_archive(project_path, options.output.as_deref(), options.locked)?
        };
        if !result.warnings.is_empty() {
            color.warning(&frontend::render(&result.warnings));
        }
        color.status("packed", &result.path, started.elapsed());
        eprintln!("{} entries, {} bytes", result.entries, result.archive_bytes);
        return Ok(());
    }
    if action == "eval" {
        if !options.plugins.is_empty() {
            return Err("--plugin is not supported with `eval`".into());
        }
        if options.target.is_some() || options.locked || options.allow_asm {
            return Err("--target, --locked, and --allow-asm are not valid with `eval`".into());
        }
        let expression = target
            .as_deref()
            .and_then(OsStr::to_str)
            .filter(|expression| !expression.trim().is_empty())
            .ok_or("eval requires one Cog expression; usage: cog eval [options] <expression>")?;
        return eval_expression(expression, &options, color);
    }
    if action == "init" {
        if !options.plugins.is_empty() {
            return Err("--plugin is only valid with check, build or run".into());
        }
        let directory = target
            .as_deref()
            .filter(|directory| !directory.is_empty())
            .ok_or("init requires a directory; usage: cog init <directory>")?;
        return init_project(Path::new(directory), options.init_library);
    }
    let target = Path::new(target.as_deref().unwrap_or(OsStr::new(".")));
    let mode = match action {
        "check" => Mode::Check,
        "build" => Mode::Build,
        _ => Mode::Run,
    };
    if target.extension() == Some(OsStr::new("cogpkg")) {
        if mode != Mode::Run {
            return Err(".cogpkg application archives can only be run".into());
        }
        return command_with_application(target, options, color);
    }
    let bytecode_input = bytecode_input || is_bytecode_path(target);
    let started = Instant::now();
    let source_input = !bytecode_input
        && (target.is_dir() || target.extension().and_then(OsStr::to_str) == Some("cog"));
    let prepared = if source_input {
        Some(
            project::prepare(target, options.target.as_deref(), options.locked).map_err(
                |errors| {
                    let action = if mode == Mode::Check {
                        "check"
                    } else {
                        "build"
                    };
                    cli_output::preparation_error(
                        frontend::render(&errors),
                        action,
                        target,
                        started.elapsed(),
                    )
                },
            )?,
        )
    } else {
        None
    };
    let package_has_plugins = prepared
        .as_ref()
        .is_some_and(|prepared| !prepared.plugins.is_empty());
    if options.native || native_library {
        if package_has_plugins || !options.plugins.is_empty() {
            return Err("AOT builds do not support plugin imports yet".into());
        }
        let name = prepared
            .as_ref()
            .map(|p| p.artifact_name.as_str())
            .or_else(|| target.file_stem().and_then(OsStr::to_str))
            .unwrap_or("main");
        let destination = options.output.clone().unwrap_or_else(|| {
            let base = if target.is_dir() {
                target
            } else {
                target.parent().unwrap_or(Path::new("."))
            };
            let file = if native_library {
                format!("lib{name}.so")
            } else {
                name.to_owned()
            };
            base.join("target").join(file)
        });
        let lock_update = prepared.as_ref().and_then(|p| p.lock_update.clone());
        let (report, warnings) = if !options.c_imports.is_empty() {
            cog::native::build_executable_with_c_imports(
                prepared.ok_or("C imports require Cog source or an application project")?,
                &destination,
                &options.c_imports,
                &options.c_objects,
            )?
        } else if native_library {
            let loaded = prepared
                .ok_or("native library builds require a Cog source or library project")?
                .resolve();
            let (compiled, warnings) = project::compile_library_with_inputs_and_warnings(
                loaded.program,
                loaded.diagnostics,
                &loaded.names,
                &loaded.build_inputs,
            )?;
            (
                cog::native::build_library(&compiled, &destination, &options.exports)?,
                warnings,
            )
        } else {
            let (compiled, _, _, warnings) =
                prepare(target, Mode::Check, &options, bytecode_input, prepared)?;
            let compiled =
                compiled.ok_or("AOT executable builds require an application entry point")?;
            (
                cog::native::build_executable(&compiled, &destination)?,
                warnings,
            )
        };
        if !warnings.is_empty() {
            color.warning(&frontend::render(&warnings));
        }
        project::write_lock(lock_update)?;
        color.status("linked", &destination, started.elapsed());
        eprintln!(
            "AOT: {} functions, {} machine code bytes, {:?} code generation",
            report.functions, report.code_bytes, report.codegen_time
        );
        return Ok(());
    }
    if !options.plugins.is_empty() || package_has_plugins {
        return command_with_plugins(target, mode, options, color, prepared);
    }
    let (compiled, reused, destination, warnings) =
        prepare(target, mode, &options, bytecode_input, prepared).map_err(|error| {
            let action = if bytecode_input {
                "load"
            } else if mode == Mode::Check {
                "check"
            } else {
                "build"
            };
            cli_output::preparation_error(error, action, target, started.elapsed())
        })?;
    let elapsed = started.elapsed();
    if !warnings.is_empty() {
        color.warning(&frontend::render(&warnings));
    }
    if options.debug
        && let Some(compiled) = &compiled
    {
        println!("bytecode\n{}", compiled.disassemble());
    }
    match mode {
        Mode::Check => {
            color.status("checked", target, elapsed);
            Ok(())
        }
        Mode::Run => {
            if !bytecode_input && !reused {
                color.status("built", &destination, elapsed);
            }
            let capabilities = if options.restricted {
                VmCapabilities {
                    allow_print: true,
                    allow_read: true,
                    ..VmCapabilities::restricted()
                }
            } else {
                VmCapabilities::default()
            };
            let mut stdout = std::io::stdout();
            let mut context = RuntimeContext::new(&mut stdout, capabilities);
            context.configure_jit(options.jit, Some(&jit_profile_path(target)), options.debug);
            context.set_process_arguments(options.program_args);
            interpreter::run_with_context(
                compiled
                    .as_ref()
                    .ok_or("internal error: run has no compiled program")?,
                &mut context,
            )
            .map_err(|error| format_runtime_error(&error, context.last_source_location()))
        }
        Mode::Build => {
            color.status(
                if reused { "finished" } else { "built" },
                &destination,
                elapsed,
            );
            Ok(())
        }
    }
}

fn eval_expression(expression: &str, options: &Options, color: &Color) -> Result<(), String> {
    let mut source = String::from("@!include(\"std\")\nuse io;\n");
    let mut imports: Vec<String> = Vec::new();
    for module in &options.use_modules {
        if !is_identifier(module) {
            return Err(format!(
                "--use expects a standard-library module name, got `{module}`"
            ));
        }
        if module != "io" && !imports.contains(module) {
            source.push_str("use ");
            source.push_str(module);
            source.push_str(";\n");
            imports.push(module.clone());
        }
    }
    source.push_str("\nfn main() { io::println((");
    source.push_str(expression);
    source.push_str("))!!; }\n");

    let (compiled, warnings) = project::compile_source_with_warnings("eval.cog", &source)?;
    if !warnings.is_empty() {
        color.warning(&frontend::render(&warnings));
    }
    if options.debug {
        println!("bytecode\n{}", compiled.disassemble());
    }
    let capabilities = if options.restricted {
        VmCapabilities {
            allow_print: true,
            allow_read: true,
            ..VmCapabilities::restricted()
        }
    } else {
        VmCapabilities::default()
    };
    let mut stdout = std::io::stdout();
    let mut context = RuntimeContext::new(&mut stdout, capabilities);
    interpreter::run_with_context(&compiled, &mut context)
        .map_err(|error| format_runtime_error(&error, context.last_source_location()))
}

fn format_runtime_error(
    error: &impl std::fmt::Display,
    location: Option<&cog::diagnostic::SourceLocation>,
) -> String {
    if let Some(location) = location {
        format!(
            "runtime error at {}:{}:{}: {error}",
            location.file.escape_debug(),
            location.line,
            location.column
        )
    } else {
        format!("runtime error: {error}")
    }
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn prepare(
    target: &Path,
    mode: Mode,
    options: &Options,
    bytecode_input: bool,
    prepared: Option<project::Prepared>,
) -> Result<
    (
        Option<CompiledProgram>,
        bool,
        PathBuf,
        Vec<cog::diagnostic::Diagnostic>,
    ),
    String,
> {
    let prepared = if prepared.is_some() {
        prepared
    } else if !bytecode_input
        && (target.is_dir() || target.extension().and_then(|s| s.to_str()) == Some("cog"))
    {
        Some(
            project::prepare(target, options.target.as_deref(), options.locked)
                .map_err(|errors| frontend::render(&errors))?,
        )
    } else {
        None
    };
    let artifact_name = prepared
        .as_ref()
        .map(|p| p.artifact_name.as_str())
        .unwrap_or("main");
    let destination = options.output.clone().unwrap_or_else(|| {
        if target.is_dir() {
            target.join(format!("target/{artifact_name}.cogb"))
        } else {
            target.with_extension("cogb")
        }
    });
    let (compiled, reused, warnings) = if let Some(prepared) = prepared {
        if prepared.kind == config::PackageKind::Library {
            if mode != Mode::Check {
                return Err("library packages can be checked but not run or built yet".into());
            }
            let loaded = prepared.resolve();
            let warnings = project::check_with_inputs_and_warnings(
                loaded.program,
                loaded.diagnostics,
                &loaded.names,
                &loaded.build_inputs,
            )?;
            (None, false, warnings)
        } else if mode == Mode::Check {
            let loaded = prepared.resolve();
            let (compiled, warnings) = project::compile_with_inputs_and_warnings(
                loaded.program,
                loaded.diagnostics,
                &loaded.names,
                &loaded.build_inputs,
            )?;
            (Some(compiled), false, warnings)
        } else {
            let (built, warnings) = build::project_with_warnings(prepared, &destination)?;
            (Some(built.program), built.reused, warnings)
        }
    } else {
        let mut compiled = compile_file(target, options, bytecode_input)?;
        if mode == Mode::Build || (mode == Mode::Run && !bytecode_input) {
            build::write_bytecode(&destination, &compiled)?;
            if mode == Mode::Run {
                compiled = load_bytecode(&destination)?;
            }
        }
        (Some(compiled), false, Vec::new())
    };
    Ok((compiled, reused, destination, warnings))
}

fn print_usage() {
    eprintln!("usage: cog [check|run|build] [path] [options] [-- program-arguments...]");
    eprintln!(
        "       cog package build [project-directory] [--target <triple>] [-o archive.cogpkg]"
    );
    eprintln!("       cog install --path <project-directory> [--locked]");
    eprintln!("       cog eval [--debug] [--restricted] [--use <module>]... <expression>");
    eprintln!("       cog <bytecode-file> [program-arguments...]");
    eprintln!("       cog --version|-V");
    eprintln!("       cog build <file.cog|project> [-o output]");
    eprintln!("       cog run [file.cog|project]  (reuse or build .cogb, then execute it)");
    eprintln!("       cog run <file.cogb>  (no source files or compilation required)");
    eprintln!("       cog build --native <file.cog|project> [-o executable]");
    eprintln!("       cog build --lib <file.cog|project> --export <function> [-o library.so]");
    eprintln!("       --c-import <module::name=symbol:i64|f64> --c-link <object.o|archive.a>");
    eprintln!("       --jit  enable experimental adaptive Cranelift JIT (run only)");
    eprintln!("       cog init [--lib] <directory>");
    eprintln!("       eval compiles and runs one expression; --debug prints its bytecode");
    eprintln!("       bytecode is recognized by its COG header regardless of the filename");
    eprintln!("       --allow-asm  enable the standalone assembler for VM diagnostics");
    eprintln!("       --restricted  allow only stdin/stdout runtime host effects");
    eprintln!(
        "       --plugin <manifest.kdl>  explicit trusted plugin for standalone sources/bytecode (repeatable)"
    );
    eprintln!("       project plugins are declared in package.kdl and discovered automatically");
    eprintln!("       `--` passes the remaining arguments to process::args() during `run`");
    eprintln!("       --version, -V  print the Cog version");
    eprintln!("       --color <auto|always|never>  diagnostic and status colors (default: auto)");
}

fn command_with_plugins(
    target: &Path,
    mode: Mode,
    options: Options,
    color: &Color,
    prepared: Option<project::Prepared>,
) -> Result<(), String> {
    use cog::sdk::{Engine, Permissions, PluginManifest, RunContext};
    if mode == Mode::Run
        && options.restricted
        && (!options.plugins.is_empty()
            || prepared
                .as_ref()
                .is_some_and(|prepared| !prepared.plugins.is_empty()))
    {
        return Err(
            "--restricted cannot be combined with native plugins; it cannot constrain trusted native code"
                .into(),
        );
    }
    let started = Instant::now();
    let mut engine = Engine::new();
    if let Some(prepared) = &prepared {
        if prepared.kind == config::PackageKind::Library && mode != Mode::Check {
            return Err("library packages can be checked but not run or built yet".into());
        }
        for (package_root, manifest) in &prepared.plugins {
            if mode == Mode::Run {
                // SAFETY: the selected package plugin runs only because the user
                // requested `run`, and its library must be trusted native code.
                // Descriptor checks establish ABI compatibility, not memory safety.
                unsafe {
                    manifest.load_for_target(&mut engine, package_root, host_plugin_target()?)
                }
                .map_err(|error| error.to_string())?;
            } else {
                manifest
                    .register_metadata(&mut engine)
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    for path in &options.plugins {
        let manifest = PluginManifest::read(path).map_err(|error| error.to_string())?;
        if mode == Mode::Run {
            // SAFETY: `--plugin` is explicit authorization to execute this trusted
            // native library with process authority. Check/build never load it;
            // descriptor validation cannot make an untrusted library safe.
            unsafe { manifest.load(&mut engine, path.parent().unwrap_or(Path::new("."))) }
                .map_err(|error| error.to_string())?;
        } else {
            manifest
                .register_metadata(&mut engine)
                .map_err(|error| error.to_string())?;
        }
    }
    let bytecode_input = is_bytecode_path(target);
    let (program, reused, destination) = if bytecode_input {
        let file = std::fs::File::open(target).map_err(|error| error.to_string())?;
        let program = engine.read(file).map_err(|error| error.to_string())?;
        let destination = options
            .output
            .clone()
            .unwrap_or_else(|| target.to_path_buf());
        if mode == Mode::Build {
            program
                .write(&destination)
                .map_err(|error| error.to_string())?;
        }
        (program, true, destination)
    } else {
        if !target.is_dir() && target.extension() != Some(OsStr::new("cog")) {
            return Err("plugins require a .cog source, project directory or bytecode file".into());
        }
        let prepared = prepared.ok_or_else(|| "project preparation is missing".to_owned())?;
        let destination = options.output.clone().unwrap_or_else(|| {
            if target.is_dir() {
                target.join(format!("target/{}.cogb", prepared.artifact_name))
            } else {
                target.with_extension("cogb")
            }
        });
        if prepared.kind == config::PackageKind::Library {
            let warnings = engine
                .check_library_prepared(prepared)
                .map_err(|error| error.to_string())?;
            if !warnings.is_empty() {
                color.warning(&frontend::render(&warnings));
            }
            color.status("checked", target, started.elapsed());
            return Ok(());
        }
        if mode == Mode::Check {
            (
                engine
                    .compile_prepared(prepared)
                    .map_err(|error| error.to_string())?,
                false,
                destination,
            )
        } else {
            let built = engine
                .build_prepared(prepared, &destination)
                .map_err(|error| error.to_string())?;
            (built.program, built.reused, destination)
        }
    };
    if options.debug {
        println!("bytecode\n{}", program.disassemble());
    }
    match mode {
        Mode::Check => {
            color.status("checked", target, started.elapsed());
            Ok(())
        }
        Mode::Build => {
            color.status(
                if reused { "finished" } else { "built" },
                &destination,
                started.elapsed(),
            );
            Ok(())
        }
        Mode::Run => {
            if !bytecode_input && !reused {
                color.status("built", &destination, started.elapsed());
            }
            let mut stdout = std::io::stdout();
            let mut context = RunContext::new(&mut stdout)
                .with_jit(options.jit)
                .with_jit_debug(options.debug);
            if options.jit {
                context = context.with_jit_profile_path(jit_profile_path(target));
            }
            if options.restricted {
                context = context.with_permissions(
                    Permissions::none().allow(cog::sdk::Effects::OUTPUT | cog::sdk::Effects::INPUT),
                );
            }
            context.set_process_arguments(options.program_args);
            program
                .run_in(&mut context)
                .map_err(|error| format_runtime_error(&error, context.last_source_location()))
        }
    }
}

fn command_with_application(target: &Path, options: Options, color: &Color) -> Result<(), String> {
    use cog::sdk::{Engine, PluginManifest, RunContext};

    if options.restricted && !options.plugins.is_empty() {
        return Err(
            "--restricted cannot be combined with native plugins; it cannot constrain trusted native code"
                .into(),
        );
    }
    let started = Instant::now();
    // Extraction validates the COGPKG 3 marker, package graph and bytecode
    // before any application or explicit plugin library is loaded.
    let application = cog::package::open_application(target)?;
    if options.restricted && !application.plugins.is_empty() {
        return Err(
            "--restricted cannot be combined with native plugins; it cannot constrain trusted native code"
                .into(),
        );
    }
    let mut engine = Engine::new();
    let mut manifests = Vec::new();
    for (root, manifest) in &application.plugins {
        manifests.push((root.clone(), manifest.clone()));
    }
    let mut external = Vec::new();
    for path in &options.plugins {
        let manifest = PluginManifest::read(path).map_err(|error| error.to_string())?;
        external.push((
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            manifest,
        ));
    }
    let host_target = if manifests.is_empty() {
        None
    } else {
        Some(host_plugin_target()?)
    };
    // Load only after archive and bytecode validation has completed.
    for (root, manifest) in manifests {
        // SAFETY: invoking the application explicitly requests its declared native
        // plugin. The selected library must be trusted; package digests do not
        // authenticate publishers and ABI checks do not prove pointer validity.
        unsafe {
            manifest.load_for_target(
                &mut engine,
                &root,
                host_target.expect("plugin target is set when manifests are present"),
            )
        }
        .map_err(|error| error.to_string())?;
    }
    for (root, manifest) in external {
        // SAFETY: this manifest was explicitly supplied for the requested run;
        // its library must obey the trusted C ABI v3 contract.
        unsafe { manifest.load(&mut engine, &root) }.map_err(|error| error.to_string())?;
    }
    let program = engine
        .decode(&application.bytecode)
        .map_err(|error| error.to_string())?;
    if options.debug {
        println!("bytecode\n{}", program.disassemble());
    }
    color.status("loaded", target, started.elapsed());
    let mut stdout = std::io::stdout();
    let mut context = RunContext::new(&mut stdout)
        .with_jit(options.jit)
        .with_jit_debug(options.debug);
    if options.jit {
        context = context.with_jit_profile_path(jit_profile_path(target));
    }
    context.set_process_arguments(options.program_args);
    program
        .run_in(&mut context)
        .map_err(|error| format_runtime_error(&error, context.last_source_location()))
}

fn jit_profile_path(target: &Path) -> PathBuf {
    let root = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or_else(|| Path::new("."))
    };
    root.join("target").join(".cog-ape-v2.json")
}

fn host_plugin_target() -> Result<&'static str, String> {
    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
    return Ok("x86_64-unknown-linux-gnu");
    #[cfg(all(target_arch = "x86_64", target_os = "windows", target_env = "msvc"))]
    return Ok("x86_64-pc-windows-msvc");
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return Ok("aarch64-apple-darwin");
    #[allow(unreachable_code)]
    Err("native plugins are not available for this host target".into())
}

fn required_cli_arg(
    args: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<String, String> {
    let argument = args
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?;
    argument
        .into_string()
        .map_err(|_| format!("the value for {option} must be UTF-8"))
}

fn init_project(root: &Path, library: bool) -> Result<(), String> {
    let mut existing_files = vec![
        "package.kdl",
        "dependencies.kdl",
        "build.kdl",
        "main.cog",
        "src/main.cog",
    ];
    if library {
        existing_files.extend(["src/lib.cog", "src/api.cog"]);
    }
    if existing_files.iter().any(|path| root.join(path).exists()) {
        return Err("init requires a new project; existing files were preserved".into());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create project: {e}"))?;
    let directory = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve project directory: {e}"))?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| config::valid_package_name(name))
        .ok_or("project directory name is not a valid UTF-8 package name")?;
    let mut package = kdl::KdlNode::new("package");
    package.push(name);
    package.insert("version", "0.1.0");
    if library {
        package.insert("kind", "library");
    }
    let manifest =
        include_str!("templates/package.kdl").replace("{{package}}", &package.to_string());
    std::fs::create_dir_all(root.join("src")).map_err(|e| format!("cannot create project: {e}"))?;
    let mut files = vec![
        ("package.kdl", manifest.as_str()),
        (
            "dependencies.kdl",
            include_str!("templates/dependencies.kdl"),
        ),
    ];
    if library {
        files.extend([
            ("src/lib.cog", "use api;\n"),
            ("src/api.cog", "public fn answer() -> int {\n    42\n}\n"),
        ]);
    } else {
        files.extend([
            ("build.kdl", include_str!("templates/build.kdl")),
            (
                "src/main.cog",
                "@!include(\"std\")\nuse io;\n\nfn main() -> nil {\n    io::println(\"Hello, Cog!\")!!;\n}\n",
            ),
        ]);
    }
    for (path, content) in files {
        use std::io::Write;
        let mut file = std::fs::File::create_new(root.join(path))
            .map_err(|e| format!("cannot create {path}: {e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("cannot write {path}: {e}"))?;
    }
    println!("created project at {}", root.display());
    Ok(())
}

fn is_bytecode_path(path: &Path) -> bool {
    if path.is_dir() {
        return false;
    }
    // Preserve bytecode diagnostics for corrupt or missing explicit .cogb files.
    if path.extension() == Some(OsStr::new("cogb")) {
        return true;
    }
    // Probe only regular files, reading a fixed-size signature. Actual loading
    // still uses the bounded decoder and rechecks the complete artifact.
    if !path.is_file() {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0; 4];
    file.read_exact(&mut magic).is_ok() && &magic == b"COG\0"
}

fn compile_file(
    path: &Path,
    options: &Options,
    bytecode_input: bool,
) -> Result<CompiledProgram, String> {
    let extension = path.extension().and_then(|s| s.to_str());
    if bytecode_input {
        return load_bytecode(path);
    }
    match extension {
        Some("asm") if options.allow_asm => {
            let bytes = cog::limits::read_source(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let source = String::from_utf8(bytes)
                .map_err(|_| format!("source {} is not valid UTF-8", path.display()))?;
            parser::assemble(&source).map_err(|e| format!("assembler error: {e}"))
        }
        Some("asm") => Err("standalone assembly requires --allow-asm".into()),
        _ => Err(format!(
            "expected a .cog source or a Cog bytecode file, got {}",
            path.display()
        )),
    }
}

fn load_bytecode(path: &Path) -> Result<CompiledProgram, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if file
        .metadata()
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
        .len()
        > bytecode::MAX_FILE_BYTES as u64
    {
        return Err("bytecode file exceeds the format limit".into());
    }
    bytecode::read(file)
}
