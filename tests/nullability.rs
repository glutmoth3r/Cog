//! General nullable types and flow narrowing.
use cog::{bytecode, interpreter, project};

const PRELUDE: &str = r#"@!include("std") use io;
struct Node { value: int, next: Node? }
enum Color { Red, Green }
extend Node { fn double(self) -> int { self.value * 2 } }
fn find(found: bool) -> Node? { if found { Node(value: 1, next: nil) } else { nil } }
fn need(node: Node) -> int { node.value }
fn touch() {}
"#;

fn source(body: &str) -> String {
    format!("{PRELUDE}\n{body}")
}

fn run(body: &str) -> String {
    let compiled = project::compile_source("nullability.cog", &source(body))
        .unwrap_or_else(|error| panic!("{body}\n{error}"));
    let compiled = bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&compiled, &mut output).unwrap_or_else(|e| panic!("{body}\n{e}"));
    String::from_utf8(output).unwrap()
}

fn accept(body: &str) {
    let source = source(&format!("{body}\nfn main() {{}}"));
    if let Err(error) = project::compile_source("nullability.cog", &source) {
        panic!("{body}\n{error}");
    }
}

fn reject(body: &str, expected: &str) {
    let source = source(&format!("{body}\nfn main() {{}}"));
    let error = match project::compile_source("nullability.cog", &source) {
        Ok(_) => panic!("accepted: {body}"),
        Err(error) => error,
    };
    assert!(error.contains(expected), "{body}\n{error}");
}

#[test]
fn crash_expression_is_a_lazy_never_typed_nil_fallback() {
    let compiled = project::compile_source(
        "nullability.cog",
        &source(
            r#"
            fn main() {
                const value = find(true) ?? crash("missing node");
                io::println(value.value)!!;
            }
        "#,
        ),
    )
    .unwrap();
    let encoded = bytecode::encode(&compiled).unwrap();
    for jit in [false, true] {
        let program = cog::sdk::Engine::new().decode(&encoded).unwrap();
        let mut output = Vec::new();
        program
            .run_in(&mut cog::sdk::RunContext::new(&mut output).with_jit(jit))
            .unwrap();
        assert_eq!(output, b"1\n");
    }

    let failing = source(
        r#"
        fn main() {
            const value = find(false) ?? crash("missing node");
            io::println(value.value)!!;
        }
    "#,
    );
    let program = project::compile_source("nullability.cog", &failing).unwrap();
    let program = cog::sdk::Engine::new()
        .decode(&bytecode::encode(&program).unwrap())
        .unwrap();
    for jit in [false, true] {
        let mut output = Vec::new();
        let error = program
            .run_in(&mut cog::sdk::RunContext::new(&mut output).with_jit(jit))
            .unwrap_err();
        assert!(error.message().contains("missing node"), "{error}");
        assert!(output.is_empty());
    }
}

#[test]
fn if_const_evaluates_nullable_value_once_and_scopes_the_binding() {
    let program = project::compile_source(
        "nullability.cog",
        &source(
            r#"
            use err;
            var calls: int = 0;
            fn next(node: Node) -> Node? { calls += 1; node.next }
            fn failing() -> Node? { err::Error::DivisionByZero() }
            fn read(node: Node) -> int {
                if const item = next(node) { item.value } else { -1 }
            }
            fn main() {
                const root = Node(value: 1, next: Node(value: 7, next: nil));
                if const item = root.next { io::println(item.value)!!; }
                io::println(read(root))!!;
                io::println(read(Node(value: 0, next: nil)))!!;
                io::println(calls)!!;
                try {
                    if const item = failing() { io::println(item.value)!!; }
                    else { io::println("wrong")!!; }
                } catch(error) { io::println(error.error_kind())!!; }
            }
        "#,
        ),
    )
    .unwrap();
    let encoded = bytecode::encode(&program).unwrap();
    for jit in [false, true] {
        let program = cog::sdk::Engine::new().decode(&encoded).unwrap();
        let mut output = Vec::new();
        program
            .run_in(&mut cog::sdk::RunContext::new(&mut output).with_jit(jit))
            .unwrap();
        assert_eq!(output, b"7\n7\n-1\n2\nDivisionByZero\n");
    }
    reject(
        "fn f(node: Node) { if const item = node.next {} item; }",
        "item",
    );
}

