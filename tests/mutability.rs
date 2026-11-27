use cog::{bytecode, interpreter, project};

fn source(body: &str) -> String {
    format!("@!include(\"std\") use io; use err::Error; {body}")
}
fn run(body: &str) -> String {
    let program =
        project::compile_source("mutability.cog", &source(body)).unwrap_or_else(|e| panic!("{e}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut out = vec![];
    interpreter::run_with(&program, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}
fn rejects(body: &str, expected: &str) {
    let error = project::compile_source("mutability.cog", &source(body))
        .expect_err("must fail semantic checking")
        .to_string();
    assert!(!error.contains("E0002"), "{error}");
    assert!(error.contains(expected), "expected {expected}: {error}");
}

#[test]
fn when_without_else_keeps_the_unmatched_path_reachable_for_mutability() {
    assert_eq!(
        run(r#"
        struct Node { value: int, next: Node? }
        extend Node {
            fn depth(self) -> int {
                var below = 0;
                const next = self.next;
                when next is Node { below = next.depth(); }
                below + 1
            }
        }
        fn early(value: int) -> int {
            var result = 0;
            when value is int { return 1; }
            result = 2;
            result
        }
        fn main() {
            var node = Node(value: 1, next: Node(value: 2, next: nil));
            io::println(node.depth());
            node.value = 3;
            io::println($"{node.value} {early(5)}");
        }
    "#),
        "2\n3 1\n"
    );
    rejects(
        "fn f(v: int) -> int { var r = 0; when v is int { return 1; } else { return 2; } r = 3; r } \
         fn main() { io::println(f(1)); }",
        "requires a reachable mutation",
    );
}

#[test]
fn shared_aliases_keep_independent_access_paths_without_moves() {
    assert_eq!(
        run("fn main() { var a = [1]; const b = a; a.append(2); io::println(a); io::println(b); }"),
        "[1, 2]\n[1, 2]\n"
    );
    rejects(
        "fn main() { var a = [1]; const b = a; a.append(2); b.append(3); }",
        "cannot mutate",
    );
    rejects(
        "fn main() { const a = [1]; var b = a; b.append(2); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn main() { const a = [1]; var b = a; b = [10, 20]; b.append(30); io::println(a); io::println(b); }"
        ),
        "[1]\n[10, 20, 30]\n"
    );
    assert_eq!(
        run("fn main() { var a = [1]; var b = a; b.append(2); io::println(a); }"),
        "[1, 2]\n"
    );
}

#[test]
fn capabilities_follow_alias_reassignment_and_control_flow_joins() {
    rejects(
        "fn main() { var a = [1]; const b = [2]; a = b; a.append(3); }",
        "cannot mutate",
    );
    rejects(
        "fn f(flag: bool) { const a = [1]; var b = [2]; if flag { b = a; } b.append(3); } fn main() { f(false); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn f(flag: bool) { const a = [1]; var b = a; if flag { b = [2]; } else { b = [3]; } b.append(4); io::println(b); } fn main() { f(true); f(false); }"
        ),
        "[2, 4]\n[3, 4]\n"
    );
    rejects(
        "fn main() { const a = [1]; var b = [2]; if false { b.append(3); } }",
        "requires a reachable mutation",
    );
}

#[test]
fn nested_paths_and_heap_stores_do_not_launder_readonly_capabilities() {
    rejects(
        "fn main() { const a = [[1]]; a[0].append(2); }",
        "cannot mutate",
    );
    rejects(
        "fn main() { const a = [1]; var holder = { item: [2] }; holder.item = a; holder.item.append(3); }",
        "cannot mutate",
    );
    rejects(
        "fn main() { const a = [1]; var holder = { item: a }; holder.item.append(3); }",
        "cannot mutate",
    );
    rejects(
        "fn main() { var holder = { item: [2] }; var other = holder; const a = [1]; holder.item = a; other.item.append(3); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn main() { var holder = { item: [2] }; const other = holder; holder.item.append(3); io::println(other.item); }"
        ),
        "[2, 3]\n"
    );
}

#[test]
fn effects_cross_helpers_aliases_returns_and_recursive_cycles() {
    rejects(
        "fn change(a) { a.append(2); } fn wrap(a) { var alias = a; change(alias); } fn main() { const a = [1]; wrap(a); }",
        "cannot mutate",
    );
    rejects(
        "fn identity(a) { a } fn main() { const a = [1]; identity(a).append(2); }",
        "cannot mutate",
    );
    rejects(
        "fn boxed(a) { { item: a } } fn main() { const a = [1]; var b = boxed(a); b.item.append(2); }",
        "cannot mutate",
    );
    rejects(
        "fn one(a, n: int) { if n > 0 { two(a, n - 1); } } fn two(a, n: int) { a.append(2); one(a, n); } fn main() { const a = [1]; one(a, 1); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn change(a) { a.append(2); } fn identity(a) { a } fn main() { var a = [1]; change(identity(a)); io::println(a); }"
        ),
        "[1, 2]\n"
    );
    // Rebinding a parameter changes its slot, not the caller's object.
    assert_eq!(
        run(
            "fn replace(a) { a = [2]; a.append(3); a } fn main() { const a = [1]; io::println(replace(a)); io::println(a); }"
        ),
        "[2, 3]\n[1]\n"
    );
}

#[test]
fn effects_reach_callers_through_helper_chains_settled_in_later_iterations() {
    // Each level only learns its callee's effect after the previous fixed-point
    // iteration, while unrelated helpers have already settled.
    let chain = "fn level0(a) { a.append(2); } fn level1(a) { level0(a); } \
                 fn level2(a) { level1(a); } fn level3(a) { level2(a); } \
                 fn level4(a) { level3(a); } fn settled(a) { a }";
    rejects(
        &format!("{chain} fn main() {{ const a = [1]; level4(settled(a)); }}"),
        "cannot mutate",
    );
    rejects(
        &format!(
            "{chain} fn pick(a, b) {{ settled(a); level4(b); }} \
             fn main() {{ var a = [1]; const b = [2]; pick(a, b); a.append(3); }}"
        ),
        "cannot mutate",
    );
    rejects(
        "fn bottom(a) { a } fn middle(a) { bottom(a) } fn top(a) { middle(a) } \
         fn main() { const a = [1]; top(a).append(2); }",
        "cannot mutate",
    );
    // The local is mutated only through the deepest helper; it must not be
    // reported as needing `const`.
    assert_eq!(
        run(&format!(
            "{chain} fn main() {{ var a = [1]; level4(a); io::println(a); }}"
        )),
        "[1, 2]\n"
    );
}

