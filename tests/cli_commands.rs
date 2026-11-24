use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cog-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn path(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cog(home: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cog"));
    command.args(args);
    if let Some(home) = home {
        command.env("COG_HOME", home);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write_library(dir: &Dir) {
    dir.write(
        "lib/package.kdl",
        "package \"integer-math\" version=\"0.1.0\" kind=\"library\"\n",
    );
    dir.write("lib/src/lib.cog", "use integer_math;\n");
    dir.write(
        "lib/src/integer_math.cog",
        "public fn square(value: int) -> int {\n    value * value\n}\n",
    );
}

fn write_consumer(dir: &Dir, root: &str) {
    dir.write(
        &format!("{root}/package.kdl"),
        "package \"consumer\" version=\"0.1.0\"\n",
    );
    dir.write(
        &format!("{root}/dependencies.kdl"),
        "arithmetic package=\"integer-math\" version=\"0.1.0\"\n",
    );
    dir.write(
        &format!("{root}/src/main.cog"),
        "@!include(\"std\")\n@!include(\"arithmetic\")\nuse io;\nuse integer_math from arithmetic;\n\nfn main() {\n    io::println(integer_math::square(12));\n}\n",
    );
}

fn assert_rejected(output: &Output, fragment: &str) {
    assert!(!output.status.success(), "expected failure: {output:?}");
    assert!(
        stderr(output).contains(fragment),
        "stderr lacks `{fragment}`: {}",
        stderr(output)
    );
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

const LOOP_PROGRAM: &str = "@!include(\"std\")\nuse io;\n\nfn main() {\n    var sum = 0;\n    for i in 0..200 {\n        sum = sum + i;\n    }\n    io::println(sum);\n}\n";

#[test]
fn installed_library_is_usable_through_a_package_dependency() {
    let dir = Dir::new();
    write_library(&dir);
    write_consumer(&dir, "app");
    let home = dir.path("home");
    let app = dir.path("app");

    let missing = cog(Some(&home), &["check", path_str(&app)]);
    assert_rejected(&missing, "is not installed");

    let installed = cog(
        Some(&home),
        &["install", "--path", path_str(&dir.path("lib"))],
    );
    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(stderr(&installed).contains("integer-math 0.1.0 library package"));
    assert!(home.join("libraries").is_dir());

    let checked = cog(Some(&home), &["check", path_str(&app)]);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let ran = cog(Some(&home), &["run", path_str(&app)]);
    assert!(ran.status.success(), "{}", stderr(&ran));
    assert_eq!(stdout(&ran), "144\n");
}

#[test]
fn install_rejects_a_changed_library_with_the_same_version() {
    let dir = Dir::new();
    write_library(&dir);
    let home = dir.path("home");
    let lib = dir.path("lib");
    let args = ["install", "--path", path_str(&lib)];
    assert!(cog(Some(&home), &args).status.success());
    assert!(cog(Some(&home), &args).status.success());
    dir.write(
        "lib/src/integer_math.cog",
        "public fn square(value: int) -> int { value }\n",
    );
    assert_rejected(
        &cog(Some(&home), &args),
        "already installed with different contents",
    );
}

#[test]
fn install_requires_one_path_and_an_absolute_cog_home() {
    let dir = Dir::new();
    write_library(&dir);
    let home = dir.path("home");
    assert_rejected(
        &cog(Some(&home), &["install"]),
        "usage: cog install --path <project-directory>",
    );
    assert_rejected(
        &cog(Some(&home), &["install", "--path", "a", "--path", "b"]),
        "--path may only be specified once",
    );
    assert_rejected(
        &cog(Some(&home), &["check", "--path", "a"]),
        "--path is only valid with `install`",
    );
    assert_rejected(
        &cog(
            Some(Path::new("relative-home")),
            &["install", "--path", path_str(&dir.path("lib"))],
        ),
        "COG_HOME must be an absolute path",
    );
}

#[test]
fn locked_install_of_a_dependency_free_library_needs_no_lock_file() {
    let dir = Dir::new();
    write_library(&dir);
    let lib = dir.path("lib");
    let output = cog(
        Some(&dir.path("home")),
        &["install", "--locked", "--path", path_str(&lib)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!lib.join("cog.lock").exists());
}

#[test]
fn locked_commands_require_a_current_lock_file() {
    let dir = Dir::new();
    write_library(&dir);
    dir.write("app/package.kdl", "package \"app\" version=\"0.1.0\"\n");
    dir.write("app/dependencies.kdl", "dep path=\"../lib\"\n");
    dir.write("app/src/main.cog", "fn main() {}\n");
    let app = dir.path("app");
    let app = path_str(&app);

    let missing = cog(None, &["check", "--locked", app]);
    assert_rejected(&missing, "cog.lock");
    assert!(!dir.path("app/cog.lock").exists());

    assert!(cog(None, &["build", app]).status.success());
    assert!(dir.path("app/cog.lock").is_file());
    let current = cog(None, &["check", "--locked", app]);
    assert!(current.status.success(), "{}", stderr(&current));

    // A new dependency makes the recorded graph stale.
    dir.write("other/package.kdl", "package \"other\" version=\"0.1.0\"\n");
    dir.write("other/src/lib.cog", "public fn one() -> int { 1 }\n");
    dir.write(
        "app/dependencies.kdl",
        "dep path=\"../lib\"\nextra path=\"../other\"\n",
    );
    let before = std::fs::read_to_string(dir.path("app/cog.lock")).unwrap();
    let stale = cog(None, &["check", "--locked", app]);
    assert_rejected(
        &stale,
        "cog.lock is stale; rebuild without --locked to update resolution",
    );
    assert_eq!(
        std::fs::read_to_string(dir.path("app/cog.lock")).unwrap(),
        before
    );
}

#[test]
fn assembler_input_requires_allow_asm() {
    let asm = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test_loops.asm");
    let asm = path_str(&asm);
    for command in ["run", "check"] {
        assert_rejected(
            &cog(None, &[command, asm]),
            "standalone assembly requires --allow-asm",
        );
    }
    assert_rejected(
        &cog(None, &[asm]),
        "standalone assembly requires --allow-asm",
    );

    let checked = cog(None, &["check", "--allow-asm", asm]);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let ran = cog(None, &["run", "--allow-asm", asm]);
    assert!(ran.status.success(), "{}", stderr(&ran));
    assert_eq!(stdout(&ran), "108");
}

#[test]
fn flags_are_rejected_on_commands_where_they_are_invalid() {
    let dir = Dir::new();
    dir.write("main.cog", "fn main() {}\n");
    let main = dir.path("main.cog");
    let main = path_str(&main);
    let cases: &[(&[&str], &str)] = &[
        (
            &["run", "--lib", main],
            "--lib is only valid with `init` or `build`",
        ),
        (
            &["check", "--export", "f", main],
            "--export is only valid with `build --lib`",
        ),
        (
            &["build", "--export", "f", main],
            "--export is only valid with `build --lib`",
        ),
        (
            &["run", "--c-import", "h.h", main],
            "C imports require an unrestricted `build --native` executable",
        ),
        (
            &["build", "--c-import", "h.h", main],
            "C imports require an unrestricted `build --native` executable",
        ),
        (
            &["run", "--c-link", "x.o", main],
            "C imports require an unrestricted `build --native` executable",
        ),
        (
            &["build", "--native", "--c-link", "x.o", main],
            "--c-link requires an explicit --c-import",
        ),
        (
            &["build", "--lib", "--native", main],
            "choose either `build --native` or `build --lib`",
        ),
        (
            &["run", "--native", main],
            "--native is only valid with `build`",
        ),
        (
            &["check", "--use", "io", main],
            "--use is only valid with `eval`",
        ),
        (&["check", "--output", "x", main], "--output is only valid"),
        (&["run", "--bogus", main], "unknown option `--bogus`"),
    ];
    for (args, message) in cases {
        assert_rejected(&cog(None, args), message);
    }
}

#[test]
fn jit_is_accepted_on_run_only_and_matches_interpreter_output() {
    let dir = Dir::new();
    dir.write("main.cog", LOOP_PROGRAM);
    let main = dir.path("main.cog");
    let main = path_str(&main);

    let plain = cog(None, &["run", main]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert_eq!(stdout(&plain), "19900\n");
    let jit = cog(None, &["run", "--jit", main]);
    assert!(jit.status.success(), "{}", stderr(&jit));
    assert_eq!(stdout(&jit), stdout(&plain));

    for command in ["check", "build"] {
        assert_rejected(
            &cog(None, &[command, "--jit", main]),
            "--jit is only valid with `run`",
        );
    }
}

#[test]
fn short_help_and_debug_flags_match_their_long_forms() {
    let help = cog(None, &["--help"]);
    assert!(help.status.success());
    assert!(!stdout(&help).is_empty() || !stderr(&help).is_empty());
    let short = cog(None, &["-h"]);
    assert!(short.status.success());
    assert_eq!(short.stdout, help.stdout);
    assert_eq!(short.stderr, help.stderr);

    let dir = Dir::new();
    dir.write("main.cog", LOOP_PROGRAM);
    let main = dir.path("main.cog");
    let main = path_str(&main);
    let long = cog(None, &["run", "--debug", main]);
    let short = cog(None, &["run", "-v", main]);
    assert!(long.status.success(), "{}", stderr(&long));
    assert!(short.status.success(), "{}", stderr(&short));
    assert_eq!(short.stdout, long.stdout);
    let plain = cog(None, &["run", main]);
    assert_ne!(stderr(&long), stderr(&plain), "--debug must add output");
}