#[test]
fn member_use_of_a_nullable_value_is_rejected_with_a_fix() {
    for (body, expected) in [
        (
            "fn f(n: Node?) -> int { n.value }",
            "cannot access field `value` of a value of type `Node?` that may be `nil`",
        ),
        (
            "fn f(n: Node?) { n.value = 2; }",
            "cannot access field `value`",
        ),
        (
            "fn f(n: Node?) { n.value += 2; }",
            "cannot access field `value`",
        ),
        (
            "fn f(n: Node) -> int { n.next.value }",
            "cannot access field `value`",
        ),
        (
            "fn f(n: Node?) -> int { n.double() }",
            "cannot call method `double()` on a value of type `Node?`",
        ),
        (
            "fn f(n: int?) -> str { n.to_str() }",
            "cannot call method `to_str()` on a value of type `int?`",
        ),
        (
            "fn f(xs: int[]?) -> int { xs[0] }",
            "cannot index a value of type `int[]?`",
        ),
        (
            "fn f(xs: int[]?) { xs.append(1); }",
            "cannot call method `append()`",
        ),
        (
            "fn f(xs: int[]?) -> int { len(xs) }",
            "cannot take the length of",
        ),
        (
            "fn f(xs: int[]?) { for x in xs {} }",
            "cannot iterate over a value of type `int[]?`",
        ),
        (
            "fn f(text: str?) -> char { text[0] }",
            "cannot index a value of type `str?`",
        ),
        (
            "fn f(data: bytes?) -> int { data.len() }",
            "cannot call method `len()` on a value of type `bytes?`",
        ),
        (
            "fn f(pair: (int, int)?) -> int { pair.0 }",
            "cannot access position 0",
        ),
        (
            "fn f(g: fn(int) -> int) {} fn h(g: (int, Node)?) { const (a, b) = g; }",
            "requires a tuple, got (int, Node)?",
        ),
        (
            "fn f(n: Node?) -> int { need(n) }",
            "parameter `node` of `need`",
        ),
        (
            "fn f(n: Node?) -> int { need(n) }",
            "check it with `!= nil`",
        ),
        (
            "fn f(n: Node) { const m: Node = n.next; }",
            "declared as Node but given Node?",
        ),
        ("fn f(n: Node?) -> Node { n }", "given Node?"),
        ("fn f(n: Node?) -> Node { return n; }", "given Node?"),
        (
            "fn f(n: Node?) { var m = Node(value: 1, next: nil); m = n; }",
            "given Node?",
        ),
        (
            "fn f(n: Node?) -> Node { Node(value: 1, next: n); n }",
            "given Node?",
        ),
        ("fn f(n: Node) { n.value = nil; }", "given nil"),
        ("fn f() -> Node { nil }", "given nil"),
        (
            "fn f(x: int?) -> int { x + 1 }",
            "invalid operands int? and int",
        ),
        (
            "fn f(flag: bool?) { if flag {} }",
            "condition declared as bool",
        ),
    ] {
        reject(body, expected);
    }
}

#[test]
fn every_type_form_tracks_nil() {
    for ty in [
        "Node",
        "Color",
        "int[]",
        "(int, str)",
        "bytes",
        "fn(int) -> int",
        "str",
        "Node[]",
    ] {
        // A nullable function type is only reachable through a join or `[]?`.
        if ty.starts_with("fn") {
            continue;
        }
        reject(&format!("fn f(v: {ty}?) -> {ty} {{ v }}"), "E2001");
        accept(&format!("fn f(v: {ty}) -> {ty}? {{ v }}"));
        accept(&format!(
            "fn f(v: {ty}?, other: {ty}) -> {ty} {{ v ?? other }}"
        ));
        accept(&format!(
            "fn f(v: {ty}?, other: {ty}) -> {ty} {{ if v == nil {{ other }} else {{ v }} }}"
        ));
    }
    reject(
        "fn f(flag: bool) -> int { const g = if flag { (fn(x: int) -> int { x }) } else { nil }; (g)(1) }",
        "cannot call a value",
    );
    accept(
        "fn f(flag: bool) -> int { const g = if flag { (fn(x: int) -> int { x }) } else { nil }; if g == nil { 0 } else { (g)(1) } }",
    );
}

#[test]
fn joins_with_nil_produce_nullable_types() {
    for body in [
        "fn f(c: bool) -> int { const n = if c { nil } else { Node(value: 1, next: nil) }; n.value }",
        "fn f(c: bool) -> int { const n = if c { Node(value: 1, next: nil) }; n.value }",
        "fn f(c: bool) -> int { const n = match c { _ if c => nil, _ => Node(value: 1, next: nil) }; n.value }",
        "fn f() -> int { const xs = [Node(value: 1, next: nil), nil]; xs[0].value }",
    ] {
        reject(body, "cannot access field `value`");
    }
    // The previously rejected recursive clone idiom.
    accept(
        r#"
        use clone;
        use clone::Clone;
        extend Node with Clone {
            fn clone(self) -> Node {
                const next = self.next;
                Node(value: self.value, next: if next == nil { nil } else { next.clone() })
            }
        }
        "#,
    );
}