#[test]
fn map_helpers_keep_effects_through_generic_specializations() {
    let helpers = "use collections; use clone::Clone; \
        fn with_entry(items: collections::Map[str, str], key: str) -> collections::Map[str, str] { \
            items.set(key, key)!!; items } \
        fn copied_entry(items: collections::Map[str, str], key: str) -> collections::Map[str, str] { \
            var copy = items.clone(); copy.set(key, key)!!; copy } \
        fn twice(items: collections::Map[str, str]) -> collections::Map[str, str] { \
            with_entry(with_entry(items, \"a\")!!, \"b\")!! } \
        fn copied_twice(items: collections::Map[str, str]) -> collections::Map[str, str] { \
            copied_entry(copied_entry(items, \"a\")!!, \"b\")!! }";
    rejects(
        &format!(
            "{helpers} fn main() {{ const items = collections::Map::new[str, str](); twice(items)!!; }}"
        ),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{helpers} fn main() {{ const items = collections::Map::new[str, str](); \
             io::println(copied_twice(items)!!.len()); io::println(items.len()); }}"
        )),
        "2\n0\n"
    );
}

#[test]
fn chained_helpers_over_map_records_keep_effects_and_check_quickly() {
    // Generic map internals store scalar fields such as hashes and links.
    // Their declared types keep those fields free of reference capabilities,
    // so repeated helper calls do not multiply symbolic heap contents.
    let helpers = "use collections; use clone::Clone; \
        struct Response { status: int, headers: collections::Map[str, str], body: str } \
        fn with_header(response: Response, name: str, value: str) -> Response { \
            var headers = response.headers.clone(); \
            var old: str[] = []; \
            for entry in headers.entries() { if entry.0 == name { old.append(entry.0); } } \
            for key in old { headers.remove(key)!!; } \
            headers.set(name, value)!!; \
            Response(status: response.status, headers: headers, body: response.body) } \
        fn decorate(response: Response, strict: bool) -> Response { \
            var result = with_header(response, \"a\", \"1\")!!; \
            if strict { result = with_header(result, \"b\", \"2\")!!; } \
            else { result = with_header(result, \"c\", \"3\")!!; } \
            result = with_header(result, \"d\", \"4\")!!; \
            result = with_header(result, \"e\", \"5\")!!; \
            if strict { result = with_header(result, \"f\", \"6\")!!; } \
            result = with_header(result, \"g\", \"7\")!!; \
            result = with_header(result, \"a\", \"8\")!!; \
            result }";
    let started = std::time::Instant::now();
    assert_eq!(
        run(&format!(
            "{helpers} fn main() {{ \
             const original = Response(status: 200, headers: collections::Map::new[str, str](), body: \"\"); \
             const strict = decorate(original, true)!!; \
             const relaxed = decorate(original, false)!!; \
             io::println(strict.headers.len()); io::println(relaxed.headers.len()); \
             io::println(original.headers.len()); }}"
        )),
        "6\n5\n0\n"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "mutability analysis took {:?}",
        started.elapsed()
    );
    rejects(
        &format!(
            "{helpers} fn main() {{ \
             const original = Response(status: 200, headers: collections::Map::new[str, str](), body: \"\"); \
             original.headers.set(\"x\", \"y\")!!; }}"
        ),
        "cannot mutate",
    );
}

/// A `main` in which rows keep flowing between `size` locals, so every row can
/// have reached any local through any subset of the others.
fn row_ring(size: usize, setup: &str, end: &str) -> String {
    let mut body = format!("fn main() {{ {setup} var seed = [[0]]; seed.append([1]);");
    for i in 0..size {
        body.push_str(&format!(
            "var a{i}: int[][] = []; a{i}.append(seed[0]!!)!!;"
        ));
    }
    body.push_str("for _round in 0..3 {");
    for i in 0..size {
        for step in [1, 3] {
            let target = (i + step) % size;
            body.push_str(&format!("a{target}.append(a{i}[0]!!)!!;"));
        }
    }
    body.push_str(&format!("}} {end} }}"));
    body
}

#[test]
fn recombined_alias_origins_check_quickly_and_keep_capabilities() {
    // Keeping each combination of origins apart took minutes at this size.
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert_eq!(
            run(&row_ring(
                16,
                "",
                "a7[0]!!.append(9)!!; io::println(seed[0]!!);"
            )),
            "[0, 9]\n"
        );
        rejects(
            &row_ring(
                16,
                "const frozen = [7];",
                "a0.append(frozen)!!; a1.append(a0[1]!!)!!; a1[0]!!.append(9)!!; a1[7]!!.append(9)!!;",
            ),
            "cannot mutate through const/read-only access from `frozen`",
        );
        done.send(()).unwrap();
    });
    if finished.recv_timeout(std::time::Duration::from_secs(60))
        == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    {
        panic!("mutability analysis exceeded its time ceiling");
    }
    worker.join().unwrap();
}

#[test]
fn merged_alias_origins_still_justify_vars_and_block_strong_updates() {
    // `p` stands for one array reached through either `x` or `y`.
    assert_eq!(
        run(
            "fn f(flag: bool) { var inner = [[1]]; var x = inner; var y = inner; \
             var p = if flag { x } else { y }; p[0] = [2]; p[0]!!.append(3)!!; \
             io::println(inner); } fn main() { f(true); }"
        ),
        "[[2, 3]]\n"
    );
    // An element overwritten through two origins keeps its older capability.
    rejects(
        "fn f(flag: bool) { const frozen = [1]; var inner = [frozen]; var x = inner; var y = inner; \
         var p = if flag { x } else { y }; p[0] = [2]; p[0]!!.append(3)!!; } fn main() { f(true); }",
        "cannot mutate through const/read-only access from `frozen`",
    );
}

