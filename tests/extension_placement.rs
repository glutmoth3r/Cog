//! Which modules may declare `extend` blocks.
use cog::{bytecode, frontend, interpreter, project};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);

impl Directory {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cog-placement-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self(root)
    }

    fn compile(&self) -> Result<cog::backend::CompiledProgram, String> {
        let loaded =
            project::load(&self.0, None, false).map_err(|errors| frontend::render(&errors))?;
        project::compile(loaded.program, loaded.diagnostics, &loaded.names)
    }

    fn run(&self) -> String {
        let program = self.compile().unwrap_or_else(|error| panic!("{error}"));
        execute(&program)
    }

    fn rejects(&self) -> String {
        self.compile().expect_err("the program must be rejected")
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn execute(program: &cog::backend::CompiledProgram) -> String {
    let program = bytecode::decode(&bytecode::encode(program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

const MISPLACED: &str = "only in the module that declares the trait or the type";

#[test]
fn the_trait_module_and_the_type_module_may_implement_a_trait() {
    let project = Directory::new(&[
        (
            "shapes.cog",
            r#"
            use render::Show;
            public struct Square { public side: int }
            extend Square with Show { fn show(self) -> str { "square" } }
            "#,
        ),
        (
            "render.cog",
            r#"
            use circles::Circle;
            public trait Show { fn show(self) -> str; }
            extend Circle with Show { fn show(self) -> str { "circle" } }
            extend int with Show { fn show(self) -> str { "int" } }
            extend[T: type] T[] with Show { fn show(self) -> str { "array" } }
            public fn print[T: type + Show](value: T) -> str { value.show() }
            "#,
        ),
        ("circles.cog", "public struct Circle {}"),
        (
            "main.cog",
            r#"@!include("std")
            use io;
            use render::print;
            use shapes::Square;
            use circles::Circle;
            fn main() {
                io::println(print(Square(1)))!!;
                io::println(print(Circle()))!!;
                io::println(print(1))!!;
                io::println(print([1]))!!;
            }
            "#,
        ),
    ]);
    assert_eq!(project.run(), "square\ncircle\nint\narray\n");
}

#[test]
fn a_third_module_cannot_implement_a_trait() {
    let project = Directory::new(&[
        ("render.cog", "public trait Show { fn show(self) -> str; }"),
        ("shapes.cog", "public struct Square {}"),
        (
            "glue.cog",
            r#"
            use render::Show;
            use shapes::Square;
            extend Square with Show { fn show(self) -> str { "square" } }
            "#,
        ),
        (
            "main.cog",
            "use glue; use shapes::Square; fn main() { Square().show(); }",
        ),
    ]);
    let error = project.rejects();
    assert!(error.contains(MISPLACED), "{error}");
    assert!(
        error.contains("trait `Show` can be implemented for `Square`"),
        "{error}"
    );
    assert!(
        error.contains("`Show` is declared in module `render`"),
        "{error}"
    );
    assert!(
        error.contains("`Square` is declared in module `shapes`"),
        "{error}"
    );
    assert!(
        error.contains("move the implementation to module `render` or module `shapes`"),
        "{error}"
    );
    assert!(error.contains("glue.cog"), "{error}");
    assert!(!error.contains("$symbol"), "{error}");
}

#[test]
fn a_shared_trait_and_type_module_is_suggested_once() {
    let project = Directory::new(&[
        (
            "geo.cog",
            "public trait Show { fn show(self) -> str; } public struct Square {}",
        ),
        (
            "glue.cog",
            r#"
            use geo::Show;
            use geo::Square;
            extend Square with Show { fn show(self) -> str { "square" } }
            "#,
        ),
        (
            "main.cog",
            "use glue; use geo::Square; fn main() { Square().show(); }",
        ),
    ]);
    let error = project.rejects();
    assert!(error.contains(MISPLACED), "{error}");
    assert!(
        error.contains("move the implementation to module `geo`, or wrap"),
        "{error}"
    );
    assert!(!error.contains("or module `geo`"), "{error}");
}

#[test]
fn built_in_targets_belong_only_to_the_trait_module() {
    for target in ["int", "int[]", "(int, str)", "int?"] {
        let source = format!(
            r#"@!include("std")
            use eq;
            extend {target} with eq::Eq[{target}] {{
                fn equals(self, other: {target}) -> bool {{ true }}
            }}
            fn main() {{}}
            "#
        );
        let error = project::compile_source("builtin.cog", &source).expect_err(target);
        assert!(error.contains(MISPLACED), "{target}: {error}");
        assert!(
            error.contains("`Eq` is declared in standard module `eq`"),
            "{error}"
        );
        assert!(
            error.contains("wrap the value in a struct declared in this module"),
            "{error}"
        );
        assert!(!error.contains("move the implementation"), "{error}");
    }
    let program = project::compile_source(
        "local.cog",
        r#"@!include("std")
        use io;
        trait Show { fn show(self) -> str; }
        extend int[] with Show { fn show(self) -> str { "local" } }
        fn main() { io::println([1].show())!!; }
        "#,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(execute(&program), "local\n");
}

#[test]
fn a_standard_trait_cannot_override_the_structural_clone_of_arrays() {
    let error = project::compile_source(
        "clone.cog",
        r#"@!include("std")
        use clone::Clone;
        extend[T: type] T[] with Clone { fn clone(self) -> T[] { self } }
        fn main() {}
        "#,
    )
    .unwrap_err();
    assert!(error.contains(MISPLACED), "{error}");
}

#[test]
fn type_aliases_do_not_move_the_declaring_module() {
    let files = |implementation: &str| {
        Directory::new(&[
            ("render.cog", "public trait Show { fn show(self) -> str; }"),
            ("shapes.cog", "public struct Square {}"),
            ("main.cog", implementation),
        ])
    };
    for target in ["int", "Square", "Alias"] {
        let error = files(&format!(
            r#"use render::Show;
            use shapes::Square;
            @comptime const Alias: type = Square;
            @comptime const Target: type = {target};
            @comptime extend Target with Show {{ fn show(self) -> str {{ "alias" }} }}
            fn main() {{}}
            "#
        ))
        .rejects();
        assert!(error.contains(MISPLACED), "{target}: {error}");
    }
    let local = files(
        r#"@!include("std")
        use io;
        use render::Show;
        struct Local {}
        @comptime const Target: type = Local;
        @comptime extend Target with Show { fn show(self) -> str { "local" } }
        fn main() { io::println(Local().show())!!; }
        "#,
    );
    assert_eq!(local.run(), "local\n");
}

#[test]
fn implementations_in_the_two_eligible_modules_must_not_overlap() {
    let project = Directory::new(&[
        (
            "render.cog",
            r#"
            use boxes::Box;
            public trait Show { fn show(self) -> str; }
            extend[T: type] Box[T] with Show { fn show(self) -> str { "generic" } }
            "#,
        ),
        (
            "boxes.cog",
            r#"
            use render::Show;
            public struct Box[T: type] { public value: T }
            extend Box[int] with Show { fn show(self) -> str { "concrete" } }
            "#,
        ),
        ("main.cog", "use render; use boxes; fn main() {}"),
    ]);
    let error = project.rejects();
    assert!(
        error.contains("trait `render::Show` has overlapping implementations")
            || error.contains("has overlapping implementations for `Box[int]`"),
        "{error}"
    );
    assert!(!error.contains(MISPLACED), "{error}");
}

#[test]
fn modules_with_the_same_path_in_different_packages_stay_distinct() {
    let project = Directory::new(&[
        ("package.kdl", "package \"app\" version=\"0.1.0\"\n"),
        ("dependencies.kdl", "dep path=\"packages/dep\"\n"),
        (
            "packages/dep/package.kdl",
            "package \"library\" version=\"0.1.0\"\n",
        ),
        (
            "packages/dep/src/shapes.cog",
            "public trait Show { fn show(self) -> str; } public struct Square {}",
        ),
        (
            "src/shapes.cog",
            r#"
            use shapes::Show from dep;
            use shapes::Square from dep;
            public fn marker() {}
            extend Square with Show { fn show(self) -> str { "app" } }
            "#,
        ),
        (
            "src/main.cog",
            "@!include(\"dep\")\nuse super::shapes::marker; fn main() {}",
        ),
    ]);
    let error = project.rejects();
    assert!(error.contains(MISPLACED), "{error}");
    assert!(
        error.contains("declared in module `shapes` of package `dep`"),
        "{error}"
    );
}

#[test]
fn inherent_extensions_belong_to_the_module_of_the_type() {
    let project = Directory::new(&[
        (
            "boxes.cog",
            r#"
            public struct Box[T: type] { public value: T }
            extend[T: type] Box[T] { public fn get(self) -> T { self.value } }
            extend Box[int] {
                public fn doubled(self) -> int { self.value * 2 }
                public fn one() -> Box[int] { Box(1) }
            }
            public enum Mode { On }
            extend Mode { public fn label(self) -> str { "on" } }
            "#,
        ),
        (
            "main.cog",
            r#"@!include("std")
            use io;
            use array;
            use boxes::Box;
            use boxes::Mode;
            fn main() {
                var values = [Box(2).doubled(), Box::one().get()];
                values.swap(0, 1)!!;
                io::println(values)!!;
                const mode: Mode = Mode::On();
                io::println(mode.label())!!;
            }
            "#,
        ),
    ]);
    assert_eq!(project.run(), "[1, 4]\non\n");
}

#[test]
fn a_module_cannot_add_inherent_methods_to_a_foreign_type() {
    for (method, signature) in [
        (
            "fn name(self) -> str { \"glue\" }",
            "`fn name(value: Box[int])`",
        ),
        ("fn empty() -> int { 0 }", "`fn empty()`"),
    ] {
        let glue = format!("use boxes::Box;\nextend Box[int] {{ {method} }}\n");
        let project = Directory::new(&[
            (
                "boxes.cog",
                "public struct Box[T: type] { public value: T }",
            ),
            ("glue.cog", &glue),
            ("main.cog", "use glue; fn main() {}"),
        ]);
        let error = project.rejects();
        assert!(
            error.contains(
                "inherent methods of `Box[int]` can be declared only in the module that declares `Box`"
            ),
            "{error}"
        );
        assert!(
            error.contains("`Box` is declared in module `boxes`"),
            "{error}"
        );
        assert!(
            error.contains(&format!(
                "declare a free function instead, such as {signature}"
            )),
            "{error}"
        );
        assert!(error.contains("glue.cog"), "{error}");
        assert!(!error.contains("$symbol"), "{error}");
    }
}

#[test]
fn only_the_standard_library_extends_built_in_types() {
    for (extension, signature) in [
        (
            "@comptime extend int { fn double(self) -> int { self * 2 } }",
            "`fn double(value: int)`",
        ),
        (
            "extend[T: type] T[] { fn head(self) -> T { self[0] } }",
            "`fn head[T: type](value: T[])`",
        ),
        (
            "@comptime const Scalar: type = int;\n@comptime extend Scalar { fn double(self) -> int { self * 2 } }",
            "`fn double(value: Scalar)`",
        ),
    ] {
        let source = format!("@!include(\"std\")\n{extension}\nfn main() {{}}\n");
        let error = project::compile_source("builtin.cog", &source).expect_err(extension);
        assert!(
            error.contains("only the standard library can declare inherent methods of"),
            "{error}"
        );
        assert!(error.contains(signature), "{error}");
    }
}