#[test]
fn comparisons_with_nil_narrow_bindings() {
    for body in [
        "fn f(n: Node?) -> int { if n != nil { n.value } else { 0 } }",
        "fn f(n: Node?) -> int { if nil != n { n.value } else { 0 } }",
        "fn f(n: Node?) -> int { if n == nil { 0 } else { n.value } }",
        "fn f(n: Node?) -> int { if !(n == nil) { n.value } else { 0 } }",
        "fn f(n: Node?) -> int { if !(n != nil) { return 0; } n.value }",
        "fn f(n: Node?) -> int { if n == nil { return 0; } n.value }",
        "fn f(n: Node?) -> int { if n == nil { crash(\"nil\"); } need(n) }",
        "fn f(n: Node?) -> bool { n != nil && n.value > 0 }",
        "fn f(n: Node?) -> bool { n == nil || n.value > 0 }",
        "fn f(n: Node?) -> int { if n == nil || n.value < 0 { return 0; } n.value }",
        "fn f(a: Node?, b: Node?) -> int { if a != nil && b != nil { a.value + b.value } else { 0 } }",
        "fn f(a: Node?, b: Node?) -> int { if a == nil || b == nil { return 0; } a.value + b.value }",
        "fn f(a: Node?, flag: bool) -> int { if a == nil { return 0; } else if flag { return 1; } a.value }",
        "fn f(n: Node?) { var i = 0; while i < 3 { if n == nil { break; } i += n.value; } }",
        "fn f(n: Node?) { for i in 0..3 { if n == nil { continue; } need(n); } }",
        "fn f(n: Node?) { var cur: Node? = n; while cur != nil { io::println(cur.value); need(cur); cur = cur.next; } }",
        "fn f(n: Node?) -> int { var cur = n; while cur == nil { cur = find(true); } cur.value }",
        "fn f(n: Node?) -> int { var cur = n; cur = find(true); if cur == nil { return 0; } touch(); cur.value }",
        "fn f(n: Node?) -> int { var total = 0; if n != nil { for i in 0..3 { total += n.value; touch(); } } total }",
        "fn f(n: Node?) -> int { const next = if n == nil { return 0; } else { n.next }; if next == nil { 1 } else { next.value } }",
        "fn f(n: Node?) -> int { match n { nil => 0, _ => n.value } }",
        "fn f(n: Node?) -> int { match n { nil => 0, Node => n.value } }",
        "fn f(n: Node?) -> int { match n { Node => n.value, nil => 0 } }",
        "fn f(n: Node?) -> int { match n { nil => { return 0; }, _ => {} }; n.value }",
        "fn f(n: Node?) -> int { when n is Node { return n.value; } 0 }",
        "fn f(n: Node?) -> int { when n is nil { return 0; } n.value }",
        "fn f(n: Node?) -> int { when n is Node if n.value > 0 { return n.value; } 0 }",
        "fn f(n: Node?) -> int { match n { _ if n == nil => 0, _ => n.value } }",
        "fn f(x: int?) -> int { if x == nil { 0 } else { x + 1 } }",
        "fn f(x: str?) -> int { if x != nil { len(x) } else { 0 } }",
        "fn f(n: Node?) -> int { (n ?? Node(value: 3, next: nil)).value }",
        "fn f(n: Node?) -> int { const m = n ?? find(true) ?? Node(value: 0, next: nil); m.value }",
        "fn f(n: Node?) -> int { if n == nil { return 0; } const g = fn[n]() -> int { n.value }; (g)() }",
        "const G: Node? = nil; fn f() -> int { if G == nil { 0 } else { touch(); G.value } }",
        "fn f(n: Node?) -> int { if n == nil { return 0; } defer { touch(); } n.value }",
        "fn f(c: Color?) -> int { match c { nil => 0, Color::Red => 1, Color::Green => 2 } }",
    ] {
        accept(body);
    }
}