#[test]
fn local_constants_and_default_arguments_are_checked_in_declaration_context() {
    rejects(
        "fn mutate(a) { const b = a; b.append(2); } fn main() { var a = [1]; mutate(a); }",
        "cannot mutate",
    );
    rejects(
        "const DEFAULT = [1]; fn mutate(a = DEFAULT) { a.append(2); } fn main() { mutate(); }",
        "cannot mutate",
    );
    rejects(
        "fn mutate(a) { a.append(2); } fn main() { const a = [1]; mutate(a: a); }",
        "cannot mutate",
    );
    assert_eq!(
        run("fn mutate(a = [1]) { a.append(2); a } fn main() { io::println(mutate()); }"),
        "[1, 2]\n"
    );
}

#[test]
fn method_default_arguments_preserve_mutation_and_return_capabilities() {
    rejects(
        "const DEFAULT = [1]; struct Box { value: int } extend Box { fn change(self, values: int[] = DEFAULT) { values.append(2); } } fn main() { const box = Box(1); box.change(); }",
        "cannot mutate",
    );
    rejects(
        "const DEFAULT = [1]; struct Box { value: int } extend Box { fn identity(self, values: int[] = DEFAULT) -> int[] { values } } fn main() { const box = Box(1); box.identity().append(2); }",
        "cannot mutate",
    );
    rejects(
        "const DEFAULT = [1]; fn change(values: int[]) -> int[] { values.append(2); values } struct Box { value: int } extend Box { fn consume(self, values: int[] = change(DEFAULT)) {} } fn main() { const box = Box(1); box.consume(); }",
        "cannot mutate",
    );
    rejects(
        "const DEFAULT = [1]; struct Box[T: type] { value: T } extend[T: type] Box[T] { fn change(self, values: int[] = DEFAULT) { values.append(2); } } fn main() { const box = Box(1); box.change(); }",
        "cannot mutate",
    );
    rejects(
        "const DEFAULT = [1]; struct Box { value: int } extend Box { @async fn change(self, values: int[] = DEFAULT) { values.append(2); } } @async fn main() { const box = Box(1); await box.change(); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "struct Box { value: int } extend Box { fn change(self, values: int[] = [1]) -> int[] { values.append(2); values } } fn main() { const box = Box(1); io::println(box.change()); }"
        ),
        "[1, 2]\n"
    );
}

#[test]
fn custom_iterator_items_preserve_capabilities_in_single_and_zip_loops() {
    let declarations = r#"
        use iter;
        const protected = [10];
        struct Source { values: int[] }
        struct Cursor { values: int[], position: int }
        extend Source with iter::IntoIterator[Cursor, int[]] {
            fn into_iter(self) -> Cursor { Cursor(self.values, 0) }
        }
        extend Cursor with iter::Iterator[int[]] {
            fn next(mut self) -> bool {
                if self.position > 0 { return false; }
                self.position += 1;
                true
            }
            fn current(self) -> int[] { self.values }
        }
    "#;
    for loop_body in [
        "for value in Source(protected) { value.append(99); }",
        "for index in 0..1, value in Source(protected) { value.append(index); }",
    ] {
        rejects(
            &format!("{declarations} fn main() {{ {loop_body} }}"),
            "cannot mutate",
        );
    }
    assert_eq!(
        run(&format!(
            "{declarations} fn main() {{ for value in Source([10]) {{ value.append(99); io::println(value); }} }}"
        )),
        "[10, 99]\n"
    );
}

#[test]
fn custom_iterator_protocol_effects_are_checked_before_loop_body() {
    let declarations = r#"
        use iter;
        const protected = [10];
        struct Source { values: int[] }
        struct Cursor { values: int[], position: int }
        extend Source with iter::IntoIterator[Cursor, int[]] {
            fn into_iter(self) -> Cursor { Cursor(self.values, 0) }
        }
        extend Cursor with iter::Iterator[int[]] {
            fn next(mut self) -> bool {
                if self.position > 0 { return false; }
                self.position += 1;
                self.values = protected;
                true
            }
            fn current(self) -> int[] { self.values }
        }
    "#;
    rejects(
        &format!("{declarations} fn main() {{ for value in Source([1]) {{ value.append(99); }} }}"),
        "cannot mutate",
    );
    rejects(
        &format!(
            "{declarations} fn main() {{ for index in 0..1, value in Source([1]) {{ value.append(index); }} }}"
        ),
        "cannot mutate",
    );
}

#[test]
fn custom_iterator_current_cannot_launder_a_global_const() {
    rejects(
        r#"
        use iter;
        const protected = [10];
        struct Source { value: int }
        struct Cursor { position: int }
        extend Source with iter::IntoIterator[Cursor, int[]] {
            fn into_iter(self) -> Cursor { Cursor(self.value) }
        }
        extend Cursor with iter::Iterator[int[]] {
            fn next(mut self) -> bool {
                if self.position > 0 { return false; }
                self.position += 1;
                true
            }
            fn current(self) -> int[] { protected }
        }
        fn main() { for value in Source(0) { value.append(99); } }
        "#,
        "cannot mutate",
    );
}

#[test]
fn custom_iterator_protocol_mutations_respect_captured_capabilities() {
    for (into_iter, next, current) in [
        ("self.values.append(2);", "", ""),
        ("", "self.values.append(2);", ""),
        ("", "", "self.values.append(2);"),
    ] {
        let program = format!(
            r#"
            use iter;
            const protected = [10];
            struct Source {{ values: int[] }}
            struct Cursor {{ values: int[], position: int }}
            extend Source with iter::IntoIterator[Cursor, int] {{
                fn into_iter(self) -> Cursor {{ {into_iter} Cursor(self.values, 0) }}
            }}
            extend Cursor with iter::Iterator[int] {{
                fn next(mut self) -> bool {{
                    {next}
                    if self.position > 0 {{ return false; }}
                    self.position += 1;
                    true
                }}
                fn current(self) -> int {{ {current} self.position }}
            }}
            fn main() {{ for value in Source(protected) {{ value; }} }}
            "#
        );
        rejects(&program, "cannot mutate");
    }
}

