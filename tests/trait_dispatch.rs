//! A call through a trait bound runs the implementation that proved the bound.
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, frontend, interpreter, project};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn execute(program: &cog::backend::CompiledProgram, jit: bool) -> String {
    let program = bytecode::decode(&bytecode::encode(program).unwrap()).unwrap();
    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    context.configure_jit(jit, None, false);
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    String::from_utf8(output).unwrap()
}

fn compile(source: &str) -> Result<cog::backend::CompiledProgram, String> {
    project::compile_source("dispatch.cog", &format!("@!include(\"std\")\n{source}"))
}

/// Runs the encoded program in the VM and with the JIT enabled.
fn run(source: &str) -> String {
    let program = compile(source).unwrap_or_else(|error| panic!("{error}"));
    let output = execute(&program, false);
    assert_eq!(execute(&program, true), output, "JIT output differs");
    output
}

fn rejects(source: &str) -> String {
    compile(source).expect_err("the program must be rejected")
}

struct Directory(PathBuf);

impl Directory {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cog-dispatch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        Self(root)
    }

    fn compile(&self) -> Result<cog::backend::CompiledProgram, String> {
        let loaded =
            project::load(&self.0, None, false).map_err(|errors| frontend::render(&errors))?;
        project::compile(loaded.program, loaded.diagnostics, &loaded.names)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SHOW: &str = r#"
    use io;
    trait Show { fn show(self) -> str; }
    struct Box[T: type] { value: T }
    extend[T: type] Box[T] with Show { fn show(self) -> str { "trait" } }
"#;

#[test]
fn bound_calls_ignore_inherent_extensions_of_a_concrete_instance() {
    let output = run(&format!(
        r#"{SHOW}
        extend Box[int] {{ fn show(self) -> str {{ "inherent" }} }}
        fn render[X: type + Show](x: X) -> str {{ x.show() }}
        fn main() {{
            io::println(render(Box(1)))!!;
            io::println(Box(1).show())!!;
            io::println(render(Box("a")))!!;
        }}
        "#
    ));
    assert_eq!(output, "trait\ninherent\ntrait\n");
}

#[test]
fn bound_calls_type_check_against_the_implementation_not_the_name() {
    let output = run(&format!(
        r#"{SHOW}
        extend Box[int] {{ fn show(self) -> int {{ 42 }} }}
        fn render[X: type + Show](x: X) -> str {{ x.show() }}
        fn main() {{ io::println(render(Box(1)))!!; io::println(Box(1).show() + 1)!!; }}
        "#
    ));
    assert_eq!(output, "trait\n43\n");
}

#[test]
fn bound_calls_ignore_a_method_of_the_same_name_from_another_trait() {
    let output = run(&format!(
        r#"{SHOW}
        trait Count {{ fn show(self) -> int; }}
        extend Box[int] with Count {{ fn show(self) -> int {{ 7 }} }}
        fn render[X: type + Show](x: X) -> str {{ x.show() }}
        fn count[X: type + Count](x: X) -> int {{ x.show() }}
        fn main() {{ io::println(render(Box(1)))!!; io::println(count(Box(1)))!!; }}
        "#
    ));
    assert_eq!(output, "trait\n7\n");
}

#[test]
fn adding_an_import_does_not_change_bound_dispatch() {
    const MODEL: &str = r#"
        public trait Show { fn show(self) -> str; }
        public struct Box[T: type] { value: T }
        extend[T: type] Box[T] with Show { fn show(self) -> str { "model" } }
        public fn render[X: type + Show](x: X) -> str { x.show() }
    "#;
    const OTHER: &str = r#"
        use model::Box;
        public trait Name { fn show(self) -> str; }
        extend Box[int] with Name { fn show(self) -> str { "other" } }
    "#;
    for import in ["", "use other;"] {
        let main = format!(
            "@!include(\"std\")\nuse io; use model::Box; use model::render; {import}\n\
             fn main() {{ io::println(render(Box(1)))!!; }}"
        );
        let project = Directory::new(&[
            ("model.cog", MODEL),
            ("other.cog", OTHER),
            ("main.cog", &main),
        ]);
        let program = project.compile().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(execute(&program, false), "model\n", "import: {import:?}");
    }
}

#[test]
fn bound_calls_are_followed_through_values_derived_from_a_type_parameter() {
    let output = run(r#"
        use io;
        trait Show { fn show(self) -> str; fn again(self) -> Self; }
        struct Box[T: type] { value: T }
        extend[T: type] Box[T] with Show {
            fn show(self) -> str { "t" }
            fn again(self) -> Box[T] { self }
        }
        extend Box[int] {
            fn show(self) -> str { "i" }
            fn again(self) -> int { 0 }
        }
        struct Wrap[T: type] { item: T, rest: T[] }
        extend[T: type + Show] Wrap[T] {
            fn render(self) -> str {
                var out = self.item.show();
                for item in self.rest { out += item.show(); }
                out
            }
        }
        fn first[T: type](items: T[]) -> T { items[0]!! }
        fn all[X: type + Show](x: X, items: X[], maybe: X?) -> str {
            const copy = x;
            var current: X = copy;
            current = current.again()!!;
            var out = x.again().show() + first(items).show() + current.show();
            out += items[0].show();
            for item in items { out += item.show(); }
            for left in items, right in [x] { out += left.show() + right.show(); }
            if maybe != nil { out += maybe.show(); }
            const pair = (x, [x]);
            out += pair.0.show() + pair.1[0].show();
            const render = fn(item: X) -> str { item.show() };
            out += (render)(x);
            var rounds = 0;
            while first([x.show()]) == "t" && rounds < 1 { rounds += 1; out += x.show(); }
            out + Wrap(x, [x]).render()
        }
        fn main() { io::println(all(Box(1), [Box(2)], Box(3)))!!; }
        "#);
    assert_eq!(output, format!("{}\n", "t".repeat(14)));
}

#[test]
fn a_binding_demoted_to_dynamic_no_longer_calls_through_the_bound() {
    let error = rejects(&format!(
        r#"{SHOW}
        fn anything(value) {{ value }}
        fn render[X: type + Show](x: X) -> str {{
            var current = x;
            const out = current.show();
            current = anything(x);
            out + current.show()
        }}
        fn main() {{ render(Box(1)); }}
        "#
    ));
    assert!(error.contains("unknown method `show()`"), "{error}");
}

#[test]
fn calls_in_a_loop_condition_are_specialized_with_explicit_type_arguments() {
    let output = run(r#"
        use io;
        trait Limit { fn limit(self) -> int; }
        struct Two {}
        extend Two with Limit { fn limit(self) -> int { 2 } }
        fn limit[T: type + Limit](value: T) -> int { value.limit() }
        fn count[T: type + Limit](value: T) -> int {
            var index = 0;
            while index < limit[T](value) { index += 1; }
            index
        }
        fn main() { io::println(count(Two()))!!; }
        "#);
    assert_eq!(output, "2\n");
}

#[test]
fn trait_arguments_select_between_methods_of_the_same_name() {
    let output = run(r#"
        use io;
        trait Convert[To: type] { fn convert(self) -> To; }
        struct Cell[T: type] { value: T }
        extend[T: type] Cell[T] with Convert[int] { fn convert(self) -> int { 5 } }
        extend[U: type] Cell[U] with Convert[str] { fn convert(self) -> str { "five" } }
        fn number[X: type + Convert[int]](x: X) -> int { x.convert() }
        fn text[X: type + Convert[str]](x: X) -> str { x.convert() }
        fn main() { io::println(number(Cell(1)) + 1)!!; io::println(text(Cell(1)))!!; }
        "#);
    assert_eq!(output, "6\nfive\n");
}

#[test]
fn two_bounds_declaring_the_called_method_are_ambiguous() {
    let error = rejects(
        r#"
        use io;
        trait Left { fn name(self) -> str; }
        trait Right { fn name(self) -> str; }
        struct Box[T: type] { value: T }
        extend[T: type] Box[T] with Left { fn name(self) -> str { "left" } }
        extend[U: type] Box[U] with Right { fn name(self) -> str { "right" } }
        fn both[X: type + Left + Right](x: X) -> str { x.name() }
        fn main() { io::println(both(Box(1)))!!; }
        "#,
    );
    assert!(
        error.contains("method `name()` is ambiguous: bounds `Left` and `Right` both"),
        "{error}"
    );
}

#[test]
fn one_trait_bound_twice_with_different_arguments_is_ambiguous_by_name() {
    let error = rejects(
        r#"
        use io;
        trait Convert[To: type] { fn convert(self) -> To; }
        struct Cell[T: type] { value: T }
        extend[T: type] Cell[T] with Convert[int] { fn convert(self) -> int { 5 } }
        extend[U: type] Cell[U] with Convert[str] { fn convert(self) -> str { "five" } }
        fn both[X: type + Convert[int] + Convert[str]](x: X) { x.convert(); }
        fn main() { both(Cell(1)); }
        "#,
    );
    assert!(
        error.contains("bounds `Convert[int]` and `Convert[str]` both declare it"),
        "{error}"
    );
}

#[test]
fn standard_equality_and_hashable_bounds_share_one_equals() {
    let output = run(r#"
        use io;
        use eq;
        use collections::Hashable;
        fn same[K: type + Hashable + eq::Eq[K]](a: K, b: K) -> bool { a.equals(b) }
        fn main() { io::println(same(1, 1))!!; io::println(same("a", "b"))!!; }
        "#);
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn lookalike_traits_do_not_share_the_standard_equals_exception() {
    let error = rejects(
        r#"
        trait Same[Other: type] { fn equals(self, other: Other) -> bool; }
        trait Hashed { fn hash_code(self) -> int; fn equals(self, other: Self) -> bool; }
        struct Id { value: int }
        extend Id with Same[Id] { fn equals(self, other: Id) -> bool { true } }
        extend Id with Hashed {
            fn hash_code(self) -> int { 0 }
            fn equals(self, other: Id) -> bool { true }
        }
        fn main() {}
        "#,
    );
    assert!(
        error.contains("extension method `equals()` is defined more than once"),
        "{error}"
    );
}

#[test]
fn struct_equality_uses_the_eq_implementation_despite_inherent_equals() {
    let output = run(r#"
        use io;
        use eq;
        struct Pair[T: type] { value: T }
        extend[T: type] Pair[T] with eq::Eq[Pair[T]] {
            fn equals(self, other: Pair[T]) -> bool { true }
        }
        extend Pair[int] { fn equals(self, other: Pair[int]) -> bool { false } }
        fn same[X: type + eq::Eq[X]](a: X, b: X) -> (bool, bool, bool) {
            (a == b, a != b, a.equals(b))
        }
        fn main() {
            io::println(Pair(1) == Pair(2))!!;
            io::println(Pair(1).equals(Pair(2)))!!;
            io::println(same(Pair(1), Pair(2)))!!;
        }
        "#);
    assert_eq!(output, "true\nfalse\n[true, false, true]\n");
}

const ITERATORS: &str = r#"
    use io;
    use iter;
    struct Source[T: type] { items: T[] }
    struct Cursor[T: type] { items: T[], at: int }
    extend[T: type] Source[T] with iter::IntoIterator[Cursor[T], T] {
        fn into_iter(self) -> Cursor[T] { Cursor(self.items, -1) }
    }
    extend[T: type] Cursor[T] with iter::Iterator[T] {
        fn next(mut self) -> bool { self.at += 1; self.at < self.items.len() }
        fn current(self) -> T { self.items[self.at]!! }
    }
    extend Source[int] { fn into_iter(self) -> int { 99 } }
    extend Cursor[int] {
        fn next(self) -> str { "inherent" }
        fn current(self) -> str { "inherent" }
    }
"#;

#[test]
fn iteration_uses_the_trait_implementations_despite_inherent_methods() {
    let output = run(&format!(
        r#"{ITERATORS}
        fn total[C: type + iter::IntoIterator[Cursor[int], int]](c: C) -> int {{
            var sum = 0;
            for x in c {{ sum += x; }}
            for x in c, y in c {{ sum += x * y; }}
            sum
        }}
        fn manual[I: type + iter::Iterator[int], C: type + iter::IntoIterator[I, int]](
            c: C
        ) -> int {{
            var cursor = c.into_iter();
            var sum = 0;
            while cursor.next() {{ sum += cursor.current()!!; }}
            sum
        }}
        fn main() {{
            var sum = 0;
            for x in Source([1, 2, 3]) {{ sum += x; }}
            io::println(sum)!!;
            io::println(total(Source([1, 2, 3])))!!;
            io::println(manual(Source([1, 2, 4])))!!;
            io::println(Source([1]).into_iter())!!;
        }}
        "#
    ));
    assert_eq!(output, "6\n20\n7\n99\n");
}

#[test]
fn a_bounded_loop_uses_the_iterator_arguments_of_its_bound() {
    let output = run(r#"
        use io;
        use iter;
        struct Both[T: type] { tag: T }
        struct Numbers { done: bool }
        struct Words { done: bool }
        extend[T: type] Both[T] with iter::IntoIterator[Numbers, int] {
            fn into_iter(self) -> Numbers { Numbers(false) }
        }
        extend[U: type] Both[U] with iter::IntoIterator[Words, str] {
            fn into_iter(self) -> Words { Words(false) }
        }
        extend Numbers with iter::Iterator[int] {
            fn next(mut self) -> bool { const more = !self.done; self.done = true; more }
            fn current(self) -> int { 1 }
        }
        extend Words with iter::Iterator[str] {
            fn next(mut self) -> bool { const more = !self.done; self.done = true; more }
            fn current(self) -> str { "word" }
        }
        fn words[C: type + iter::IntoIterator[Words, str]](c: C) {
            for item in c { io::println(item)!!; }
        }
        fn numbers[C: type + iter::IntoIterator[Numbers, int]](c: C) {
            for item in c { io::println(item + 1)!!; }
        }
        fn main() { words(Both(0)); numbers(Both(0)); }
        "#);
    assert_eq!(output, "word\n2\n");
}

#[test]
fn mutation_effects_follow_the_implementation_that_proved_the_bound() {
    let declarations = r#"
        use io;
        trait Touch { fn touch(mut self); fn peek(self) -> int; }
        struct List[T: type] { items: int[], tag: T }
        extend[T: type] List[T] with Touch {
            fn touch(mut self) { self.items.append(1); }
            fn peek(self) -> int { self.items.len() }
        }
        extend List[int] {
            fn touch(self) {}
            fn peek(mut self) -> int { self.items.append(2); 0 }
        }
        fn touch[X: type + Touch](x: X) { x.touch(); }
        fn peek[X: type + Touch](x: X) -> int { x.peek() }
        const SHARED = List([], 0);
    "#;
    let error = rejects(&format!("{declarations} fn main() {{ touch(SHARED); }}"));
    assert!(
        error.contains("cannot mutate through const/read-only access"),
        "{error}"
    );
    let output = run(&format!(
        r#"{declarations}
        fn main() {{
            io::println(peek(SHARED))!!;
            var list = List([], 0);
            touch(list);
            io::println(peek(list))!!;
        }}"#
    ));
    assert_eq!(output, "0\n1\n");
}

#[test]
fn clone_bounds_use_the_proving_implementation_or_the_structural_clone() {
    let output = run(r#"
        use io;
        use clone::Clone;
        struct Box[T: type] { value: T }
        extend[T: type] Box[T] with Clone { fn clone(self) -> Box[T] { Box(self.value) } }
        extend Box[int] { fn clone(self) -> int { 0 } }
        fn copy[T: type + Clone](v: T) -> T { v.clone() }
        fn copies[T: type + Clone](items: T[]) -> T[] { items.clone() }
        fn main() {
            io::println(copy(Box(3)).value)!!;
            const original = [[1]];
            var copied = copy(original);
            copied[0].append(2);
            io::println(original)!!;
            io::println(copied)!!;
            io::println(copies([(1, "a")]))!!;
        }
        "#);
    assert_eq!(output, "3\n[[1]]\n[[1, 2]]\n[[1, a]]\n");
}

#[test]
fn bound_dispatch_keeps_async_methods_nilable_targets_and_evaluation_order() {
    let output = run(r#"
        use io;
        trait Load { @async fn load(self, first: int, second: int) -> int; }
        struct Box[T: type] { value: T }
        extend[T: type] Box[T] with Load {
            @async fn load(self, first: int, second: int) -> int { first * 10 + second }
        }
        extend Box[int] { fn load(self) -> str { "inherent" } }
        fn mark(value: int) -> int { io::println(value)!!; value }
        fn receiver[T: type](value: T) -> T { io::println("receiver")!!; value }
        @async fn load[X: type + Load](x: X) -> int {
            (await receiver(x).load(mark(1), mark(2)))!!
        }
        trait Describe { fn describe(self) -> str; }
        extend int? with Describe {
            fn describe(self) -> str { if self == nil { "nil" } else { "int" } }
        }
        fn describe[X: type + Describe](x: X) -> str { x.describe() }
        @async fn main() {
            io::println((await load(Box(1)))!!)!!;
            const none: int? = nil;
            io::println(describe(none))!!;
        }
        "#);
    assert_eq!(output, "receiver\n1\n2\n12\nnil\n");
}

#[test]
fn bound_calls_dispatch_in_hot_loops() {
    let output = run(&format!(
        r#"{SHOW}
        trait Weight {{ fn weight(self) -> int; }}
        extend[T: type] Box[T] with Weight {{ fn weight(self) -> int {{ 1 }} }}
        extend Box[int] {{ fn weight(self) -> int {{ 1000 }} }}
        fn total[X: type + Weight](x: X, rounds: int) -> int {{
            var index = 0;
            var sum = 0;
            while index < rounds {{ sum += x.weight(); index += 1; }}
            sum
        }}
        fn main() {{ io::println(total(Box(1), 6000))!!; }}
        "#
    ));
    assert_eq!(output, "6000\n");
}