#[test]
fn narrowing_is_conservative() {
    for body in [
        // Wrong exit.
        "fn f(n: Node?) -> int { if n == nil { n.value } else { 0 } }",
        "fn f(n: Node?) -> int { if n != nil { } n.value }",
        "fn f(n: Node?) -> int { if n == nil { touch(); } n.value }",
        "fn f(n: Node?, c: bool) -> int { if n == nil && c { return 0; } n.value }",
        "fn f(n: Node?, c: bool) -> bool { n != nil || n.value > 0 }",
        "fn f(n: Node?, c: bool) -> bool { n == nil && n.value > 0 }",
        "fn f(n: Node?, c: bool) -> int { if n != nil || c { n.value } else { 0 } }",
        "fn f(n: Node?, c: bool) -> int { if !(n != nil && c) { n.value } else { 0 } }",
        "fn f(n: Node?) -> bool { (n != nil) == false && n.value > 0 }",
        // Fields and elements can change through aliases.
        "fn f(n: Node) -> int { if n.next == nil { 0 } else { n.next.value } }",
        "fn f(xs: Node?[]) -> int { if xs[0] == nil { 0 } else { xs[0].value } }",
        // Reassignment.
        "fn f(n: Node?) -> int { var m = n; if m == nil { return 0; } m = n; m.value }",
        "fn f(n: Node?) -> int { var m = n; if m != nil { m = nil; m.value } else { 0 } }",
        "fn f(n: Node?, c: bool) -> int { var m = n; if m == nil { return 0; } if c { m = nil; } m.value }",
        "fn f(n: Node?) -> int { var m = n; m != nil && (if true { m = nil; true } else { true }) && m.value > 0; 0 }",
        "fn f(n: Node?, o: Node?) -> int { var m = n; if m == nil { return 0; } o ?? (if true { m = nil; Node(value: 1, next: nil) } else { Node(value: 2, next: nil) }); m.value }",
        "fn f(o: Node?, n: Node?) -> int { var m = n; o ?? { if m == nil { return 0; } m }; m.value }",
        // Loops invalidate what any iteration rebinds.
        "fn f(n: Node?) { var m = n; if m == nil { return; } while m.value > 0 { m = m.next; } }",
        "fn f(n: Node?) { var m = n; if m == nil { return; } for i in 0..3 { need(m); m = nil; } }",
        "fn f(n: Node?) -> int { var m = n; if m == nil { return 0; } while true { m = nil; break; } m.value }",
        "fn f(n: Node?, c: bool) -> int { var m = n; while m == nil { if c { break; } m = find(true); } m.value }",
        "fn f(n: Node?) -> int { var m = n; if m == nil { return 0; } try { m = nil; touch()!!; } catch(e) { return m.value; } 0 }",
        // Mutable captures are rebound by any call.
        "fn f(n: Node?) -> int { var m = n; const clear = fn[var m]() -> nil { m = nil; }; if m == nil { return 0; } touch(); m.value }",
        "fn f(n: Node?) -> int { var m = n; const clear = fn[var m]() -> nil { m = nil; }; if m == nil { return 0; } (clear)(); m.value }",
        "fn f(n: Node?) -> int { var m = n; if m == nil { return 0; } const clear = fn[var m]() -> nil { m = nil; }; (clear)(); m.value }",
        "fn f(n: Node?) -> int { var m = n; const clear = fn[var m]() -> bool { m = nil; true }; if m != nil && (clear)() { m.value } else { 0 } }",
        "fn f(n: Node?) { var m = n; const clear = fn[var m]() -> nil { m = nil; }; while m != nil { (clear)(); need(m); } }",
        // Deferred writes happen at an implicit exit.
        "fn f(n: Node?) -> int { var m = n; if m == nil { return 0; } if true { defer { m = nil; } } m.value }",
        "fn f(n: Node?) { var m = n; if m != nil { defer { need(m); } m = nil; } }",
        // Cleanup that calls code runs when its block is left.
        "var G: Node? = nil; fn clear() { G = nil; } fn f() -> int { if true { defer { clear(); } if G == nil { return 0; } } else { return 0; } G.value }",
        "var G: Node? = nil; fn clear() { G = nil; } fn f() -> int { while true { defer { clear(); } if G == nil { return 0; } break; } G.value }",
        // Mutable globals are rebound by calls.
        "var G: Node? = nil; fn clear() { G = nil; } fn f() -> int { if G == nil { return 0; } clear(); G.value }",
        // Guards and other arms.
        "fn f(n: Node?) -> int { match n { Node if n.value > 0 => 1, _ => n.value } }",
        "fn f(n: Node?, c: bool) -> int { match n { nil if c => 0, _ => n.value } }",
        "fn f(n: Node?) -> int { var m = n; match m { nil => 0, _ if { m = nil; false } => 1, Node => m.value, _ => 2 } }",
    ] {
        reject(body, "may be `nil`");
    }
}

#[test]
fn a_task_switch_discards_global_facts() {
    reject(
        r#"
        var G: Node? = nil;
        @async fn clear() { G = nil; }
        @async fn f() -> int {
            if G == nil { return 0; }
            await yield_now();
            G.value
        }
        "#,
        "may be `nil`",
    );
}

#[test]
fn nullable_matches_need_a_nil_arm() {
    for body in [
        "fn f(n: Node?) { match n { Node => 1 }; }",
        "fn f(c: Color?) { match c { Color::Red => 1, Color::Green => 2 }; }",
    ] {
        reject(body, "the scrutinee may be nil");
    }
    accept("fn f(n: Node) -> int { match n.value { _ => 1 }; match n { Node => 1 } }");
}