#[test]
fn generic_iterator_specialization_retains_item_capabilities() {
    let declarations = r#"
        use iter;
        const protected = [10];
        struct Source[T: type] { value: T }
        struct Cursor[T: type] { value: T, position: int }
        extend[T: type] Source[T] with iter::IntoIterator[Cursor[T], T] {
            fn into_iter(self) -> Cursor[T] { Cursor(self.value, 0) }
        }
        extend[T: type] Cursor[T] with iter::Iterator[T] {
            fn next(mut self) -> bool {
                if self.position > 0 { return false; }
                self.position += 1;
                true
            }
            fn current(self) -> T { self.value }
        }
    "#;
    rejects(
        &format!(
            "{declarations} fn main() {{ for value in Source(protected) {{ value.append(99); }} }}"
        ),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{declarations} fn main() {{ for value in Source([10]) {{ value.append(99); io::println(value); }} }}"
        )),
        "[10, 99]\n"
    );
}

#[test]
fn iterator_effects_remain_visible_after_break() {
    rejects(
        r#"
        use iter;
        var shared = [1];
        const protected = [10];
        struct Source { value: int }
        struct Cursor { position: int }
        extend Source with iter::IntoIterator[Cursor, int] {
            fn into_iter(self) -> Cursor { Cursor(self.value) }
        }
        extend Cursor with iter::Iterator[int] {
            fn next(mut self) -> bool { self.position += 1; true }
            fn current(self) -> int { shared = protected; self.position }
        }
        fn main() { for value in Source(0) { break; } shared.append(99); }
        "#,
        "cannot mutate",
    );
}

#[test]
fn zip_skips_iterator_steps_after_a_known_empty_range() {
    let declarations = r#"
        use iter;
        const protected = [10];
        struct Source { values: int[] }
        struct Cursor { values: int[] }
        extend Source with iter::IntoIterator[Cursor, int] {
            fn into_iter(self) -> Cursor { Cursor(self.values) }
        }
        extend Cursor with iter::Iterator[int] {
            fn next(mut self) -> bool { self.values.append(99); self.values = [1]; false }
            fn current(self) -> int { 0 }
        }
    "#;
    run(&format!(
        "{declarations} fn main() {{ for index in 0..0, value in Source(protected) {{ index; value; }} }}"
    ));
    rejects(
        &format!(
            "{declarations} fn main() {{ for value in Source(protected), index in 0..0 {{ value; index; }} }}"
        ),
        "cannot mutate",
    );
}

#[test]
fn iteration_rechecks_elements_after_the_collection_changes() {
    for loop_body in [
        "for value in arrays { value.append(99); arrays[1] = protected; }",
        "for index in 0..2, value in arrays { value.append(index); arrays[1] = protected; }",
    ] {
        rejects(
            &format!(
                "fn main() {{ const protected = [10]; var arrays = [[1], [2]]; {loop_body} }}"
            ),
            "cannot mutate",
        );
    }
    assert_eq!(
        run(
            "fn main() { var arrays = [[1], [2]]; for value in arrays { value.append(3); } io::println(arrays); }"
        ),
        "[[1, 3], [2, 3]]\n"
    );
}

#[test]
fn native_heap_mutations_are_distinct_from_resource_operations() {
    rejects(
        "use bytes; fn main() { const b = bytes::new(1); bytes::set(b, 0, 65); }",
        "cannot mutate",
    );
    rejects(
        "use bytes; fn main() { const b = bytes::new(1); io::read_into(io::stdin(), b, 0, 1); }",
        "cannot mutate",
    );
    rejects(
        "use str; use fmt; fn main() { const b = str::builder(); fmt::write(b, \"%\", 1); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "use bytes; fn main() { var b = bytes::new(1); bytes::set(b, 0, 65); const out = io::stdout(); io::write_all(out, b, 0, 1); io::flush(out); }"
        ),
        "A"
    );
}

#[test]
fn reachable_mutations_are_counted_with_shadowing_lazy_paths_and_terminal_calls() {
    for body in [
        "fn main() { var a = [1]; while false { a.append(2); } }",
        "fn main() { var a = [1]; for i in 0..0 { a.append(i); } }",
        "fn main() { var a = [1]; crash(\"stop\"); a.append(2); }",
        "fn main() { var a = 1; if true { var a = 2; a = 3; } io::println(a); }",
        "fn main() { var a = [1]; false && (if true { a.append(2); true } else { false }); }",
        "fn main() { var a = [1]; 1 ?? (if true { a.append(2); 2 } else { 2 }); }",
        "fn main() { var a = [1]; const x: int = 2; x ?? (if true { a.append(2); 2 } else { 2 }); }",
    ] {
        rejects(body, "requires a reachable mutation");
    }
    assert_eq!(
        run("fn main() { var a = [1]; defer { a.append(2); io::println(a); } }"),
        "[1, 2]\n"
    );
}

#[test]
fn defer_uses_capability_at_exit_and_retains_lexical_capture() {
    rejects(
        "fn main() { var a = [1]; const b = [2]; defer { a.append(3); } a = b; }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn main() { var a = [1]; defer { a.append(3); io::println(a); } if true { const a = [2]; io::println(a); } }"
        ),
        "[2]\n[1, 3]\n"
    );
}

#[test]
fn named_sequence_methods_evaluate_once_and_return_the_mutated_array() {
    assert_eq!(
        run(
            "var calls = 0; var a = [2]; fn get() { calls += 1; a } fn main() { io::println(get().append(3).prepend(1)); io::println(calls); io::println(a); }"
        ),
        "[1, 2, 3]\n1\n[1, 2, 3]\n"
    );
    rejects(
        "fn main() { const a = [1]; a.prepend(2); }",
        "cannot mutate",
    );
    rejects("fn main() { 1.append(2); }", "array receiver");
}

#[test]
fn capabilities_follow_global_reassignment_across_calls() {
    rejects(
        "var global = [1]; fn set(a) { global = a; } fn change() { global.append(2); } fn main() { const a = [3]; set(a); change(); }",
        "cannot mutate",
    );
    rejects(
        "const a = [1]; var global = a; fn main() { global.append(2); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "var global = [1]; fn change() { global.append(2); } fn main() { const view = global; change(); io::println(view); }"
        ),
        "[1, 2]\n"
    );
}

#[test]
fn closure_heap_and_global_stores_preserve_readonly_aliases() {
    for body in [
        r#"
        fn main() {
            const protected = [10];
            var holder = {item: [1]};
            const install = fn[var holder, protected]() -> nil { holder.item = protected; };
            (install)();
            holder.item.append(99);
        }
        "#,
        r#"
        fn make(holder, value) -> fn() -> nil {
            var current = holder;
            const install = fn[var current, value]() -> nil { current.item = value; };
            install
        }
        fn main() {
            const protected = [10];
            var holder = {item: [1]};
            const install = make(holder, protected);
            (install)();
            holder.item.append(99);
        }
        "#,
        r#"
        var shared = [1];
        fn main() {
            const protected = [10];
            const install = fn[protected]() -> nil { shared = protected; };
            (install)();
            shared.append(99);
        }
        "#,
        r#"
        fn main() {
            const protected = [10];
            var holder = {item: [1]};
            const install = fn[var holder, protected](flag: bool) -> nil {
                if flag { holder.item = protected; return; }
            };
            (install)(true);
            holder.item.append(99);
        }
        "#,
    ] {
        rejects(body, "cannot mutate");
    }
    assert_eq!(
        run(r#"
        fn main() {
            var holder = {item: [1]};
            var value = [10];
            const install = fn[var holder, var value]() -> nil { holder.item = value; };
            (install)();
            holder.item.append(99);
            io::println(value);
        }
        "#),
        "[10, 99]\n"
    );
}

#[test]
fn named_function_values_preserve_global_effects_through_indirect_calls() {
    for invocation in [
        "const callback: fn() -> nil = change; (callback)();",
        "invoke(change);",
        "const holder = Holder(change); (holder.callback)();",
        "const callback = select(); (callback)();",
    ] {
        rejects(
            &format!(
                r#"
                const protected = [10];
                var shared = [1];
                struct Holder {{ callback: fn() -> nil }}
                fn change() -> nil {{ shared = protected; }}
                fn invoke(callback: fn() -> nil) -> nil {{ (callback)(); }}
                fn select() -> fn() -> nil {{ change }}
                fn main() {{ {invocation} shared.append(99); }}
                "#
            ),
            "cannot mutate",
        );
    }
    assert_eq!(
        run(
            "fn increment(value: int) -> int { value + 1 } fn main() { const callback: fn(int) -> int = increment; io::println((callback)(41)); }"
        ),
        "42\n"
    );
    assert_eq!(
        run(
            "const protected = [10]; var shared = [1]; fn change() -> nil { shared = protected; } fn noop() -> nil {} fn main() { const unused: fn() -> nil = change; const callback: fn() -> nil = noop; (callback)(); shared.append(2); io::println(shared); }"
        ),
        "[1, 2]\n"
    );
    rejects(
        "const protected = [10]; var shared = [1]; struct Holder { item: int[] } fn change(holder: Holder) -> nil { shared = holder.item; } fn main() { const callback: fn(Holder) -> nil = change; (callback)(Holder(protected)); shared.append(99); }",
        "cannot mutate",
    );
    rejects(
        "const protected = [10]; var shared = [1]; struct Holder { item: int[] } fn change(holder: Holder) -> nil { shared = holder.item; } fn invoke(callback: fn(Holder) -> nil, holder: Holder) -> nil { (callback)(holder); } fn main() { invoke(change, Holder(protected)); shared.append(99); }",
        "cannot mutate",
    );
}

#[test]
fn immutable_function_values_can_replace_a_mutable_capture() {
    assert_eq!(
        run(r#"
        fn first() -> int { 1 }
        fn second() -> int { 2 }
        fn main() {
            var callback: fn() -> int = first;
            const other: fn() -> int = second;
            const replace = fn[var callback, other]() -> nil { callback = other; };
            (replace)();
            io::println((callback)());
        }
        "#),
        "2\n"
    );
}

#[test]
fn closures_cannot_mutate_readonly_aliases_stored_after_creation() {
    for body in [
        r#"
        var shared = [1];
        fn main() {
            const callback = fn[]() -> nil { shared.append(99); };
            const protected = [10];
            shared = protected;
            (callback)();
        }
        "#,
        r#"
        var shared = [1];
        fn make() -> fn() -> nil {
            const callback = fn[]() -> nil { shared.append(99); };
            callback
        }
        fn main() {
            const callback = make();
            const protected = [10];
            shared = protected;
            (callback)();
        }
        "#,
        r#"
        fn main() {
            var holder = {item: [1]};
            const callback = fn[var holder]() -> nil { holder.item.append(99); };
            const protected = [10];
            holder.item = protected;
            (callback)();
        }
        "#,
        r#"
        var shared = [1];
        fn main() {
            const protected = [10];
            const callback = fn[protected]() -> nil { shared = protected; };
            shared = [2];
            (callback)();
            shared.append(99);
        }
        "#,
        r#"
        fn make(holder) -> fn() -> nil {
            var current = holder;
            const callback = fn[var current]() -> nil { current.item.append(99); };
            callback
        }
        fn main() {
            var holder = {item: [1]};
            const callback = make(holder);
            const protected = [10];
            holder.item = protected;
            (callback)();
        }
        "#,
    ] {
        rejects(body, "read-only");
    }
    assert_eq!(
        run(
            "fn main() { var holder = {item: [1]}; const callback = fn[var holder]() -> nil { holder.item.append(99); }; holder.item = [2]; (callback)(); io::println(holder.item); }"
        ),
        "[2, 99]\n"
    );
}

#[test]
fn terminal_helpers_and_infinite_loops_do_not_justify_unreachable_writes() {
    rejects(
        "fn stop() { crash(\"stop\"); } fn main() { var a = [1]; stop(); a.append(2); }",
        "requires a reachable mutation",
    );
    rejects(
        "fn main() { var a = [1]; while true {} a.append(2); }",
        "requires a reachable mutation",
    );
    rejects(
        "fn main() { var a = [1]; defer { a.append(2); } while true {} }",
        "requires a reachable mutation",
    );
    assert_eq!(
        run("fn main() { var a = [1]; while true { a.append(2); break; } io::println(a); }"),
        "[1, 2]\n"
    );
}

#[test]
fn aliases_stored_by_helpers_keep_capabilities_and_parameter_aliasing_is_checked() {
    rejects(
        "fn save(holder, value) { holder.item = value; } fn main() { var holder = { item: [1] }; const value = [2]; save(holder, value); holder.item.append(3); }",
        "cannot mutate",
    );
    rejects(
        "fn save_and_change(p, q, value) { p.item = value; q.item.append(3); } fn main() { var holder = { item: [1] }; const value = [2]; save_and_change(holder, holder, value); }",
        "cannot mutate",
    );
    rejects(
        "fn boxed(a, flag: bool) { if flag { return { item: a }; } { item: [1] } } fn main() { const a = [2]; var b = boxed(a, true); b.item.append(3); }",
        "cannot mutate",
    );
}

#[test]
fn catch_entry_keeps_capabilities_from_interrupted_prefixes() {
    rejects(
        "fn fail() { 1 / 0 } fn main() { var b = [1]; const a = [2]; try { b = a; fail(); b = [3]; } catch(e) { b.append(4); } }",
        "cannot mutate",
    );
}

#[test]
fn scalar_copies_do_not_inherit_readonly_reference_capability() {
    assert_eq!(
        run(
            "fn increase(x: int) -> int { x += 1; x } fn main() { const a = 1; var b = a; b = increase(b); io::println(a); io::println(b); }"
        ),
        "1\n2\n"
    );
}

#[test]
fn recursive_projection_widening_keeps_nested_readonly_access() {
    rejects(
        "fn walk(node, depth: int) { if depth > 0 { walk(node.next, depth - 1); } else { node.item.append(2); } } fn main() { const a = [1]; var node = { next: { next: { next: { next: { next: { item: a } } } } } }; walk(node, 5); }",
        "cannot mutate",
    );
}

#[test]
fn shift_compound_assignments_reject_arrays_and_array_addition_retains_child_capabilities() {
    for (op, method) in [("<<=", "append"), (">>=", "prepend")] {
        let body = format!("fn main() {{ var values = [1]; values {op} 2; }}");
        let error = project::compile_source("mutability.cog", &source(&body))
            .expect_err("shifts do not mutate arrays")
            .to_string();
        assert!(error.contains("E2002"), "{error}");
        assert!(error.contains(&format!("array.{method}(value)")), "{error}");
    }
    rejects(
        "fn main() { const a = [[1]]; var b = a; b += []; b[0].append(2); }",
        "cannot mutate",
    );
    assert_eq!(
        run(
            "fn main() { const a = [1]; var b = a; b += [2]; b.append(3); io::println(a); io::println(b); }"
        ),
        "[1]\n[1, 2, 3]\n"
    );
}

#[test]
fn invalid_initializers_do_not_cascade_into_required_mutation_errors() {
    let error = project::compile_source(
        "mutability.cog",
        &source("fn main() { var bad: int = \"text\"; const a = [1]; a.append(2); missing(); }"),
    )
    .expect_err("independent semantic errors")
    .to_string();
    for message in ["type mismatch", "cannot mutate", "unresolved name"] {
        assert!(error.contains(message), "{error}");
    }
    assert!(
        !error.contains("E1006"),
        "invalid binding must not cascade: {error}"
    );
    rejects(
        "fn main() { const a = [1]; if false { var b = [2]; b = a; b.append(3); } }",
        "cannot mutate",
    );
}

#[test]
fn returns_in_loop_conditions_and_known_nonempty_ranges_stop_successor_mutations() {
    assert_eq!(
        run(
            "fn main() { var a = [1]; for i in 0..1 { const a = [2]; io::println(a); break; } a.append(3); io::println(a); }"
        ),
        "[2]\n[1, 3]\n"
    );
    rejects(
        "fn main() { var a = [1]; while (if true { return; } else { true }) {} a.append(2); }",
        "requires a reachable mutation",
    );
    rejects(
        "fn main() { var a = [1]; for i in 0..1 { return; } a.append(2); }",
        "requires a reachable mutation",
    );
}

#[test]
fn repeated_allocations_do_not_clear_capabilities_of_escaped_older_objects() {
    rejects(
        "fn main() { var saved = { item: [0] }; const a = [1]; for i in 0..2 { var fresh = { item: [2] }; if i == 0 { fresh.item = a; saved = fresh; } else { saved.item.append(3); } } }",
        "cannot mutate",
    );
    rejects(
        "fn make() { { item: [2] } } fn main() { var saved = { item: [0] }; const a = [1]; for i in 0..2 { var fresh = make(); if i == 0 { fresh.item = a; saved = fresh; } else { saved.item.append(3); } } }",
        "cannot mutate",
    );
}

/// Helpers whose mutation effect on the first parameter becomes visible only
/// after several fixed-point iterations; every level needs one more iteration
/// than its callee.
const LATE_CHAIN: &str = "fn level0(a: int[]) { a.append(2); } fn level1(a: int[]) { level0(a); } \
     fn level2(a: int[]) { level1(a); } fn level3(a: int[]) { level2(a); } \
     fn level4(a: int[]) { level3(a); }";

/// Helpers that return their argument; the alias becomes visible one level
/// per fixed-point iteration.
const LATE_ALIAS: &str = "fn bottom(a: int[]) -> int[] { a } fn middle(a: int[]) -> int[] { bottom(a) } \
     fn top(a: int[]) -> int[] { middle(a) }";

#[test]
fn indirect_calls_depend_on_late_settling_function_value_candidates() {
    // Arguments of indirect calls are treated as mutated conservatively, so the
    // late effect travels through a global that the target rebinds.
    let chain = "const protected = [10]; var shared = [1]; \
         fn step0() -> nil { SET } fn step1() -> nil { step0(); } \
         fn step2() -> nil { step1(); } fn step3() -> nil { step2(); } \
         fn step4() -> nil { step3(); }";
    for (apply, call) in [
        (
            "fn apply(f: fn() -> nil) -> nil { (f)(); }",
            "apply(step4);",
        ),
        (
            "fn apply() -> nil { const f: fn() -> nil = step4; (f)(); }",
            "apply();",
        ),
    ] {
        rejects(
            &format!(
                "{} {apply} fn main() {{ {call} shared.append(99); }}",
                chain.replace("SET", "shared = protected;")
            ),
            "cannot mutate",
        );
        assert_eq!(
            run(&format!(
                "{} {apply} fn main() {{ {call} shared.append(99); io::println(shared); }}",
                chain.replace("SET", "shared = [5];")
            )),
            "[5, 99]\n",
            "{call}"
        );
    }
}

#[test]
fn closures_depend_on_late_settling_helpers_called_in_their_bodies() {
    let wrapper = "fn wrapper(a: int[]) { var local = a; \
                   const f = fn[var local]() -> int { level4(local); 0 }; (f)(); }";
    rejects(
        &format!("{LATE_CHAIN} {wrapper} fn main() {{ const a = [1]; wrapper(a); }}"),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{LATE_CHAIN} {wrapper} fn main() {{ var a = [1]; wrapper(a); io::println(a); }}"
        )),
        "[1, 2]\n"
    );
}