#[test]
fn generic_and_container_nullability_is_preserved() {
    accept(
        r#"
        struct Cell[T: type] { value: T, spare: T? }
        fn pick[T: type](value: T?, other: T) -> T { value ?? other }
        fn wrap[T: type](value: T) -> T? { value }
        fn f(n: Node?) -> int {
            const picked = pick(n, Node(value: 2, next: nil));
            const empty: Cell[Node?] = Cell(value: nil, spare: nil);
            const full = Cell(value: 1, spare: nil);
            picked.value + (full.spare ?? 0)
        }
        "#,
    );
    reject(
        "fn wrap[T: type](value: T) -> T? { value } fn f() -> int { wrap(Node(value: 1, next: nil)).value }",
        "cannot access field `value`",
    );
    reject(
        "fn id[T: type](value: T) -> T { value } fn f(n: Node?) -> int { id(n).value }",
        "cannot access field `value`",
    );
    reject(
        "struct Cell[T: type] { value: T } fn g(c: Cell[int?]) {} fn f() { const c = Cell(value: 1); g(c); }",
        "parameter `c` of `g`",
    );
    reject(
        "fn f(xs: Node[]) { const ys: Node?[] = xs; }",
        "declared as Node?[]",
    );
    reject("fn f(xs: Node[]) { xs.append(nil); }", "given nil");
}

#[test]
fn dynamic_values_keep_runtime_checks() {
    accept(
        "fn f(value) { value.value; value.method_free; value[0]; if value == nil { value.x; } }",
    );
}