#[test]
fn user_defined_iteration_depends_on_late_settling_protocol_helpers() {
    for protocol in ["next", "current"] {
        let (next, current) = if protocol == "next" {
            ("level4(self.values);", "")
        } else {
            ("", "level4(self.values);")
        };
        let declarations = format!(
            r#"
            use iter;
            {LATE_CHAIN}
            struct Source {{ values: int[] }}
            struct Cursor {{ values: int[], position: int }}
            extend Source with iter::IntoIterator[Cursor, int] {{
                fn into_iter(self) -> Cursor {{ Cursor(self.values, 0) }}
            }}
            extend Cursor with iter::Iterator[int] {{
                fn next(mut self) -> bool {{
                    {next}
                    if self.position > 0 {{ return false; }}
                    self.position += 1;
                    true
                }}
                fn current(self) -> int {{ {current} self.position }}
            }}
            fn drain(source: Source) {{ for value in source {{ value; }} }}
            "#
        );
        rejects(
            &format!("{declarations} fn main() {{ const a = [1]; drain(Source(a)); }}"),
            "cannot mutate",
        );
        assert_eq!(
            run(&format!(
                "{declarations} fn main() {{ var a = [1]; drain(Source(a)); io::println(a); }}"
            )),
            if protocol == "next" {
                "[1, 2, 2]\n"
            } else {
                "[1, 2]\n"
            },
            "{protocol}"
        );
    }
}

#[test]
fn mutual_recursion_exposes_effects_after_extra_iterations() {
    let cycle = "fn ping(a: int[], n: int) { if n > 0 { pong(a, n - 1); } } \
                 fn pong(a: int[], n: int) { level4(a); ping(a, n); }";
    rejects(
        &format!("{LATE_CHAIN} {cycle} fn main() {{ const a = [1]; ping(a, 1); }}"),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{LATE_CHAIN} {cycle} fn main() {{ var a = [1]; ping(a, 1); io::println(a); }}"
        )),
        "[1, 2]\n"
    );
}

#[test]
fn global_initializers_depend_on_late_settling_alias_helpers() {
    // The global aliases a constant only through a late-settling helper chain;
    // the reader's returned capability must follow once that alias is visible.
    rejects(
        &format!(
            "{LATE_ALIAS} const protected = [1]; var shared: int[] = top(protected); \
             fn reader() -> int[] {{ shared }} fn main() {{ reader().append(2); }}"
        ),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{LATE_ALIAS} var base: int[] = [1]; var shared: int[] = top(base); \
             fn reader() -> int[] {{ shared }} \
             fn main() {{ reader().append(2); io::println(base); }}"
        )),
        "[1, 2]\n"
    );
}

#[test]
fn cloned_array_from_generic_clone_is_a_valid_mutation_target() {
    assert_eq!(
        run("use clone; fn main() { const arr: int[] = [1, 2, 3]; \
             var x = clone::value(arr)!!; x.append(4); io::println(x.len()); }"),
        "4\n"
    );
    assert_eq!(
        run(
            "use clone; fn dup[T: type + clone::Clone](a: T) -> T { a.clone() } \
             fn main() { const arr: int[] = [1]; var x = dup(arr); x.append(2); io::println(x); }"
        ),
        "[1, 2]\n"
    );
}

#[test]
fn mutating_generic_clone_leaves_the_original_unchanged() {
    assert_eq!(
        run("use clone; fn main() { const arr: int[] = [1, 2, 3]; \
             var x = clone::value(arr)!!; x.append(4); \
             io::println(arr); io::println(x); }"),
        "[1, 2, 3]\n[1, 2, 3, 4]\n"
    );
}

#[test]
fn generic_alias_of_const_array_is_still_rejected_for_mutation() {
    rejects(
        "fn id[T: type](a: T) -> T { a } \
         fn main() { const arr: int[] = [1]; var x = id(arr); x.append(2); }",
        "cannot mutate",
    );
    rejects(
        "use clone; fn main() { const arr: int[] = [1]; \
         const x = clone::value(arr)!!; x.append(2); }",
        "cannot mutate",
    );
}

#[test]
fn unmutated_var_passed_to_generic_function_is_still_reported() {
    rejects(
        "fn noop[T: type](a: T) -> nil { nil } fn main() { var v = [1]; noop(v); }",
        "E1006",
    );
}

#[test]
fn local_mutated_only_through_generic_callee_needs_no_later_mutation() {
    assert_eq!(
        run(
            "use random; fn main() { var rng = random::Seeded::new(1); var items = [1, 2, 3]; \
             random::shuffle(rng, items)!!; io::println(items.len()); }"
        ),
        "3\n"
    );
}

const PICK: &str = "struct P { n: int } fn one(v: int) -> int { v } \
    fn read_n(p: P) -> int { p.n } fn bump(p: P) -> int { p.n += 1; p.n } \
    fn pick[T: type](items: T[], f: fn(T) -> int) -> int { \
        var item = items[0]!!; (f)(item)!! }";

#[test]
fn generic_var_passed_to_a_function_value_is_valid_for_every_instantiation() {
    for (call, expected) in [
        ("const xs = [1]; io::println(pick(xs, one));", "1\n"),
        ("var ps = [P(n: 2)]; io::println(pick(ps, read_n));", "2\n"),
        (
            "const xs = [1]; var ps = [P(n: 2)]; io::println(pick(xs, one)); \
             io::println(pick(ps, bump)); io::println(ps[0].n);",
            "1\n3\n3\n",
        ),
    ] {
        assert_eq!(
            run(&format!("{PICK} fn main() {{ {call} }}")),
            expected,
            "{call}"
        );
    }
}