#[test]
fn nullable_values_run_after_a_bytecode_round_trip() {
    assert_eq!(
        run(r#"
        use clone;
        use clone::Clone;
        struct Cell[T: type] { value: T }
        extend Node with Clone {
            fn clone(self) -> Node {
                const next = self.next;
                Node(value: self.value, next: if next == nil { nil } else { next.clone() })
            }
        }
        fn total(head: Node?) -> int {
            var sum = 0;
            var cur: Node? = head;
            while cur != nil {
                sum += cur.value;
                cur = cur.next;
            }
            sum
        }
        fn show(cell: Cell[int?]) { io::println(cell.value ?? -1); }
        fn main() {
            const list = Node(value: 1, next: Node(value: 2, next: nil));
            io::println(total(list.clone()));
            io::println(total(nil));
            io::println(find(false) == nil);
            const hit = find(true);
            if hit != nil && hit.double() == 2 { io::println("hit"); }
            var maybe: Node?[] = [list, nil];
            maybe.append(find(false));
            var numbers: int?[] = [1, 2];
            numbers.append(nil);
            const grid: (int?, Node?)[] = [(1, nil)];
            io::println(len(maybe) + len(numbers) + len(grid));
            const cell: Cell[int?] = Cell(value: 4);
            show(cell);
            const empty: Cell[int?] = Cell(value: nil);
            show(empty);
        }
        "#),
        "3\n0\ntrue\nhit\n7\n4\n-1\n"
    );
}

#[test]
fn nullable_arrays_preserve_nil_and_array_values_at_typed_boundaries() {
    assert_eq!(
        run(r#"
        struct Holder { items: int[]? }
        var calls = 0;

        fn produce(present: bool) -> int[]? {
            calls += 1;
            if present { [2, 3] } else { nil }
        }
        fn count(items: int[]?) -> int {
            if items == nil { return -1; }
            len(items)
        }
        fn make_error(divisor: int) -> int[]? {
            if 10 / divisor > 0 { [5] } else { nil }
        }
        fn main() {
            const missing: int[]? = produce(false);
            io::println(missing == nil);
            io::println(count(nil));
            io::println(count(produce(true)));
            const holder = Holder(items: produce(false));
            io::println(holder.items == nil);
            const nested: int[]?[] = [nil, [7]];
            io::println(count(nested[0]));
            io::println(count(nested[1]));
            io::println(make_error(0).error_kind());
            io::println(calls);
        }
        "#),
        "true\n-1\n2\ntrue\n-1\n1\nDivisionByZero\n3\n"
    );
}

#[test]
fn nullable_generic_contracts_keep_the_underlying_instance_identity() {
    assert_eq!(
        run(r#"
        struct Box[T: type] { value: T }
        struct Holder { item: Box[int?]? }
        fn relay(item: Box[int?]?) -> Box[int?]? { item }
        fn read(item: Box[int?]?) -> int {
            if item == nil { return -1; }
            item.value ?? -2
        }
        fn main() {
            const present: Box[int?] = Box(value: 7);
            const empty: Box[int?] = Box(value: nil);
            io::println(read(relay(present)));
            io::println(read(relay(nil)));
            io::println(read(relay(empty)));
            const holder = Holder(item: present);
            io::println(read(holder.item));
            const items: Box[int?]?[] = [nil, present];
            io::println(read(items[1]));
        }
        "#),
        "7\n-1\n-2\n7\n7\n"
    );
}

#[test]
fn literals_take_nullable_element_types_from_their_context() {
    assert_eq!(
        run(r#"
        struct Holder { items: int?[] }
        struct Cell[T: type] { value: T }
        extend Holder { fn extra(self, xs: int?[]) -> int { xs.append(nil); len(xs) } }
        fn tail() -> int?[] { [1] }
        fn early() -> Node?[] { return [Node(value: 1, next: nil)]; }
        fn pick[T: type](value: T?, other: T) -> T { value ?? other }
        fn wrap[T: type](value: T) -> T? { value }
        fn count(xs: int?[]) -> int { xs.append(nil); len(xs) }
        fn main() {
            var a: int?[] = [1];
            a.append(nil);
            io::println(len(a));
            var b = tail();
            b.append(nil);
            var c = early();
            c.append(nil);
            var h = Holder(items: [1]);
            h.items.append(nil);
            io::println(len(a) + len(b) + len(c) + len(h.items));
            io::println(count([1]) + h.extra([1, 2]));
            const closure = fn() -> int?[] { [1] };
            io::println(len((closure)()));
            const cell: Cell[int?[]] = Cell(value: [1]);
            io::println(cell.value);
            io::println(pick(nil, 3) + pick(wrap(4), 5));
            io::println(wrap("a") ?? "b");
        }
        "#),
        "2\n8\n5\n1\n[1]\n7\na\n"
    );
}

#[test]
fn equality_with_nil_does_not_require_or_call_eq() {
    assert_eq!(
        run(r#"
        use eq;
        use eq::Eq;
        struct Key { id: int }
        extend Key with Eq[Key] {
            fn equals(self, other: Key) -> bool { io::println("equals"); self.id == other.id }
        }
        fn same(a: Key?, b: Key?) -> bool { a == b }
        fn main() {
            io::println(same(nil, nil));
            io::println(same(Key(id: 1), nil));
            io::println(same(Key(id: 1), Key(id: 1)));
            const key: Key? = Key(id: 2);
            if key != nil { io::println(key.id); }
        }
        "#),
        "true\nfalse\nequals\ntrue\n2\n"
    );
}

#[test]
fn imported_globals_narrow_by_their_resolved_binding() {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("nullable-module-globals");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("store.cog"),
        "@public struct Item { value: int }\n\
         @public const CURRENT: Item? = nil;\n\
         @public fn read() -> int { if CURRENT == nil { 0 } else { CURRENT.value } }\n",
    )
    .unwrap();
    let entry = root.join("main.cog");
    std::fs::write(
        &entry,
        "use store;\n\
         fn main() -> int {\n\
             const current = store::CURRENT;\n\
             if current == nil { store::read() } else { current.value }\n\
         }\n",
    )
    .unwrap();
    let loaded = project::load(&entry, None, false).expect("module project loads");
    let result = project::compile(loaded.program, loaded.diagnostics, &loaded.names);
    std::fs::remove_dir_all(&root).unwrap();
    result.unwrap_or_else(|error| panic!("{error}"));
}

const STORE: &str = r#"
struct S { v: int }
struct Wrap[T: type] { it: T }
var g: S? = nil;
fn clear() -> int { g = nil; 0 }
"#;

#[test]
fn assignment_targets_are_checked_before_the_assigned_value() {
    for body in [
        "fn f() { var a = [5]; g = S(v: 7); if g != nil { a[clear()] = g.v; } }",
        "fn f() { var x: S? = S(v: 7); const reset = fn[var x]() -> int { x = nil; 0 }; var a = [5]; if x != nil { a[(reset)()] = x.v; } }",
        "fn f() { var x: S? = nil; x.v = if true { x = S(v: 1); if x == nil { return; } 2 } else { 0 }; }",
        "fn f() { var x: S? = nil; x.v += if true { x = S(v: 1); if x == nil { return; } 2 } else { 0 }; }",
        "fn f() { var a = [S(v: 1)]; var x: S? = nil; a[if true { x = nil; 0 } else { 0 }] = x; }",
    ] {
        reject(&format!("{STORE}\n{body}"), "nil");
    }
    reject(
        &format!(
            "{STORE}\nfn f() {{ var a: S[] = [S(v: 1)]; g = S(v: 7); if g != nil {{ a[clear()] = g; }} }}"
        ),
        "given S?",
    );
    // The receiver is held before the value runs, so rebinding it is harmless.
    assert_eq!(
        run(&format!(
            r#"{STORE}
            fn main() {{
                var first = S(v: 7);
                var x: S? = first;
                if x != nil {{ x.v = if true {{ x = nil; 2 }} else {{ 0 }}; }}
                io::println(first.v);
                var y: S? = first;
                if y != nil {{ y.v += if true {{ y = nil; 3 }} else {{ 0 }}; }}
                io::println(first.v);
                var a = [0];
                a[if true {{ x = first; 0 }} else {{ 0 }}] = if x != nil {{ x.v }} else {{ -1 }};
                io::println(a[0]);
            }}"#
        )),
        "2\n5\n5\n"
    );
}

#[test]
fn function_type_arguments_are_invariant_in_nullability() {
    for body in [
        "fn f(a: Wrap[fn() -> S]) { const b: Wrap[fn() -> S?] = a; }",
        "fn f(a: Wrap[fn(S?) -> int]) { const b: Wrap[fn(S) -> int] = a; }",
        "fn f(a: Wrap[fn() -> int[]]) { const b: Wrap[fn() -> int?[]] = a; }",
        "fn g(b: Wrap[fn() -> S?]) {} fn f(a: Wrap[fn() -> S]) { g(a); }",
    ] {
        reject(&format!("{STORE}\n{body}"), "E2001");
    }
    accept(&format!(
        "{STORE}\nfn f(a: Wrap[fn() -> S?]) {{ const b: Wrap[fn() -> S?] = a; }} fn h(a: fn() -> S) {{ const b: fn() -> S? = a; }}"
    ));
}

#[test]
fn joins_do_not_widen_the_element_type_of_an_existing_array() {
    for body in [
        "fn f() { var a: S[] = [S(v: 1)]; var b = if a.len() > 0 { a } else { [nil] }; b[0] = nil; }",
        "fn f(c: bool, a: S[]) { const b = match c { _ if c => a, _ => [nil] }; }",
        "fn f(a: S[]?) { const b = a ?? [nil]; }",
        "fn f(a: S[], o: S?[]) { const b = [a, o]; }",
        "fn f(c: bool, a: S[], o: S?[]) { const b = if c { o } else { a }; }",
        "fn f(c: bool, a: S[]) { const b: S?[] = if c { a } else { [nil] }; }",
        "fn f(c: bool, a: S[]) { const b = try { a } catch (e) { [nil] }; }",
        "fn f(c: bool, a: S[], o: S?[]) { const b = if c { (a, 1) } else { (o, 1) }; }",
        "fn f(c: bool, a: Wrap[S], o: Wrap[S?]) { const b = if c { a } else { o }; }",
    ] {
        reject(&format!("{STORE}\n{body}"), "invariant");
    }
    // Unused branch values cannot alias anything.
    accept(&format!(
        "{STORE}\nfn f(c: bool, a: S[], o: S?[]) {{ match c {{ _ if c => a.append(S(v: 1)), _ => o.append(nil) }}; for i in 0..2 {{ if c {{ a }} else {{ o }} }} }}"
    ));
    assert_eq!(
        run(&format!(
            r#"{STORE}
            fn main() {{
                var a: S[] = [S(v: 1)];
                var same = if len(a) > 0 {{ a }} else {{ [] }};
                same.append(S(v: 2));
                var b: S?[] = if len(a) > 0 {{ [S(v: 3)] }} else {{ [nil] }};
                b[0] = nil;
                io::println(len(a));
                io::println(b[0] == nil);
            }}"#
        )),
        "2\ntrue\n"
    );
}

#[test]
fn expected_types_reach_branches_and_arguments() {
    assert_eq!(
        run(&format!(
            r#"{STORE}
            struct Holder {{ item: Wrap[S?], items: int?[] }}
            extend Holder {{ fn take(self, w: Wrap[S?]) -> bool {{ w.it == nil }} }}
            fn unwrap(w: Wrap[S?]) -> int {{ const it = w.it; if it == nil {{ -1 }} else {{ it.v }} }}
            fn count(xs: int?[]) -> int {{ xs.append(nil); len(xs) }}
            fn pick(c: bool) -> int?[] {{ if c {{ [1] }} else {{ [2, 3] }} }}
            fn early(c: bool) -> int?[] {{ return match c {{ _ if c => [1], _ => [] }}; }}
            fn main() {{
                const c = true;
                var a: int?[] = if c {{ [1] }} else {{ [2] }};
                a.append(nil);
                var b: int?[] = match c {{ _ if c => [4], _ => [5] }};
                b.append(nil);
                var d: int?[][] = [if c {{ [1] }} else {{ [nil] }}, [2]];
                d[1].append(nil);
                var t: (int?[], int) = if c {{ ([1], 1) }} else {{ ([2], 2) }};
                t.0.append(nil);
                var p = pick(c);
                p.append(nil);
                var e = early(c);
                e.append(nil);
                io::println(len(a) + len(b) + len(d[1]) + len(t.0) + len(p) + len(e));
                io::println(count(if c {{ [1] }} else {{ [2] }}));
                io::println(unwrap(Wrap(it: S(v: 1))));
                var h = Holder(item: Wrap(it: S(v: 2)), items: if c {{ [1] }} else {{ [2] }});
                h.items.append(nil);
                io::println(h.take(Wrap(it: S(v: 3))));
                const f = fn(w: Wrap[S?]) -> int {{ unwrap(w) }};
                io::println((f)(Wrap(it: S(v: 4))));
                var ws: Wrap[S?][] = [];
                ws.append(Wrap(it: S(v: 5)));
                io::println(unwrap(ws[0]) + unwrap(if c {{ Wrap(it: S(v: 1)) }} else {{ Wrap(it: nil) }}));
            }}"#
        )),
        "12\n2\n1\nfalse\n4\n6\n"
    );
}

#[test]
fn expected_types_reach_closure_results_and_skipped_fallbacks() {
    assert_eq!(
        run(r#"
        fn main() {
            const c = true;
            const make = fn[c]() -> int?[] { if c { [1] } else { [2] } };
            const made: int?[] = (make)();
            io::println(made);
        }
        "#),
        "[1]\n"
    );
    // A fallback that can never run is not typed against the destination.
    accept("fn f(x: int?[]) { var y: int?[] = [1]; y = x ?? y; const z: int?[] = y ?? (1, 2); }");
}

#[test]
fn nil_cannot_reach_indexes_or_native_parameters() {
    for body in [
        "fn f(a: int[], p: int?) { a[p]; }",
        "fn f(a: int[], p: int?) { a[p] = 1; }",
        "fn f(a: int[], p: int?) { a[p] += 1; }",
        "fn f(a: str, p: int?) { a[p]; }",
        "fn f(a, p: int?) { a[p]; }",
        "fn f(a: int[]) { a[nil]; }",
        "use str; fn f(p: str?) { str::to_int(p); }",
        "use str; fn f(p: str?) { str::to_uint(p); }",
        "use str; fn f(p: str?) { str::to_float(p); }",
        "use str; fn f(p: str?) { str::to_bool(p); }",
        "use str; fn f(p: str?) { str::to_char(p); }",
        "use str; fn f(p: str?) { str::to_bigint(p); }",
        "use str::*; fn f(p: str?) { to_int(p); }",
        "use hash; fn f(p: int?) { hash::of(p); }",
        "use bytes; fn f(p: str?) { bytes::from_str(p); }",
        "use bytes; fn f(p: bytes?) { bytes::len(p); }",
        "use bytes::*; fn f(p: bytes?) { len(p); copy(p); }",
        "fn f(p: int?) { for i in 0..p {} }",
    ] {
        reject(body, "nil");
    }
    accept(
        r#"
        use str;
        use json;
        use bytes;
        fn f(p: str?, n: int?) {
            str::to_str(p);
            json::stringify(n);
            bytes::render(p);
            exclusive(p);
            if p != nil { str::to_int(p); }
        }
        "#,
    );
}

#[test]
fn nullable_function_types_can_be_written() {
    assert_eq!(
        run(r#"
        struct Hooks { before: (fn() -> int)?, all: (fn() -> int)?[] }
        struct Cell[T: type] { value: T }
        fn call(h: (fn() -> int)?) -> int { if h == nil { -1 } else { (h)() } }
        fn one() -> int { 1 }
        fn main() {
            const h: (fn() -> int)? = nil;
            io::println(call(h));
            io::println(call(one));
            const hooks = Hooks(before: one, all: [one, nil]);
            io::println(call(hooks.before) + call(hooks.all[1]));
            const list: (fn() -> int)[] = [one];
            const cell: Cell[(fn() -> int)[]] = Cell(value: list);
            io::println((cell.value[0])());
            const grouped: (int)? = (2);
            io::println(grouped ?? 0);
        }
        "#),
        "-1\n1\n0\n1\n2\n"
    );
    reject(
        "fn f(g: (fn() -> int)?) { const h: fn() -> int = g; }",
        "given (fn() -> int)?",
    );
    reject(
        "fn f(g: (fn() -> int)[]) { const h: int = g; }",
        "given (fn() -> int)[]",
    );
}

#[test]
fn diagnostic_help_spells_globals_by_their_source_names() {
    for body in [
        "struct S { v: int } var current: S? = nil; fn f() -> int { current.v }",
        "struct S { v: int } const current: S? = nil; fn need_s(s: S) {} fn f() { need_s(current); }",
        "var current: int? = nil; fn f() -> int { var a = [1]; a[current] }",
    ] {
        let source = source(&format!("{body}\nfn main() {{}}"));
        let Err(error) = project::compile_source("nullability.cog", &source) else {
            panic!("accepted: {body}");
        };
        assert!(!error.contains("$symbol"), "{error}");
        assert!(error.contains("current"), "{error}");
    }
}