#[test]
fn generic_var_is_valid_in_methods_closures_and_nested_generics_over_scalars() {
    assert_eq!(
        run(r#"
        fn one(v: int) -> int { v }
        struct Slot[T: type] { value: T }
        extend[T: type] Slot[T] {
            fn apply(self, f: fn(T) -> int) -> int { var item = self.value; (f)(item)!! }
        }
        fn inner[T: type](x: T, f: fn(T) -> int) -> int { var y = x; (f)(y)!! }
        fn outer[U: type](x: U, f: fn(U) -> int) -> int {
            const g = fn[x, f]() -> int { var z = x; inner(z, f)!! };
            (g)()!!
        }
        fn main() {
            const slot = Slot[int](value: 4);
            io::println(slot.apply(one));
            io::println(outer(5, one));
        }
    "#),
        "4\n5\n"
    );
}

#[test]
fn generic_bodies_still_reject_mutation_through_const_access() {
    // A const binding cannot reach a function value once T is a reference.
    rejects(
        &format!(
            "{PICK} fn peek[T: type](items: T[], f: fn(T) -> int) -> int {{ \
                 const item = items[0]!!; (f)(item)!! }} \
             fn main() {{ var ps = [P(n: 1)]; peek(ps, bump); }}"
        ),
        "cannot mutate through const/read-only access from `item`",
    );
    // A generic var must not launder the caller's const array.
    for call in ["pick(ps, bump)", "pick(ps, read_n)"] {
        rejects(
            &format!("{PICK} fn main() {{ const ps = [P(n: 1)]; {call}; }}"),
            "cannot mutate",
        );
    }
    rejects(
        "fn grow[T: type](items: T[]) { var other = items; other.append(items[0]!!); } \
         fn main() { const xs = [1]; grow(xs); }",
        "cannot mutate",
    );
    rejects(
        "fn grow[T: type](items: T[]) { const other = items; other.append(items[0]!!); } \
         fn main() { const xs = [1]; grow(xs); }",
        "cannot mutate through const/read-only access from `other`",
    );
}

#[test]
fn unneeded_var_in_a_generic_body_is_still_reported() {
    for body in [
        "var y = x; y",
        "var count = 0; count; x",
        "var v = [1]; noop(v); x",
        "var y = x; if false { y = x; } y",
    ] {
        rejects(
            &format!(
                "fn noop[T: type](a: T) -> nil {{ nil }} \
                 fn go[T: type](x: T) -> T {{ {body} }} fn main() {{ io::println(go(1)); }}"
            ),
            "requires a reachable mutation",
        );
    }
}

#[test]
fn mutation_through_nested_generic_instances_is_checked_on_the_final_program() {
    let chain = "fn h[T: type](xs: T[], x: T) -> nil { xs.append(x); } \
                 fn g[T: type](xs: T[], x: T) -> nil { h(xs, x); } \
                 fn f[T: type](xs: T[], x: T) -> nil { g(xs, x); }";
    rejects(
        &format!("{chain} fn main() {{ const a = [1]; f(a, 2); }}"),
        "cannot mutate",
    );
    assert_eq!(
        run(&format!(
            "{chain} fn main() {{ var a = [1]; f(a, 2); io::println(a); }}"
        )),
        "[1, 2]\n"
    );
    rejects(
        "fn h[T: type](xs: T[]) -> int { xs.len() } fn g[T: type](xs: T[]) -> int { h(xs) } \
         fn main() { var a = [1]; io::println(g(a)); }",
        "E1006",
    );
    rejects(
        "trait Touch { fn touch(self) -> nil; } struct Bag { items: int[] } \
         extend Bag with Touch { fn touch(self) -> nil { self.items.append(1); } } \
         fn apply[T: type + Touch](x: T) -> nil { x.touch(); } \
         fn outer[T: type + Touch](x: T) -> nil { apply(x); } \
         fn main() { const b = Bag([1]); outer(b); }",
        "cannot mutate",
    );
}

#[test]
fn mutation_errors_are_reported_with_errors_from_later_specialization_rounds() {
    let error = project::compile_source(
        "mutability.cog",
        &source(
            "fn h[T: type](x: T) -> int { x + \"s\" } fn g[T: type](x: T) -> int { h(x) } \
             fn main() { const a = [1]; a.append(2); io::println(g(1)); }",
        ),
    )
    .expect_err("must fail semantic checking")
    .to_string();
    assert!(error.contains("cannot mutate"), "{error}");
    assert!(error.contains("invalid operands int and str"), "{error}");
}

#[test]
fn var_mutated_only_by_a_late_generic_instance_is_accepted() {
    let chain = "struct P { n: int } extend P { fn bump(self) -> nil { self.n += 1; } } \
                 fn inner[T: type](x: T) -> nil { x.bump(); } \
                 fn outer[T: type](x: T) -> nil { inner(x); }";
    assert_eq!(
        run(&format!(
            "{chain} fn main() {{ var v = P(n: 1); outer(v); io::println(v.n); }}"
        )),
        "2\n"
    );
    rejects(
        &format!("{chain} fn main() {{ const v = P(n: 1); outer(v); }}"),
        "cannot mutate through const/read-only access from `v`",
    );
}

#[test]
fn pending_clone_results_add_no_noise_to_a_failing_round() {
    // The clone instances do not exist yet when the first round fails, so
    // their results must still count as writable independent values.
    for body in [
        "const a = [1]; var c = clone::value(a); c.append(2);",
        "const a = [[1]]; var c = clone::value(a); c[0]!!.append(2);",
    ] {
        let error = project::compile_source(
            "mutability.cog",
            &source(&format!("use clone; fn main() {{ {body} 1 + \"s\"; }}")),
        )
        .expect_err("must fail semantic checking")
        .to_string();
        assert!(error.contains("invalid operands int and str"), "{error}");
        assert!(!error.contains("E1006"), "{error}");
        assert!(!error.contains("E1003"), "{error}");
    }
}
